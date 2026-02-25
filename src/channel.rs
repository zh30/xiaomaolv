use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::domain::{IncomingMessage, ReplyTarget};
use crate::mcp_commands::{
    discover_mcp_registry, execute_mcp_command, mcp_help_text, parse_telegram_mcp_command,
};
use crate::provider::StreamSink;
use crate::service::MessageService;

pub use crate::config::ChannelPluginConfig;

#[derive(Debug, Deserialize)]
pub struct TelegramUpdate {
    pub message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: TelegramChat,
    pub from: Option<TelegramUser>,
    pub text: Option<String>,
    pub message_thread_id: Option<i64>,
    pub reply_to_message: Option<TelegramReplyMessage>,
    pub entities: Option<Vec<TelegramMessageEntity>>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
    pub is_bot: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramReplyMessage {
    pub message_id: i64,
    pub from: Option<TelegramUser>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramMessageEntity {
    #[serde(rename = "type")]
    pub kind: String,
    pub offset: i64,
    pub length: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramPollUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse<T> {
    ok: bool,
    result: T,
    description: Option<String>,
    parameters: Option<TelegramApiErrorParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramApiErrorParameters {
    retry_after: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TelegramSentMessage {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramBotProfile {
    username: Option<String>,
    can_read_all_group_messages: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct TelegramPollingDiagSnapshot {
    worker_started_at_unix: Option<u64>,
    last_poll_ok_at_unix: Option<u64>,
    last_poll_error_at_unix: Option<u64>,
    last_poll_error: Option<String>,
    last_batch_size: usize,
    total_poll_ok: u64,
    total_poll_err: u64,
    consecutive_poll_err: u64,
    total_updates_received: u64,
}

#[derive(Debug, Default)]
struct TelegramPollingDiagState {
    worker_started_at_unix: Option<u64>,
    last_poll_ok_at_unix: Option<u64>,
    last_poll_error_at_unix: Option<u64>,
    last_poll_error: Option<String>,
    last_batch_size: usize,
    total_poll_ok: u64,
    total_poll_err: u64,
    consecutive_poll_err: u64,
    total_updates_received: u64,
}

impl TelegramPollingDiagState {
    fn snapshot(&self) -> TelegramPollingDiagSnapshot {
        TelegramPollingDiagSnapshot {
            worker_started_at_unix: self.worker_started_at_unix,
            last_poll_ok_at_unix: self.last_poll_ok_at_unix,
            last_poll_error_at_unix: self.last_poll_error_at_unix,
            last_poll_error: self.last_poll_error.clone(),
            last_batch_size: self.last_batch_size,
            total_poll_ok: self.total_poll_ok,
            total_poll_err: self.total_poll_err,
            consecutive_poll_err: self.consecutive_poll_err,
            total_updates_received: self.total_updates_received,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramRenderedText {
    text: String,
    parse_mode: Option<&'static str>,
}

const TELEGRAM_MAX_TEXT_CHARS: usize = 4096;

#[derive(Clone)]
pub struct TelegramSender {
    client: Client,
    bot_token: String,
}

impl TelegramSender {
    pub fn new(bot_token: String) -> Self {
        Self {
            client: Client::new(),
            bot_token,
        }
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
        text: &str,
    ) -> anyhow::Result<()> {
        let parts = render_telegram_text_parts(text, TELEGRAM_MAX_TEXT_CHARS);
        if parts.is_empty() {
            return Ok(());
        }
        for (idx, part) in parts.iter().enumerate() {
            let part_reply_to = if idx == 0 { reply_to_message_id } else { None };
            let _ = self
                .send_rendered_message_with_id(chat_id, message_thread_id, part_reply_to, part)
                .await?;
        }
        Ok(())
    }

    pub async fn send_message_with_id(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
        text: &str,
    ) -> anyhow::Result<i64> {
        let parts = render_telegram_text_parts(text, TELEGRAM_MAX_TEXT_CHARS);
        let first = parts.first().context("empty telegram message text")?;
        self.send_rendered_message_with_id(chat_id, message_thread_id, reply_to_message_id, first)
            .await
    }

    async fn send_rendered_message_with_id(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
        rendered: &TelegramRenderedText,
    ) -> anyhow::Result<i64> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let mut payload = serde_json::json!({ "chat_id": chat_id, "text": rendered.text });
        if let Some(thread_id) = message_thread_id {
            payload["message_thread_id"] = serde_json::json!(thread_id);
        }
        if let Some(reply_to) = reply_to_message_id {
            payload["reply_parameters"] = serde_json::json!({
                "message_id": reply_to,
                "allow_sending_without_reply": true
            });
        }
        if let Some(mode) = rendered.parse_mode {
            payload["parse_mode"] = serde_json::json!(mode);
        }

        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("failed to call telegram sendMessage")?
            .error_for_status()
            .context("telegram sendMessage returned error")?;

        let body: TelegramApiResponse<TelegramSentMessage> = response
            .json()
            .await
            .context("failed to decode telegram sendMessage payload")?;

        if !body.ok {
            bail!(
                "telegram sendMessage returned ok=false: {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        Ok(body.result.message_id)
    }

    pub async fn send_message_draft(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
        draft_message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let parts = render_telegram_text_parts(text, TELEGRAM_MAX_TEXT_CHARS);
        let first = parts
            .first()
            .context("empty telegram message text for draft")?;

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessageDraft",
            self.bot_token
        );
        let mut payload = serde_json::json!({
            "chat_id": chat_id,
            "draft_message_id": draft_message_id,
            "text": first.text
        });
        if let Some(thread_id) = message_thread_id {
            payload["message_thread_id"] = serde_json::json!(thread_id);
        }
        if let Some(reply_to) = reply_to_message_id {
            payload["reply_parameters"] = serde_json::json!({
                "message_id": reply_to,
                "allow_sending_without_reply": true
            });
        }
        if let Some(mode) = first.parse_mode {
            payload["parse_mode"] = serde_json::json!(mode);
        }

        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("failed to call telegram sendMessageDraft")?
            .error_for_status()
            .context("telegram sendMessageDraft returned error")?;

        let body: TelegramApiResponse<bool> = response
            .json()
            .await
            .context("failed to decode telegram sendMessageDraft payload")?;

        if !body.ok {
            bail!(
                "telegram sendMessageDraft returned ok=false: {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        Ok(())
    }

    pub async fn send_chat_action_typing(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
    ) -> anyhow::Result<()> {
        let url = format!(
            "https://api.telegram.org/bot{}/sendChatAction",
            self.bot_token
        );
        let payload = typing_action_payload(chat_id, message_thread_id);

        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("failed to call telegram sendChatAction")?
            .error_for_status()
            .context("telegram sendChatAction returned error")?;

        let body: TelegramApiResponse<bool> = response
            .json()
            .await
            .context("failed to decode telegram sendChatAction payload")?;

        if !body.ok || !body.result {
            bail!(
                "telegram sendChatAction returned ok=false: {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        Ok(())
    }

    pub async fn set_my_short_description(&self, short_description: &str) -> anyhow::Result<()> {
        let url = format!(
            "https://api.telegram.org/bot{}/setMyShortDescription",
            self.bot_token
        );
        let payload = short_description_payload(short_description);
        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("failed to call telegram setMyShortDescription")?
            .error_for_status()
            .context("telegram setMyShortDescription returned error")?;

        let body: TelegramApiResponse<bool> = response
            .json()
            .await
            .context("failed to decode telegram setMyShortDescription payload")?;

        if !body.ok || !body.result {
            bail!(
                "telegram setMyShortDescription returned ok=false: {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        Ok(())
    }

    pub async fn set_my_commands(
        &self,
        commands: &[(&str, &str)],
        all_private_chats: bool,
    ) -> anyhow::Result<()> {
        let url = format!(
            "https://api.telegram.org/bot{}/setMyCommands",
            self.bot_token
        );
        let mut payload = serde_json::json!({
            "commands": commands
                .iter()
                .map(|(command, description)| serde_json::json!({
                    "command": command,
                    "description": description
                }))
                .collect::<Vec<_>>()
        });
        if all_private_chats {
            payload["scope"] = serde_json::json!({ "type": "all_private_chats" });
        }

        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("failed to call telegram setMyCommands")?
            .error_for_status()
            .context("telegram setMyCommands returned error")?;

        let body: TelegramApiResponse<bool> = response
            .json()
            .await
            .context("failed to decode telegram setMyCommands payload")?;

        if !body.ok || !body.result {
            bail!(
                "telegram setMyCommands returned ok=false: {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        Ok(())
    }

    pub async fn get_me_username(&self) -> anyhow::Result<Option<String>> {
        let profile = self.get_me_profile().await?;
        Ok(profile
            .username
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()))
    }

    async fn get_me_profile(&self) -> anyhow::Result<TelegramBotProfile> {
        let url = format!("https://api.telegram.org/bot{}/getMe", self.bot_token);
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("failed to call telegram getMe")?
            .error_for_status()
            .context("telegram getMe returned error")?;

        let body: TelegramApiResponse<TelegramBotProfile> = response
            .json()
            .await
            .context("failed to decode telegram getMe payload")?;

        if !body.ok {
            bail!(
                "telegram getMe returned ok=false: {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        Ok(body.result)
    }

    async fn get_webhook_info(&self) -> anyhow::Result<Value> {
        let url = format!(
            "https://api.telegram.org/bot{}/getWebhookInfo",
            self.bot_token
        );
        let response = self
            .client
            .post(url)
            .send()
            .await
            .context("failed to call telegram getWebhookInfo")?
            .error_for_status()
            .context("telegram getWebhookInfo returned error")?;

        let body: TelegramApiResponse<Value> = response
            .json()
            .await
            .context("failed to decode telegram getWebhookInfo payload")?;

        if !body.ok {
            bail!(
                "telegram getWebhookInfo returned ok=false: {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        Ok(body.result)
    }

    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> anyhow::Result<()> {
        let parts = render_telegram_text_parts(text, TELEGRAM_MAX_TEXT_CHARS);
        let first = parts
            .first()
            .context("empty telegram message text for edit")?;

        let rendered = TelegramRenderedText {
            text: first.text.clone(),
            parse_mode: first.parse_mode,
        };
        self.edit_rendered_message(chat_id, message_id, &rendered)
            .await
    }

    async fn edit_rendered_message(
        &self,
        chat_id: i64,
        message_id: i64,
        rendered: &TelegramRenderedText,
    ) -> anyhow::Result<()> {
        let url = format!(
            "https://api.telegram.org/bot{}/editMessageText",
            self.bot_token
        );
        let mut payload = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": rendered.text
        });
        if let Some(mode) = rendered.parse_mode {
            payload["parse_mode"] = serde_json::json!(mode);
        }

        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("failed to call telegram editMessageText")?
            .error_for_status()
            .context("telegram editMessageText returned error")?;

        let body: TelegramApiResponse<Value> = response
            .json()
            .await
            .context("failed to decode telegram editMessageText payload")?;

        if !body.ok {
            bail!(
                "telegram editMessageText returned ok=false: {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct ChannelContext {
    pub service: Arc<MessageService>,
    pub channel_name: String,
}

#[derive(Clone)]
pub struct ChannelRuntimeContext {
    pub service: Arc<MessageService>,
    pub channel_name: String,
    pub shutdown: watch::Receiver<bool>,
}

pub struct ChannelWorker {
    pub name: String,
    pub task: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub struct ChannelInbound {
    pub payload: Value,
    pub path_secret: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChannelResponse {
    pub body: Value,
}

impl ChannelResponse {
    pub fn json(body: Value) -> Self {
        Self { body }
    }
}

#[derive(Debug)]
pub enum ChannelPluginError {
    BadRequest(String),
    Unauthorized(String),
    Internal(anyhow::Error),
}

impl ChannelPluginError {
    pub fn internal(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}

impl std::fmt::Display for ChannelPluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Self::Unauthorized(msg) => write!(f, "unauthorized: {msg}"),
            Self::Internal(err) => write!(f, "internal error: {err}"),
        }
    }
}

impl std::error::Error for ChannelPluginError {}

#[async_trait]
pub trait ChannelPlugin: Send + Sync {
    async fn handle_inbound(
        &self,
        ctx: ChannelContext,
        inbound: ChannelInbound,
    ) -> Result<ChannelResponse, ChannelPluginError>;

    async fn start_background(
        &self,
        _ctx: ChannelRuntimeContext,
    ) -> anyhow::Result<Option<ChannelWorker>> {
        Ok(None)
    }

    fn mode(&self) -> Option<&'static str> {
        None
    }

    async fn diagnostics(&self) -> anyhow::Result<Option<Value>> {
        Ok(None)
    }
}

pub trait ChannelFactory: Send + Sync {
    fn kind(&self) -> &'static str;
    fn create(
        &self,
        name: &str,
        config: &ChannelPluginConfig,
    ) -> anyhow::Result<Arc<dyn ChannelPlugin>>;
}

#[derive(Clone, Default)]
pub struct ChannelRegistry {
    factories: HashMap<String, Arc<dyn ChannelFactory>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry
            .register(Arc::new(TelegramChannelFactory))
            .expect("failed to register default channel factory");
        registry
    }

    pub fn register(&mut self, factory: Arc<dyn ChannelFactory>) -> anyhow::Result<()> {
        let kind = factory.kind().to_string();
        if self.factories.contains_key(&kind) {
            bail!("channel factory '{kind}' already registered");
        }
        self.factories.insert(kind, factory);
        Ok(())
    }

    pub fn build(
        &self,
        name: &str,
        config: &ChannelPluginConfig,
    ) -> anyhow::Result<Arc<dyn ChannelPlugin>> {
        let factory = self.factories.get(&config.kind).with_context(|| {
            format!(
                "channel kind '{}' is not registered for channel '{}'.",
                config.kind, name
            )
        })?;
        factory.create(name, config)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TelegramIngressMode {
    Polling,
    Webhook,
}

impl TelegramIngressMode {
    fn resolve(mode: Option<&str>, has_webhook_secret: bool) -> anyhow::Result<Self> {
        let raw = mode.unwrap_or("polling").trim().to_ascii_lowercase();
        match raw.as_str() {
            "polling" => Ok(Self::Polling),
            "webhook" => Ok(Self::Webhook),
            "auto" => {
                if has_webhook_secret {
                    Ok(Self::Webhook)
                } else {
                    Ok(Self::Polling)
                }
            }
            other => bail!("unsupported telegram mode '{other}', expected polling|webhook|auto"),
        }
    }
}

pub struct TelegramChannelFactory;

impl ChannelFactory for TelegramChannelFactory {
    fn kind(&self) -> &'static str {
        "telegram"
    }

    fn create(
        &self,
        _name: &str,
        config: &ChannelPluginConfig,
    ) -> anyhow::Result<Arc<dyn ChannelPlugin>> {
        let bot_token = config
            .settings
            .get("bot_token")
            .cloned()
            .context("telegram channel requires settings.bot_token")?;

        let webhook_secret = config
            .settings
            .get("webhook_secret")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let group_mention_bot_username = config
            .settings
            .get("bot_username")
            .map(|value| value.trim().trim_start_matches('@').to_string())
            .filter(|value| !value.is_empty());

        let mode = TelegramIngressMode::resolve(
            config.settings.get("mode").map(String::as_str),
            webhook_secret.is_some(),
        )?;

        if matches!(mode, TelegramIngressMode::Webhook) && webhook_secret.is_none() {
            bail!("telegram webhook mode requires settings.webhook_secret");
        }

        let polling_timeout_secs = config
            .settings
            .get("polling_timeout_secs")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(30)
            .clamp(1, 50);
        let streaming_enabled = config
            .settings
            .get("streaming_enabled")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(true);
        let streaming_edit_interval_ms = config
            .settings
            .get("streaming_edit_interval_ms")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(900)
            .clamp(200, 5_000);
        let streaming_prefer_draft = config
            .settings
            .get("streaming_prefer_draft")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(true);
        let startup_online_enabled = config
            .settings
            .get("startup_online_enabled")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(false);
        let startup_online_text = config
            .settings
            .get("startup_online_text")
            .map(|raw| raw.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "online".to_string());
        let commands_enabled = config
            .settings
            .get("commands_enabled")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(true);
        let commands_auto_register = config
            .settings
            .get("commands_auto_register")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(true);
        let commands_private_only = config
            .settings
            .get("commands_private_only")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(true);
        let admin_user_ids = parse_admin_user_ids(
            config
                .settings
                .get("admin_user_ids")
                .map(String::as_str)
                .unwrap_or_default(),
        )
        .context("failed to parse telegram settings.admin_user_ids")?;

        Ok(Arc::new(TelegramChannelPlugin {
            sender: TelegramSender::new(bot_token.clone()),
            bot_token,
            webhook_secret,
            mode,
            polling_timeout_secs,
            streaming_enabled,
            streaming_edit_interval_ms,
            streaming_prefer_draft,
            startup_online_enabled,
            startup_online_text,
            commands_enabled,
            commands_auto_register,
            commands_private_only,
            admin_user_ids,
            group_mention_bot_username_cache: Arc::new(tokio::sync::Mutex::new(
                group_mention_bot_username,
            )),
            polling_diag_state: Arc::new(tokio::sync::Mutex::new(
                TelegramPollingDiagState::default(),
            )),
        }))
    }
}

struct TelegramChannelPlugin {
    sender: TelegramSender,
    bot_token: String,
    webhook_secret: Option<String>,
    mode: TelegramIngressMode,
    polling_timeout_secs: u64,
    streaming_enabled: bool,
    streaming_edit_interval_ms: u64,
    streaming_prefer_draft: bool,
    startup_online_enabled: bool,
    startup_online_text: String,
    commands_enabled: bool,
    commands_auto_register: bool,
    commands_private_only: bool,
    admin_user_ids: Vec<i64>,
    group_mention_bot_username_cache: Arc<tokio::sync::Mutex<Option<String>>>,
    polling_diag_state: Arc<tokio::sync::Mutex<TelegramPollingDiagState>>,
}

#[derive(Clone)]
struct TelegramCommandSettings {
    enabled: bool,
    private_only: bool,
    admin_user_ids: Vec<i64>,
}

impl TelegramChannelPlugin {
    async fn resolve_group_mention_bot_username(&self) -> Option<String> {
        resolve_group_mention_bot_username_with_cache(
            &self.sender,
            &self.group_mention_bot_username_cache,
        )
        .await
    }

    fn command_settings(&self) -> TelegramCommandSettings {
        TelegramCommandSettings {
            enabled: self.commands_enabled,
            private_only: self.commands_private_only,
            admin_user_ids: self.admin_user_ids.clone(),
        }
    }
}

async fn resolve_group_mention_bot_username_with_cache(
    sender: &TelegramSender,
    cache: &Arc<tokio::sync::Mutex<Option<String>>>,
) -> Option<String> {
    {
        let cached = cache.lock().await;
        if let Some(username) = cached.as_ref() {
            return Some(username.clone());
        }
    }

    match sender.get_me_username().await {
        Ok(Some(username)) => {
            let mut cached = cache.lock().await;
            *cached = Some(username.clone());
            Some(username)
        }
        Ok(None) => {
            warn!("telegram getMe has no username, group mention filter disabled");
            None
        }
        Err(err) => {
            warn!(
                error = %format!("{err:#}"),
                "failed to fetch telegram bot username for group mention filter"
            );
            None
        }
    }
}

#[async_trait]
impl ChannelPlugin for TelegramChannelPlugin {
    async fn handle_inbound(
        &self,
        ctx: ChannelContext,
        inbound: ChannelInbound,
    ) -> Result<ChannelResponse, ChannelPluginError> {
        if !matches!(self.mode, TelegramIngressMode::Webhook) {
            return Err(ChannelPluginError::BadRequest(
                "telegram webhook endpoint is disabled because mode is polling".to_string(),
            ));
        }

        let expected_secret = self.webhook_secret.as_ref().ok_or_else(|| {
            ChannelPluginError::BadRequest("telegram webhook secret is not configured".to_string())
        })?;

        let incoming_secret = inbound.path_secret.as_deref().unwrap_or("");
        if incoming_secret != expected_secret {
            return Err(ChannelPluginError::Unauthorized(
                "invalid webhook secret".to_string(),
            ));
        }

        let update: TelegramUpdate = serde_json::from_value(inbound.payload).map_err(|err| {
            ChannelPluginError::BadRequest(format!("invalid telegram payload: {err}"))
        })?;

        process_telegram_update(
            ctx,
            &self.sender,
            update,
            self.streaming_enabled,
            self.streaming_edit_interval_ms,
            self.streaming_prefer_draft,
            self.resolve_group_mention_bot_username().await,
            self.command_settings(),
        )
        .await
        .map_err(ChannelPluginError::internal)?;

        Ok(ChannelResponse::json(serde_json::json!({ "ok": true })))
    }

    async fn start_background(
        &self,
        ctx: ChannelRuntimeContext,
    ) -> anyhow::Result<Option<ChannelWorker>> {
        match self.sender.get_me_profile().await {
            Ok(profile) => {
                if let Some(username) = profile
                    .username
                    .map(|v| v.trim().trim_start_matches('@').to_string())
                    .filter(|v| !v.is_empty())
                {
                    let mut cached = self.group_mention_bot_username_cache.lock().await;
                    if cached.is_none() {
                        *cached = Some(username);
                    }
                }

                if profile.can_read_all_group_messages == Some(false) {
                    warn!(
                        channel = %ctx.channel_name,
                        "telegram privacy mode appears enabled (can_read_all_group_messages=false). \
                    for group @mention workflows, disable privacy in @BotFather: /setprivacy -> select bot -> Disable"
                    );
                }
            }
            Err(err) => {
                warn!(
                    channel = %ctx.channel_name,
                    error = %format!("{err:#}"),
                    "failed to read telegram bot profile on startup"
                );
            }
        }

        if self.startup_online_enabled {
            let startup_text = truncate_chars(self.startup_online_text.trim(), 120);
            if let Err(err) = self.sender.set_my_short_description(&startup_text).await {
                warn!(
                    channel = %ctx.channel_name,
                    error = %format!("{err:#}"),
                    "failed to set telegram startup online status"
                );
            } else {
                info!(
                    channel = %ctx.channel_name,
                    status = %startup_text,
                    "telegram startup online status set"
                );
            }
        }

        if self.commands_enabled && self.commands_auto_register {
            let commands = telegram_registered_commands();
            if let Err(err) = self
                .sender
                .set_my_commands(&commands, self.commands_private_only)
                .await
            {
                warn!(
                    channel = %ctx.channel_name,
                    error = %format!("{err:#}"),
                    "failed to register telegram bot commands"
                );
            } else {
                info!(
                    channel = %ctx.channel_name,
                    private_only = self.commands_private_only,
                    "telegram bot commands registered"
                );
            }
        }

        if !matches!(self.mode, TelegramIngressMode::Polling) {
            return Ok(None);
        }

        let channel_name = ctx.channel_name.clone();
        let service = ctx.service.clone();
        let sender = self.sender.clone();
        let bot_token = self.bot_token.clone();
        let mut shutdown = ctx.shutdown.clone();
        let timeout_secs = self.polling_timeout_secs;
        let streaming_enabled = self.streaming_enabled;
        let streaming_edit_interval_ms = self.streaming_edit_interval_ms;
        let streaming_prefer_draft = self.streaming_prefer_draft;
        let command_settings = self.command_settings();
        let group_mention_bot_username_cache = self.group_mention_bot_username_cache.clone();
        let polling_diag_state = self.polling_diag_state.clone();
        let worker_name = format!("{channel_name}-polling");

        let task = tokio::spawn(async move {
            info!(channel = %channel_name, "telegram polling worker started");
            let client = Client::new();
            let mut offset: i64 = 0;
            {
                let mut diag = polling_diag_state.lock().await;
                diag.worker_started_at_unix = Some(current_unix_timestamp());
            }

            if let Err(err) = delete_telegram_webhook(&client, &bot_token).await {
                warn!(channel = %channel_name, error = %err, "failed to delete webhook before polling");
            }

            loop {
                if *shutdown.borrow() {
                    break;
                }

                let updates_result = tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                        continue;
                    }
                    result = fetch_telegram_updates(&client, &bot_token, offset, timeout_secs) => result,
                };

                match updates_result {
                    Ok(updates) => {
                        let batch_size = updates.len();
                        {
                            let mut diag = polling_diag_state.lock().await;
                            diag.last_poll_ok_at_unix = Some(current_unix_timestamp());
                            diag.last_batch_size = batch_size;
                            diag.total_poll_ok += 1;
                            diag.total_updates_received += batch_size as u64;
                            diag.consecutive_poll_err = 0;
                            diag.last_poll_error = None;
                        }
                        for update in updates {
                            offset = offset.max(update.update_id + 1);

                            let payload = TelegramUpdate {
                                message: update.message,
                            };
                            let group_mention_bot_username =
                                resolve_group_mention_bot_username_with_cache(
                                    &sender,
                                    &group_mention_bot_username_cache,
                                )
                                .await;

                            if let Err(err) = process_telegram_update(
                                ChannelContext {
                                    service: service.clone(),
                                    channel_name: channel_name.clone(),
                                },
                                &sender,
                                payload,
                                streaming_enabled,
                                streaming_edit_interval_ms,
                                streaming_prefer_draft,
                                group_mention_bot_username.clone(),
                                command_settings.clone(),
                            )
                            .await
                            {
                                warn!(
                                    channel = %channel_name,
                                    error = %format!("{err:#}"),
                                    "failed to process telegram update in polling mode"
                                );
                            }
                        }
                    }
                    Err(err) => {
                        let err_text = format!("{err:#}");
                        {
                            let mut diag = polling_diag_state.lock().await;
                            diag.last_poll_error_at_unix = Some(current_unix_timestamp());
                            diag.last_poll_error = Some(err_text.clone());
                            diag.total_poll_err += 1;
                            diag.consecutive_poll_err += 1;
                        }
                        warn!(
                            channel = %channel_name,
                            error = %err_text,
                            "telegram polling request failed"
                        );
                        if err_text.contains("terminated by other getUpdates request")
                            || err_text
                                .contains("can't use getUpdates method while webhook is active")
                        {
                            warn!(
                                channel = %channel_name,
                                "telegram polling conflict detected: ensure only one polling instance is running and webhook is disabled"
                            );
                        }
                        if wait_for_shutdown_or_timeout(&mut shutdown, Duration::from_secs(1)).await
                        {
                            break;
                        }
                    }
                }
            }

            info!(channel = %channel_name, "telegram polling worker stopped");
        });

        Ok(Some(ChannelWorker {
            name: worker_name,
            task,
        }))
    }

    async fn diagnostics(&self) -> anyhow::Result<Option<Value>> {
        let cached_username = {
            let cached = self.group_mention_bot_username_cache.lock().await;
            cached.clone()
        };
        let polling = {
            let diag = self.polling_diag_state.lock().await;
            diag.snapshot()
        };

        let get_me = match self.sender.get_me_profile().await {
            Ok(profile) => serde_json::json!({
                "ok": true,
                "username": profile.username,
                "can_read_all_group_messages": profile.can_read_all_group_messages,
            }),
            Err(err) => serde_json::json!({
                "ok": false,
                "error": format!("{err:#}")
            }),
        };

        let get_webhook_info = match self.sender.get_webhook_info().await {
            Ok(result) => serde_json::json!({
                "ok": true,
                "result": result
            }),
            Err(err) => serde_json::json!({
                "ok": false,
                "error": format!("{err:#}")
            }),
        };

        Ok(Some(serde_json::json!({
            "channel": "telegram",
            "mode": self.mode(),
            "group_mention_bot_username_cached": cached_username,
            "commands": {
                "enabled": self.commands_enabled,
                "auto_register": self.commands_auto_register,
                "private_only": self.commands_private_only,
                "admin_user_ids_count": self.admin_user_ids.len()
            },
            "polling": polling,
            "get_me": get_me,
            "get_webhook_info": get_webhook_info
        })))
    }

    fn mode(&self) -> Option<&'static str> {
        Some(match self.mode {
            TelegramIngressMode::Polling => "polling",
            TelegramIngressMode::Webhook => "webhook",
        })
    }
}

async fn process_telegram_update(
    ctx: ChannelContext,
    sender: &TelegramSender,
    update: TelegramUpdate,
    streaming_enabled: bool,
    streaming_edit_interval_ms: u64,
    streaming_prefer_draft: bool,
    group_mention_bot_username: Option<String>,
    command_settings: TelegramCommandSettings,
) -> anyhow::Result<()> {
    if let Some(message) = update.message
        && let Some(ref text) = message.text
    {
        let chat_id = message.chat.id;
        let message_id = message.message_id;
        let chat_kind = message
            .chat
            .kind
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let message_thread_id = message.message_thread_id;
        let reply_to_message_id = message.reply_to_message.as_ref().map(|m| m.message_id);
        let replied_to_bot = is_reply_to_bot_message(message.reply_to_message.as_ref());
        let is_group = is_group_chat_type(message.chat.kind.as_deref());
        let from_is_bot = message
            .from
            .as_ref()
            .and_then(|u| u.is_bot)
            .unwrap_or(false);
        let mention_entity_count = message
            .entities
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .filter(|e| e.kind == "mention" || e.kind == "text_mention")
                    .count()
            })
            .unwrap_or(0);
        let text_preview = truncate_chars(text.trim(), 120).replace('\n', " ");
        info!(
            chat_id,
            message_id,
            chat_type = %chat_kind,
            is_group,
            from_is_bot,
            message_thread_id = ?message_thread_id,
            reply_to_message_id = ?reply_to_message_id,
            replied_to_bot,
            mention_entity_count,
            text_len = text.chars().count(),
            text_preview = %text_preview,
            "telegram inbound message"
        );
        if is_group && from_is_bot {
            debug!(
                chat_id,
                message_id = message.message_id,
                "skip telegram group message from bot account"
            );
            info!(
                chat_id,
                message_id, "telegram group message skipped: sender is bot"
            );
            return Ok(());
        }

        if maybe_handle_telegram_command(
            &ctx,
            sender,
            &message,
            text.trim(),
            group_mention_bot_username.as_deref(),
            &command_settings,
        )
        .await?
        {
            return Ok(());
        }

        if is_group {
            let bot_username = group_mention_bot_username.as_deref().unwrap_or("");
            let mentioned = if bot_username.is_empty() {
                false
            } else {
                message_mentions_bot(&text, bot_username)
            };
            let accepted = mentioned || replied_to_bot;
            debug!(
                chat_id,
                message_id = message.message_id,
                bot_username = %bot_username,
                mentioned,
                replied_to_bot,
                accepted,
                text_len = text.chars().count(),
                "telegram group mention check"
            );
            info!(
                chat_id,
                message_id,
                bot_username = %bot_username,
                mentioned,
                replied_to_bot,
                accepted,
                mention_entity_count,
                "telegram group trigger decision"
            );
            if !accepted {
                if bot_username.is_empty() {
                    info!(
                        chat_id,
                        message_id,
                        "telegram group message skipped: bot username unavailable and message is not a reply to bot"
                    );
                }
                info!(
                    chat_id,
                    message_id, "telegram group message skipped: no @mention and not reply-to-bot"
                );
                return Ok(());
            }
        }
        let user_id = message
            .from
            .as_ref()
            .map(|u| u.id.to_string())
            .unwrap_or_else(|| chat_id.to_string());
        let session_id = if is_group {
            telegram_session_id(chat_id, message_thread_id, reply_to_message_id)
        } else {
            format!("tg:{chat_id}")
        };
        let outbound_reply_to_message_id = if is_group && replied_to_bot {
            Some(message_id)
        } else {
            None
        };
        info!(
            chat_id,
            message_id,
            session_id = %session_id,
            outbound_reply_to_message_id = ?outbound_reply_to_message_id,
            "telegram message accepted for processing"
        );

        let incoming = IncomingMessage {
            channel: ctx.channel_name,
            session_id,
            user_id,
            text: text.to_string(),
            reply_target: Some(ReplyTarget::Telegram {
                chat_id,
                message_thread_id,
            }),
        };

        let typing_sender = sender.clone();
        let typing_task = tokio::spawn(async move {
            loop {
                if let Err(err) = typing_sender
                    .send_chat_action_typing(chat_id, message_thread_id)
                    .await
                {
                    warn!(error = %err, chat_id, "telegram typing heartbeat failed");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        let result: anyhow::Result<()> = if streaming_enabled {
            let mut sink = TelegramStreamSink::new(
                sender,
                chat_id,
                message_thread_id,
                outbound_reply_to_message_id,
                streaming_prefer_draft,
                Duration::from_millis(streaming_edit_interval_ms),
            );
            match ctx
                .service
                .handle_stream(incoming, &mut sink)
                .await
                .context("failed to process telegram streaming message")
            {
                Ok(reply) => sink.finalize(&reply.text).await,
                Err(err) => Err(err),
            }
        } else {
            match ctx
                .service
                .handle(incoming)
                .await
                .context("failed to process telegram message")
            {
                Ok(reply) => sender
                    .send_message(
                        chat_id,
                        message_thread_id,
                        outbound_reply_to_message_id,
                        &reply.text,
                    )
                    .await
                    .context("failed to send telegram response"),
                Err(err) => Err(err),
            }
        };

        typing_task.abort();
        let _ = typing_task.await;
        result?;
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramSlashCommand {
    Start,
    Help,
    WhoAmI,
    Mcp { tail: String },
    Unknown { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpCommandAccess {
    Allowed,
    RequirePrivateChat,
    MissingAdminAllowlist,
    Unauthorized,
}

async fn maybe_handle_telegram_command(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    text: &str,
    bot_username: Option<&str>,
    command_settings: &TelegramCommandSettings,
) -> anyhow::Result<bool> {
    let Some(command) = parse_telegram_slash_command(text, bot_username) else {
        return Ok(false);
    };
    let is_private_chat = is_private_chat_type(message.chat.kind.as_deref());

    match command {
        TelegramSlashCommand::Start => {
            send_telegram_command_reply(sender, message, &telegram_start_text()).await?;
            Ok(true)
        }
        TelegramSlashCommand::Help => {
            send_telegram_command_reply(sender, message, &telegram_help_text()).await?;
            Ok(true)
        }
        TelegramSlashCommand::WhoAmI => {
            send_telegram_command_reply(sender, message, &telegram_whoami_text(message)).await?;
            Ok(true)
        }
        TelegramSlashCommand::Mcp { tail } => {
            if !command_settings.enabled {
                send_telegram_command_reply(sender, message, "命令功能未启用。").await?;
                return Ok(true);
            }
            match evaluate_mcp_command_access(
                is_private_chat,
                message.from.as_ref().map(|u| u.id),
                command_settings,
            ) {
                McpCommandAccess::Allowed => {}
                McpCommandAccess::RequirePrivateChat => {
                    send_telegram_command_reply(sender, message, "请私聊使用 /mcp 管理命令。")
                        .await?;
                    return Ok(true);
                }
                McpCommandAccess::MissingAdminAllowlist => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!(
                            "管理员白名单未配置，命令不可用。\n{}\n\n请在 .env.realtest 中配置 TELEGRAM_ADMIN_USER_IDS，然后重启服务。",
                            telegram_whoami_hint(message)
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
                McpCommandAccess::Unauthorized => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!(
                            "无权限执行该命令。\n{}\n\n请将你的 ID 加到 TELEGRAM_ADMIN_USER_IDS。",
                            telegram_whoami_hint(message)
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
            }

            if tail.trim().is_empty() {
                send_telegram_command_reply(sender, message, &mcp_help_text()).await?;
                return Ok(true);
            }

            let parsed = match parse_telegram_mcp_command(&tail) {
                Ok(command) => command,
                Err(err) => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!("命令参数错误: {err}\n\n{}", mcp_help_text()),
                    )
                    .await?;
                    return Ok(true);
                }
            };

            let registry = match discover_mcp_registry() {
                Ok(registry) => registry,
                Err(err) => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!("无法加载 MCP 配置路径: {err}"),
                    )
                    .await?;
                    return Ok(true);
                }
            };

            match execute_mcp_command(&registry, parsed).await {
                Ok(output) => {
                    let mut out = if output.text.trim().is_empty() {
                        "命令执行完成。".to_string()
                    } else {
                        output.text
                    };
                    if output.reload_runtime {
                        match ctx
                            .service
                            .reload_mcp_runtime_from_registry(&registry)
                            .await
                        {
                            Ok(()) => {
                                out.push_str("\n\nMCP runtime 已热重载。");
                            }
                            Err(err) => {
                                out.push_str(&format!("\n\nMCP runtime 热重载失败: {err}"));
                            }
                        }
                    }
                    send_telegram_command_reply(sender, message, &out).await?;
                }
                Err(err) => {
                    send_telegram_command_reply(sender, message, &format!("命令执行失败: {err}"))
                        .await?;
                }
            }
            Ok(true)
        }
        TelegramSlashCommand::Unknown { name } => {
            if is_private_chat {
                send_telegram_command_reply(
                    sender,
                    message,
                    &format!("未知命令 /{name}。可用命令见 /help。"),
                )
                .await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

fn evaluate_mcp_command_access(
    is_private_chat: bool,
    caller_user_id: Option<i64>,
    command_settings: &TelegramCommandSettings,
) -> McpCommandAccess {
    if command_settings.private_only && !is_private_chat {
        return McpCommandAccess::RequirePrivateChat;
    }
    if command_settings.admin_user_ids.is_empty() {
        return McpCommandAccess::MissingAdminAllowlist;
    }
    if caller_user_id.is_none_or(|id| !command_settings.admin_user_ids.contains(&id)) {
        return McpCommandAccess::Unauthorized;
    }
    McpCommandAccess::Allowed
}

fn parse_telegram_slash_command(
    text: &str,
    bot_username: Option<&str>,
) -> Option<TelegramSlashCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let split_idx = trimmed
        .char_indices()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(idx))
        .unwrap_or(trimmed.len());
    let head = &trimmed[..split_idx];
    let tail = trimmed[split_idx..].trim().to_string();
    let raw = head.trim_start_matches('/');
    if raw.is_empty() {
        return None;
    }

    let (command, at_target) = raw.split_once('@').unwrap_or((raw, ""));
    if !at_target.is_empty()
        && bot_username.is_some_and(|v| {
            let expect = v.trim().trim_start_matches('@');
            !expect.is_empty() && !at_target.eq_ignore_ascii_case(expect)
        })
    {
        return None;
    }

    let command = command.to_ascii_lowercase();
    match command.as_str() {
        "start" => Some(TelegramSlashCommand::Start),
        "help" => Some(TelegramSlashCommand::Help),
        "whoami" => Some(TelegramSlashCommand::WhoAmI),
        "mcp" => Some(TelegramSlashCommand::Mcp { tail }),
        _ => Some(TelegramSlashCommand::Unknown { name: command }),
    }
}

fn telegram_start_text() -> String {
    [
        "你好，我是 xiaomaolv Telegram 机器人。",
        "输入 /help 查看可用命令。",
    ]
    .join("\n")
}

fn telegram_help_text() -> String {
    [
        "可用命令:",
        "/start - 查看欢迎信息",
        "/help - 查看帮助",
        "/whoami - 查看你的 Telegram 用户 ID",
        "/mcp - 管理 MCP 服务器（仅私聊管理员）",
    ]
    .join("\n")
}

fn telegram_whoami_text(message: &TelegramMessage) -> String {
    match message.from.as_ref().map(|u| u.id) {
        Some(user_id) => format!(
            "你的 Telegram 用户 ID: {user_id}\n建议配置:\nTELEGRAM_ADMIN_USER_IDS={user_id}"
        ),
        None => "无法识别当前用户 ID（message.from 缺失）。请使用普通用户账号私聊 bot 再试一次。"
            .to_string(),
    }
}

fn telegram_whoami_hint(message: &TelegramMessage) -> String {
    match message.from.as_ref().map(|u| u.id) {
        Some(user_id) => format!("你的当前用户 ID: {user_id}（也可发送 /whoami 查看）"),
        None => "可发送 /whoami 查看你的 Telegram 用户 ID。".to_string(),
    }
}

fn telegram_registered_commands() -> Vec<(&'static str, &'static str)> {
    vec![
        ("start", "启动与介绍"),
        ("help", "查看帮助"),
        ("whoami", "查看当前用户ID"),
        ("mcp", "管理 MCP 服务器"),
    ]
}

async fn send_telegram_command_reply(
    sender: &TelegramSender,
    message: &TelegramMessage,
    text: &str,
) -> anyhow::Result<()> {
    sender
        .send_message(
            message.chat.id,
            message.message_thread_id,
            Some(message.message_id),
            text,
        )
        .await
}

fn is_private_chat_type(kind: Option<&str>) -> bool {
    matches!(
        kind.map(|v| v.trim().to_ascii_lowercase()),
        Some(v) if v == "private"
    )
}

struct TelegramStreamSink<'a> {
    sender: &'a TelegramSender,
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    prefer_draft: bool,
    draft_supported: bool,
    draft_message_id: String,
    min_edit_interval: Duration,
    message_id: Option<i64>,
    tail_message_ids: Vec<i64>,
    pending_text: String,
    rendered_parts: Vec<TelegramRenderedText>,
    last_push: Option<Instant>,
}

impl<'a> TelegramStreamSink<'a> {
    fn new(
        sender: &'a TelegramSender,
        chat_id: i64,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
        prefer_draft: bool,
        min_edit_interval: Duration,
    ) -> Self {
        Self {
            sender,
            chat_id,
            message_thread_id,
            reply_to_message_id,
            prefer_draft,
            draft_supported: message_thread_id.is_some() && reply_to_message_id.is_none(),
            draft_message_id: build_draft_message_id(chat_id, message_thread_id),
            min_edit_interval,
            message_id: None,
            tail_message_ids: Vec::new(),
            pending_text: String::new(),
            rendered_parts: Vec::new(),
            last_push: None,
        }
    }

    async fn finalize(&mut self, full_text: &str) -> anyhow::Result<()> {
        if full_text.is_empty() {
            return Ok(());
        }

        self.pending_text = full_text.to_string();
        let parts = render_telegram_text_parts(&self.pending_text, TELEGRAM_MAX_TEXT_CHARS);
        if !parts.is_empty() && parts == self.rendered_parts {
            return Ok(());
        }

        if self.prefer_draft && self.draft_supported && self.message_id.is_none() {
            if let Err(err) = self
                .sender
                .send_message(
                    self.chat_id,
                    self.message_thread_id,
                    self.reply_to_message_id,
                    full_text,
                )
                .await
            {
                warn!(error = %err, "telegram stream final send after draft failed, fallback to send/edit");
                self.draft_supported = false;
            } else {
                self.rendered_parts = parts;
                self.last_push = Some(Instant::now());
                return Ok(());
            }
        }

        if let Err(err) = self.flush(true).await {
            warn!(error = %err, "telegram stream finalize failed, fallback to sendMessage");
            self.sender
                .send_message(
                    self.chat_id,
                    self.message_thread_id,
                    self.reply_to_message_id,
                    full_text,
                )
                .await
                .context("telegram stream fallback sendMessage failed")?;
            self.rendered_parts =
                render_telegram_text_parts(&self.pending_text, TELEGRAM_MAX_TEXT_CHARS);
            self.last_push = Some(Instant::now());
        }

        Ok(())
    }

    async fn flush(&mut self, force: bool) -> anyhow::Result<()> {
        if self.pending_text.is_empty() {
            return Ok(());
        }

        if !force
            && let Some(last) = self.last_push
            && last.elapsed() < self.min_edit_interval
        {
            return Ok(());
        }

        let parts = render_telegram_text_parts(&self.pending_text, TELEGRAM_MAX_TEXT_CHARS);
        if parts.is_empty() || parts == self.rendered_parts {
            return Ok(());
        }

        if self.prefer_draft
            && self.draft_supported
            && self.message_id.is_none()
            && self.tail_message_ids.is_empty()
        {
            match self
                .sender
                .send_message_draft(
                    self.chat_id,
                    self.message_thread_id,
                    self.reply_to_message_id,
                    &self.draft_message_id,
                    &self.pending_text,
                )
                .await
            {
                Ok(_) => {
                    self.rendered_parts = parts;
                    self.last_push = Some(Instant::now());
                    return Ok(());
                }
                Err(err) => {
                    warn!(error = %err, "telegram sendMessageDraft failed, fallback to send/edit stream");
                    self.draft_supported = false;
                }
            }
        }

        if self.message_id.is_none() {
            let message_id = self
                .sender
                .send_rendered_message_with_id(
                    self.chat_id,
                    self.message_thread_id,
                    self.reply_to_message_id,
                    &parts[0],
                )
                .await
                .context("failed to send telegram stream message")?;
            self.message_id = Some(message_id);
        } else if self.rendered_parts.first() != Some(&parts[0]) {
            self.sender
                .edit_rendered_message(
                    self.chat_id,
                    self.message_id.expect("checked above"),
                    &parts[0],
                )
                .await
                .context("failed to edit telegram stream message")?;
        }

        for (i, rendered_tail) in parts.iter().enumerate().skip(1) {
            let tail_idx = i - 1;
            if let Some(msg_id) = self.tail_message_ids.get(tail_idx).copied() {
                if self.rendered_parts.get(i) != Some(rendered_tail) {
                    self.sender
                        .edit_rendered_message(self.chat_id, msg_id, rendered_tail)
                        .await
                        .context("failed to edit telegram stream tail message")?;
                }
            } else {
                let tail_id = self
                    .sender
                    .send_rendered_message_with_id(
                        self.chat_id,
                        self.message_thread_id,
                        None,
                        rendered_tail,
                    )
                    .await
                    .context("failed to send telegram stream tail message")?;
                self.tail_message_ids.push(tail_id);
            }
        }

        self.rendered_parts = parts;
        self.last_push = Some(Instant::now());
        Ok(())
    }
}

#[async_trait]
impl StreamSink for TelegramStreamSink<'_> {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        if delta.is_empty() {
            return Ok(());
        }

        self.pending_text.push_str(delta);
        if let Err(err) = self.flush(false).await {
            warn!(error = %err, "telegram stream flush failed, continue buffering");
        }
        Ok(())
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_admin_user_ids(raw: &str) -> anyhow::Result<Vec<i64>> {
    if raw.trim().is_empty() {
        return Ok(vec![]);
    }

    raw.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| {
            v.parse::<i64>()
                .with_context(|| format!("invalid telegram admin user id '{v}'"))
        })
        .collect()
}

fn is_group_chat_type(kind: Option<&str>) -> bool {
    matches!(
        kind.map(|v| v.trim().to_ascii_lowercase()),
        Some(v) if v == "group" || v == "supergroup"
    )
}

fn is_reply_to_bot_message(reply: Option<&TelegramReplyMessage>) -> bool {
    reply
        .and_then(|msg| msg.from.as_ref())
        .and_then(|from| from.is_bot)
        .unwrap_or(false)
}

fn message_mentions_bot(text: &str, bot_username: &str) -> bool {
    let username = bot_username
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    if username.is_empty() {
        return false;
    }
    let needle = format!("@{username}");
    let hay = text.to_ascii_lowercase();
    let mut start = 0usize;
    while let Some(found) = hay[start..].find(&needle) {
        let idx = start + found;
        let after_idx = idx + needle.len();
        let before = idx
            .checked_sub(1)
            .and_then(|i| hay.as_bytes().get(i))
            .copied();
        let after = hay.as_bytes().get(after_idx).copied();
        if is_mention_boundary(before) && is_mention_boundary(after) {
            return true;
        }
        start = after_idx;
    }
    false
}

fn is_mention_boundary(ch: Option<u8>) -> bool {
    match ch {
        None => true,
        Some(v) => {
            let c = v as char;
            !(c.is_ascii_alphanumeric() || c == '_')
        }
    }
}

fn telegram_session_id(
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
) -> String {
    if let Some(thread_id) = message_thread_id {
        return format!("tg:{chat_id}:thread:{thread_id}");
    }
    if let Some(reply_id) = reply_to_message_id {
        return format!("tg:{chat_id}:reply:{reply_id}");
    }
    format!("tg:{chat_id}")
}

fn typing_action_payload(chat_id: i64, message_thread_id: Option<i64>) -> Value {
    let mut payload = serde_json::json!({
        "chat_id": chat_id,
        "action": "typing"
    });
    if let Some(thread_id) = message_thread_id {
        payload["message_thread_id"] = serde_json::json!(thread_id);
    }
    payload
}

fn short_description_payload(short_description: &str) -> Value {
    serde_json::json!({
        "short_description": short_description
    })
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn build_draft_message_id(chat_id: i64, message_thread_id: Option<i64>) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let thread = message_thread_id.unwrap_or(0);
    format!("xm-{chat_id}-{thread}-{millis}")
}

fn render_telegram_text_parts(raw: &str, max_chars: usize) -> Vec<TelegramRenderedText> {
    let (think, body) = split_think_and_body(raw);
    if think.trim().is_empty() {
        return split_text_chunks(raw, max_chars.max(1))
            .into_iter()
            .map(|chunk| TelegramRenderedText {
                text: chunk,
                parse_mode: None,
            })
            .collect();
    }

    let think_prefix = "<blockquote>🧠 <tg-spoiler>";
    let think_suffix = "</tg-spoiler></blockquote>";
    let think_overhead = think_prefix.chars().count() + think_suffix.chars().count();
    let think_chunk_max = max_chars.saturating_sub(think_overhead).max(1);

    let mut out = Vec::new();
    for think_chunk in split_text_chunks(think.trim(), think_chunk_max) {
        out.push(TelegramRenderedText {
            text: format!("{think_prefix}{}{think_suffix}", escape_html(&think_chunk)),
            parse_mode: Some("HTML"),
        });
    }

    let body_trimmed = body.trim();
    if !body_trimmed.is_empty() {
        for body_chunk in split_text_chunks(body_trimmed, max_chars.max(1)) {
            out.push(TelegramRenderedText {
                text: escape_html(&body_chunk),
                parse_mode: Some("HTML"),
            });
        }
    }

    out
}

fn split_text_chunks(input: &str, max_chars: usize) -> Vec<String> {
    if input.is_empty() {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);

    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut count = 0usize;

    for (idx, _) in input.char_indices() {
        if count == max_chars {
            chunks.push(input[start..idx].to_string());
            start = idx;
            count = 0;
        }
        count += 1;
    }

    if start < input.len() {
        chunks.push(input[start..].to_string());
    }

    chunks
}

fn split_think_and_body(raw: &str) -> (String, String) {
    let mut remaining = raw;
    let mut think_parts: Vec<String> = Vec::new();
    let mut body = String::new();
    let open = "<think>";
    let close = "</think>";

    while let Some(start) = remaining.find(open) {
        body.push_str(&remaining[..start]);
        remaining = &remaining[start + open.len()..];

        if let Some(end) = remaining.find(close) {
            think_parts.push(remaining[..end].to_string());
            remaining = &remaining[end + close.len()..];
        } else {
            think_parts.push(remaining.to_string());
            remaining = "";
            break;
        }
    }

    body.push_str(remaining);
    (think_parts.join("\n\n"), body)
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars.max(1)).collect()
}

async fn fetch_telegram_updates(
    client: &Client,
    bot_token: &str,
    offset: i64,
    timeout_secs: u64,
) -> anyhow::Result<Vec<TelegramPollUpdate>> {
    let url = format!("https://api.telegram.org/bot{bot_token}/getUpdates");
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "offset": offset,
            "timeout": timeout_secs,
            "allowed_updates": ["message"]
        }))
        .send()
        .await
        .context("failed to call telegram getUpdates")?;

    let status = response.status();
    let raw = response
        .bytes()
        .await
        .context("failed to read telegram getUpdates payload")?;
    if !status.is_success() {
        let raw_text = String::from_utf8_lossy(&raw);
        let preview = truncate_chars(raw_text.trim(), 800);
        bail!("telegram getUpdates returned error: http_status={status} body={preview}");
    }

    let body: TelegramApiResponse<Vec<TelegramPollUpdate>> =
        serde_json::from_slice(&raw).context("failed to decode telegram getUpdates payload")?;

    if !body.ok {
        let retry_after = body.parameters.as_ref().and_then(|p| p.retry_after);
        let description = body
            .description
            .unwrap_or_else(|| "unknown error".to_string());
        if let Some(seconds) = retry_after {
            bail!("telegram getUpdates returned ok=false: {description} (retry_after={seconds}s)");
        }
        bail!("telegram getUpdates returned ok=false: {}", description);
    }

    Ok(body.result)
}

async fn delete_telegram_webhook(client: &Client, bot_token: &str) -> anyhow::Result<()> {
    let url = format!("https://api.telegram.org/bot{bot_token}/deleteWebhook");
    client
        .post(url)
        .json(&serde_json::json!({ "drop_pending_updates": false }))
        .send()
        .await
        .context("failed to call telegram deleteWebhook")?
        .error_for_status()
        .context("telegram deleteWebhook returned error")?;
    Ok(())
}

async fn wait_for_shutdown_or_timeout(
    shutdown: &mut watch::Receiver<bool>,
    timeout: Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(timeout) => false,
        changed = shutdown.changed() => {
            if changed.is_err() {
                true
            } else {
                *shutdown.borrow()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        McpCommandAccess, TELEGRAM_MAX_TEXT_CHARS, TelegramCommandSettings, TelegramReplyMessage,
        TelegramSlashCommand, TelegramUser, build_draft_message_id, evaluate_mcp_command_access,
        is_reply_to_bot_message, message_mentions_bot, parse_admin_user_ids,
        parse_telegram_slash_command, render_telegram_text_parts, short_description_payload,
        telegram_session_id, truncate_chars, typing_action_payload,
    };

    #[test]
    fn think_block_is_rendered_with_brain_prefix_and_body() {
        let raw = "<think>内部推理A\n内部推理B</think>\n\n最终回答";
        let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
        let rendered = &parts[0];

        assert_eq!(rendered.parse_mode, Some("HTML"));
        assert!(rendered.text.contains("<blockquote>"));
        assert!(rendered.text.contains("🧠 "));
        assert!(rendered.text.contains("<tg-spoiler>"));
        assert!(rendered.text.contains("🧠 <tg-spoiler>"));
        assert!(parts.iter().any(|p| p.text.contains("最终回答")));
        assert!(!rendered.text.contains("思考草稿（点击展开）"));
        assert!(!rendered.text.contains("<think>"));
        assert!(!rendered.text.contains("</think>"));
    }

    #[test]
    fn plain_text_keeps_original_mode() {
        let raw = "你好，这里是正文。";
        let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
        let rendered = &parts[0];
        assert_eq!(rendered.parse_mode, None);
        assert_eq!(rendered.text, raw);
    }

    #[test]
    fn unclosed_think_is_rendered_with_brain_prefix() {
        let raw = "<think>这段还没闭合";
        let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
        let rendered = &parts[0];
        assert_eq!(rendered.parse_mode, Some("HTML"));
        assert!(rendered.text.contains("<blockquote>"));
        assert!(rendered.text.contains("🧠 "));
        assert!(rendered.text.contains("<tg-spoiler>"));
        assert!(rendered.text.contains("🧠 <tg-spoiler>"));
        assert!(rendered.text.contains("这段还没闭合"));
    }

    #[test]
    fn draft_message_id_format_is_stable() {
        let draft_id = build_draft_message_id(-100123, Some(88));
        assert!(draft_id.starts_with("xm-"));
        assert!(draft_id.contains("-100123-88-"));
        assert!(draft_id.len() < 128);
    }

    #[test]
    fn typing_payload_includes_action_and_chat_id() {
        let payload = typing_action_payload(123, None);
        assert_eq!(payload["chat_id"], 123);
        assert_eq!(payload["action"], "typing");
        assert!(payload.get("message_thread_id").is_none());
    }

    #[test]
    fn typing_payload_includes_thread_id_when_present() {
        let payload = typing_action_payload(-100, Some(7));
        assert_eq!(payload["chat_id"], -100);
        assert_eq!(payload["action"], "typing");
        assert_eq!(payload["message_thread_id"], 7);
    }

    #[test]
    fn short_description_payload_has_expected_shape() {
        let payload = short_description_payload("online");
        assert_eq!(payload["short_description"], "online");
    }

    #[test]
    fn truncate_chars_respects_limit() {
        assert_eq!(truncate_chars("abcdef", 3), "abc".to_string());
        assert_eq!(truncate_chars("你好世界", 2), "你好".to_string());
    }

    #[test]
    fn mention_detection_matches_username_boundaries() {
        assert!(message_mentions_bot(
            "hello @xiaomaolv_bot",
            "xiaomaolv_bot"
        ));
        assert!(message_mentions_bot(
            "@xiaomaolv_bot，请回复",
            "xiaomaolv_bot"
        ));
        assert!(!message_mentions_bot(
            "not-mention@xiaomaolv_botx",
            "xiaomaolv_bot"
        ));
        assert!(!message_mentions_bot("hello @other_bot", "xiaomaolv_bot"));
    }

    #[test]
    fn reply_to_bot_detection_uses_reply_from_is_bot() {
        let reply_from_bot = TelegramReplyMessage {
            message_id: 1,
            from: Some(TelegramUser {
                id: 42,
                is_bot: Some(true),
            }),
        };
        let reply_from_user = TelegramReplyMessage {
            message_id: 2,
            from: Some(TelegramUser {
                id: 7,
                is_bot: Some(false),
            }),
        };
        let reply_without_from = TelegramReplyMessage {
            message_id: 3,
            from: None,
        };

        assert!(is_reply_to_bot_message(Some(&reply_from_bot)));
        assert!(!is_reply_to_bot_message(Some(&reply_from_user)));
        assert!(!is_reply_to_bot_message(Some(&reply_without_from)));
        assert!(!is_reply_to_bot_message(None));
    }

    #[test]
    fn telegram_session_id_uses_thread_then_reply_then_chat() {
        assert_eq!(
            telegram_session_id(-1001, Some(88), Some(9)),
            "tg:-1001:thread:88".to_string()
        );
        assert_eq!(
            telegram_session_id(-1001, None, Some(9)),
            "tg:-1001:reply:9".to_string()
        );
        assert_eq!(
            telegram_session_id(-1001, None, None),
            "tg:-1001".to_string()
        );
    }

    #[test]
    fn long_plain_text_is_split_into_multiple_messages() {
        let raw = "a".repeat(TELEGRAM_MAX_TEXT_CHARS + 37);
        let parts = render_telegram_text_parts(&raw, TELEGRAM_MAX_TEXT_CHARS);
        assert!(parts.len() >= 2);
        for part in &parts {
            assert!(part.text.chars().count() <= TELEGRAM_MAX_TEXT_CHARS);
            assert_eq!(part.parse_mode, None);
        }
    }

    #[test]
    fn long_think_text_is_split_into_multiple_messages() {
        let think = "推理".repeat(TELEGRAM_MAX_TEXT_CHARS);
        let raw = format!("<think>{think}</think>\n\n最终结论");
        let parts = render_telegram_text_parts(&raw, TELEGRAM_MAX_TEXT_CHARS);
        assert!(parts.len() >= 2);
        assert!(parts[0].text.contains("<tg-spoiler>"));
        assert!(parts[0].text.contains("🧠 "));
        for part in &parts {
            assert!(part.text.chars().count() <= TELEGRAM_MAX_TEXT_CHARS);
        }
    }

    #[test]
    fn parse_slash_command_supports_bot_username_suffix() {
        let parsed = parse_telegram_slash_command(
            "/mcp@xiaomaolv_bot ls --scope merged",
            Some("xiaomaolv_bot"),
        );
        assert_eq!(
            parsed,
            Some(TelegramSlashCommand::Mcp {
                tail: "ls --scope merged".to_string()
            })
        );
    }

    #[test]
    fn parse_slash_command_ignores_other_bot_username() {
        let parsed = parse_telegram_slash_command("/mcp@other_bot ls", Some("xiaomaolv_bot"));
        assert_eq!(parsed, None);
    }

    #[test]
    fn parse_slash_command_supports_whoami() {
        let parsed = parse_telegram_slash_command("/whoami", Some("xiaomaolv_bot"));
        assert_eq!(parsed, Some(TelegramSlashCommand::WhoAmI));
    }

    #[test]
    fn parse_admin_user_ids_supports_csv() {
        let ids = parse_admin_user_ids("123, 456 ,789").expect("ids");
        assert_eq!(ids, vec![123, 456, 789]);
    }

    #[test]
    fn mcp_command_access_requires_private_chat_first() {
        let settings = TelegramCommandSettings {
            enabled: true,
            private_only: true,
            admin_user_ids: vec![42],
        };
        let access = evaluate_mcp_command_access(false, Some(42), &settings);
        assert_eq!(access, McpCommandAccess::RequirePrivateChat);
    }

    #[test]
    fn mcp_command_access_rejects_non_admin() {
        let settings = TelegramCommandSettings {
            enabled: true,
            private_only: true,
            admin_user_ids: vec![42],
        };
        let access = evaluate_mcp_command_access(true, Some(7), &settings);
        assert_eq!(access, McpCommandAccess::Unauthorized);
    }
}
