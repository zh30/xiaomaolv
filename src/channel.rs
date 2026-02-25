use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
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
use crate::memory::{
    CompleteTelegramSchedulerJobRunRequest, CreateTelegramSchedulerJobRequest,
    FailTelegramSchedulerJobRunRequest, TelegramSchedulerJobRecord, TelegramSchedulerJobStatus,
    TelegramSchedulerScheduleKind, TelegramSchedulerTaskKind,
    UpsertTelegramSchedulerPendingIntentRequest,
};
use crate::provider::StreamSink;
use crate::scheduler::{ScheduleSpec, compute_next_run_at_unix, compute_retry_backoff_secs};
use crate::service::{MessageService, TelegramSchedulerIntent};

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
    pub username: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
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
    id: i64,
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
    group_messages_total: u64,
    group_decision_respond_total: u64,
    group_decision_observe_total: u64,
    group_decision_ignore_total: u64,
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
    group_messages_total: u64,
    group_decision_respond_total: u64,
    group_decision_observe_total: u64,
    group_decision_ignore_total: u64,
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
            group_messages_total: self.group_messages_total,
            group_decision_respond_total: self.group_decision_respond_total,
            group_decision_observe_total: self.group_decision_observe_total,
            group_decision_ignore_total: self.group_decision_ignore_total,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct TelegramSchedulerDiagSnapshot {
    worker_started_at_unix: Option<u64>,
    last_tick_at_unix: Option<u64>,
    last_claimed_jobs: usize,
    total_claimed_jobs: u64,
    jobs_total: i64,
    jobs_active: i64,
    jobs_paused: i64,
    jobs_completed: i64,
    jobs_canceled: i64,
    jobs_due: i64,
    total_runs_ok: u64,
    total_runs_err: u64,
    last_run_ok_at_unix: Option<u64>,
    last_run_err_at_unix: Option<u64>,
    last_error: Option<String>,
}

#[derive(Debug, Default)]
struct TelegramSchedulerDiagState {
    worker_started_at_unix: Option<u64>,
    last_tick_at_unix: Option<u64>,
    last_claimed_jobs: usize,
    total_claimed_jobs: u64,
    jobs_total: i64,
    jobs_active: i64,
    jobs_paused: i64,
    jobs_completed: i64,
    jobs_canceled: i64,
    jobs_due: i64,
    total_runs_ok: u64,
    total_runs_err: u64,
    last_run_ok_at_unix: Option<u64>,
    last_run_err_at_unix: Option<u64>,
    last_error: Option<String>,
}

impl TelegramSchedulerDiagState {
    fn snapshot(&self) -> TelegramSchedulerDiagSnapshot {
        TelegramSchedulerDiagSnapshot {
            worker_started_at_unix: self.worker_started_at_unix,
            last_tick_at_unix: self.last_tick_at_unix,
            last_claimed_jobs: self.last_claimed_jobs,
            total_claimed_jobs: self.total_claimed_jobs,
            jobs_total: self.jobs_total,
            jobs_active: self.jobs_active,
            jobs_paused: self.jobs_paused,
            jobs_completed: self.jobs_completed,
            jobs_canceled: self.jobs_canceled,
            jobs_due: self.jobs_due,
            total_runs_ok: self.total_runs_ok,
            total_runs_err: self.total_runs_err,
            last_run_ok_at_unix: self.last_run_ok_at_unix,
            last_run_err_at_unix: self.last_run_err_at_unix,
            last_error: self.last_error.clone(),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TelegramGroupTriggerMode {
    Strict,
    Smart,
}

impl TelegramGroupTriggerMode {
    fn parse(raw: Option<&str>) -> anyhow::Result<Self> {
        let value = raw.unwrap_or("strict").trim().to_ascii_lowercase();
        match value.as_str() {
            "strict" => Ok(Self::Strict),
            "smart" => Ok(Self::Smart),
            other => {
                bail!("unsupported telegram group trigger mode '{other}', expected strict|smart")
            }
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Smart => "smart",
        }
    }
}

#[derive(Clone, Debug)]
struct TelegramGroupTriggerSettings {
    mode: TelegramGroupTriggerMode,
    followup_window_secs: u64,
    cooldown_secs: u64,
    rule_min_score: u32,
    llm_gate_enabled: bool,
}

#[derive(Clone, Debug)]
struct TelegramSchedulerSettings {
    enabled: bool,
    tick_secs: u64,
    batch_size: usize,
    lease_secs: u64,
    default_timezone: String,
    nl_enabled: bool,
    nl_min_confidence: f32,
    require_confirm: bool,
    max_jobs_per_owner: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramSchedulerIntentDraft {
    task_kind: TelegramSchedulerTaskKind,
    schedule_kind: TelegramSchedulerScheduleKind,
    payload: String,
    timezone: String,
    run_at_unix: Option<i64>,
    cron_expr: Option<String>,
    source_text: String,
}

const TELEGRAM_SCHEDULER_PENDING_TTL_SECS: i64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchedulerJobOperation {
    Pause,
    Resume,
    Delete,
}

#[derive(Debug, Clone)]
enum SchedulerJobTargetResolution {
    Selected {
        job_id: String,
        inferred: bool,
    },
    Ambiguous {
        options: Vec<TelegramSchedulerJobRecord>,
    },
    Empty,
}

#[derive(Debug, Default)]
struct TelegramGroupRuntimeState {
    last_bot_response_unix_by_chat: HashMap<i64, u64>,
    learned_aliases_by_chat: HashMap<i64, HashSet<String>>,
    loaded_aliases_chats: HashSet<i64>,
    user_profiles_by_chat: HashMap<i64, HashMap<i64, TelegramGroupUserProfile>>,
    loaded_profile_chats: HashSet<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramGroupUserProfile {
    preferred_name: String,
    username: Option<String>,
    updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupDecisionKind {
    Respond,
    ObserveOnly,
    Ignore,
}

impl GroupDecisionKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Respond => "respond",
            Self::ObserveOnly => "observe_only",
            Self::Ignore => "ignore",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupDecision {
    kind: GroupDecisionKind,
    score: i32,
    reasons: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupSignalInput {
    mentioned: bool,
    replied_to_bot: bool,
    recent_bot_participation: bool,
    alias_hit: bool,
    has_question_marker: bool,
    points_to_other_bot: bool,
    low_signal_noise: bool,
    cooldown_active: bool,
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
        let group_trigger_mode = TelegramGroupTriggerMode::parse(
            config
                .settings
                .get("group_trigger_mode")
                .map(String::as_str),
        )
        .context("failed to parse telegram settings.group_trigger_mode")?;
        let group_followup_window_secs = config
            .settings
            .get("group_followup_window_secs")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(180)
            .clamp(10, 86_400);
        let group_cooldown_secs = config
            .settings
            .get("group_cooldown_secs")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(20)
            .clamp(0, 3_600);
        let group_rule_min_score = config
            .settings
            .get("group_rule_min_score")
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(70)
            .clamp(1, 100);
        let group_llm_gate_enabled = config
            .settings
            .get("group_llm_gate_enabled")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(false);
        let scheduler_enabled = config
            .settings
            .get("scheduler_enabled")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(true);
        let scheduler_tick_secs = config
            .settings
            .get("scheduler_tick_secs")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(2)
            .clamp(1, 60);
        let scheduler_batch_size = config
            .settings
            .get("scheduler_batch_size")
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(8)
            .clamp(1, 128);
        let scheduler_lease_secs = config
            .settings
            .get("scheduler_lease_secs")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(30)
            .clamp(5, 600);
        let scheduler_default_timezone = config
            .settings
            .get("scheduler_default_timezone")
            .map(|raw| raw.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "Asia/Shanghai".to_string());
        let scheduler_nl_enabled = config
            .settings
            .get("scheduler_nl_enabled")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(true);
        let scheduler_nl_min_confidence = config
            .settings
            .get("scheduler_nl_min_confidence")
            .and_then(|raw| raw.parse::<f32>().ok())
            .unwrap_or(0.78)
            .clamp(0.0, 1.0);
        let scheduler_require_confirm = config
            .settings
            .get("scheduler_require_confirm")
            .and_then(|raw| parse_bool(raw))
            .unwrap_or(true);
        let scheduler_max_jobs_per_owner = config
            .settings
            .get("scheduler_max_jobs_per_owner")
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(64)
            .clamp(1, 1024);

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
            group_trigger_settings: TelegramGroupTriggerSettings {
                mode: group_trigger_mode,
                followup_window_secs: group_followup_window_secs,
                cooldown_secs: group_cooldown_secs,
                rule_min_score: group_rule_min_score,
                llm_gate_enabled: group_llm_gate_enabled,
            },
            scheduler_settings: TelegramSchedulerSettings {
                enabled: scheduler_enabled,
                tick_secs: scheduler_tick_secs,
                batch_size: scheduler_batch_size,
                lease_secs: scheduler_lease_secs,
                default_timezone: scheduler_default_timezone,
                nl_enabled: scheduler_nl_enabled,
                nl_min_confidence: scheduler_nl_min_confidence,
                require_confirm: scheduler_require_confirm,
                max_jobs_per_owner: scheduler_max_jobs_per_owner,
            },
            group_mention_bot_username_cache: Arc::new(tokio::sync::Mutex::new(
                group_mention_bot_username,
            )),
            bot_user_id_cache: Arc::new(tokio::sync::Mutex::new(None)),
            polling_diag_state: Arc::new(tokio::sync::Mutex::new(
                TelegramPollingDiagState::default(),
            )),
            group_runtime_state: Arc::new(tokio::sync::Mutex::new(
                TelegramGroupRuntimeState::default(),
            )),
            scheduler_diag_state: Arc::new(tokio::sync::Mutex::new(
                TelegramSchedulerDiagState::default(),
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
    group_trigger_settings: TelegramGroupTriggerSettings,
    scheduler_settings: TelegramSchedulerSettings,
    group_mention_bot_username_cache: Arc<tokio::sync::Mutex<Option<String>>>,
    bot_user_id_cache: Arc<tokio::sync::Mutex<Option<i64>>>,
    polling_diag_state: Arc<tokio::sync::Mutex<TelegramPollingDiagState>>,
    group_runtime_state: Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
    scheduler_diag_state: Arc<tokio::sync::Mutex<TelegramSchedulerDiagState>>,
}

#[derive(Clone)]
struct TelegramCommandSettings {
    enabled: bool,
    private_only: bool,
    admin_user_ids: Vec<i64>,
    scheduler: TelegramSchedulerSettings,
}

impl TelegramChannelPlugin {
    async fn resolve_group_mention_bot_username(&self) -> Option<String> {
        resolve_group_mention_bot_username_with_cache(
            &self.sender,
            &self.group_mention_bot_username_cache,
        )
        .await
    }

    async fn resolve_bot_user_id(&self) -> Option<i64> {
        resolve_bot_user_id_with_cache(&self.sender, &self.bot_user_id_cache).await
    }

    fn command_settings(&self) -> TelegramCommandSettings {
        TelegramCommandSettings {
            enabled: self.commands_enabled,
            private_only: self.commands_private_only,
            admin_user_ids: self.admin_user_ids.clone(),
            scheduler: self.scheduler_settings.clone(),
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

async fn resolve_bot_user_id_with_cache(
    sender: &TelegramSender,
    cache: &Arc<tokio::sync::Mutex<Option<i64>>>,
) -> Option<i64> {
    {
        let cached = cache.lock().await;
        if let Some(user_id) = *cached {
            return Some(user_id);
        }
    }

    match sender.get_me_profile().await {
        Ok(profile) => {
            let mut cached = cache.lock().await;
            *cached = Some(profile.id);
            Some(profile.id)
        }
        Err(err) => {
            warn!(
                error = %format!("{err:#}"),
                "failed to fetch telegram bot user id"
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
            self.resolve_bot_user_id().await,
            self.command_settings(),
            self.group_trigger_settings.clone(),
            self.group_runtime_state.clone(),
            self.polling_diag_state.clone(),
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
                {
                    let mut cached = self.bot_user_id_cache.lock().await;
                    if cached.is_none() {
                        *cached = Some(profile.id);
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

        let scheduler_settings = self.scheduler_settings.clone();
        let scheduler_diag_state = self.scheduler_diag_state.clone();

        if !matches!(self.mode, TelegramIngressMode::Polling) {
            if !scheduler_settings.enabled {
                return Ok(None);
            }
            let channel_name = ctx.channel_name.clone();
            let worker_name = format!("{channel_name}-scheduler");
            let service = ctx.service.clone();
            let sender = self.sender.clone();
            let shutdown = ctx.shutdown.clone();

            let task = tokio::spawn(async move {
                run_telegram_scheduler_loop(
                    channel_name,
                    service,
                    sender,
                    scheduler_settings,
                    shutdown,
                    scheduler_diag_state,
                )
                .await;
            });

            return Ok(Some(ChannelWorker {
                name: worker_name,
                task,
            }));
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
        let group_trigger_settings = self.group_trigger_settings.clone();
        let group_runtime_state = self.group_runtime_state.clone();
        let group_mention_bot_username_cache = self.group_mention_bot_username_cache.clone();
        let bot_user_id_cache = self.bot_user_id_cache.clone();
        let polling_diag_state = self.polling_diag_state.clone();
        let worker_name = format!("{channel_name}-polling");

        let task = tokio::spawn(async move {
            info!(channel = %channel_name, "telegram polling worker started");
            let mut scheduler_task = if scheduler_settings.enabled {
                let scheduler_channel_name = channel_name.clone();
                let scheduler_service = service.clone();
                let scheduler_sender = sender.clone();
                let scheduler_shutdown = shutdown.clone();
                let scheduler_diag = scheduler_diag_state.clone();
                let scheduler_cfg = scheduler_settings.clone();
                Some(tokio::spawn(async move {
                    run_telegram_scheduler_loop(
                        scheduler_channel_name,
                        scheduler_service,
                        scheduler_sender,
                        scheduler_cfg,
                        scheduler_shutdown,
                        scheduler_diag,
                    )
                    .await;
                }))
            } else {
                None
            };

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
                            let bot_user_id =
                                resolve_bot_user_id_with_cache(&sender, &bot_user_id_cache).await;

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
                                bot_user_id,
                                command_settings.clone(),
                                group_trigger_settings.clone(),
                                group_runtime_state.clone(),
                                polling_diag_state.clone(),
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

            if let Some(task) = scheduler_task.take() {
                task.abort();
                let _ = task.await;
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
        let cached_bot_user_id = {
            let cached = self.bot_user_id_cache.lock().await;
            *cached
        };
        let polling = {
            let diag = self.polling_diag_state.lock().await;
            diag.snapshot()
        };
        let scheduler = {
            let diag = self.scheduler_diag_state.lock().await;
            diag.snapshot()
        };
        let (
            learned_alias_chats,
            learned_alias_total,
            learned_profile_chats,
            learned_profile_total,
        ) = {
            let runtime = self.group_runtime_state.lock().await;
            (
                runtime.learned_aliases_by_chat.len(),
                runtime
                    .learned_aliases_by_chat
                    .values()
                    .map(|v| v.len())
                    .sum::<usize>(),
                runtime.user_profiles_by_chat.len(),
                runtime
                    .user_profiles_by_chat
                    .values()
                    .map(|v| v.len())
                    .sum::<usize>(),
            )
        };

        let get_me = match self.sender.get_me_profile().await {
            Ok(profile) => serde_json::json!({
                "ok": true,
                "id": profile.id,
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
            "bot_user_id_cached": cached_bot_user_id,
            "commands": {
                "enabled": self.commands_enabled,
                "auto_register": self.commands_auto_register,
                "private_only": self.commands_private_only,
                "admin_user_ids_count": self.admin_user_ids.len()
            },
            "group_trigger": {
                "mode": self.group_trigger_settings.mode.as_str(),
                "followup_window_secs": self.group_trigger_settings.followup_window_secs,
                "cooldown_secs": self.group_trigger_settings.cooldown_secs,
                "rule_min_score": self.group_trigger_settings.rule_min_score,
                "aliases_count": learned_alias_total,
                "learned_alias_chats": learned_alias_chats,
                "learned_alias_total": learned_alias_total,
                "profiles_count": learned_profile_total,
                "learned_profile_chats": learned_profile_chats,
                "learned_profile_total": learned_profile_total,
                "llm_gate_enabled": self.group_trigger_settings.llm_gate_enabled
            },
            "scheduler": {
                "enabled": self.scheduler_settings.enabled,
                "tick_secs": self.scheduler_settings.tick_secs,
                "batch_size": self.scheduler_settings.batch_size,
                "lease_secs": self.scheduler_settings.lease_secs,
                "default_timezone": self.scheduler_settings.default_timezone,
                "nl_enabled": self.scheduler_settings.nl_enabled,
                "nl_min_confidence": self.scheduler_settings.nl_min_confidence,
                "require_confirm": self.scheduler_settings.require_confirm,
                "max_jobs_per_owner": self.scheduler_settings.max_jobs_per_owner,
                "diag": scheduler
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
    bot_user_id: Option<i64>,
    command_settings: TelegramCommandSettings,
    group_trigger_settings: TelegramGroupTriggerSettings,
    group_runtime_state: Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
    diag_state: Arc<tokio::sync::Mutex<TelegramPollingDiagState>>,
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
        let replied_to_bot =
            is_reply_to_bot_message(message.reply_to_message.as_ref(), bot_user_id);
        let is_group = is_group_chat_type(message.chat.kind.as_deref());
        let is_private_chat = is_private_chat_type(message.chat.kind.as_deref());
        let from_id = message.from.as_ref().map(|u| u.id);
        let from_is_bot = message
            .from
            .as_ref()
            .and_then(|u| u.is_bot)
            .unwrap_or(false)
            || bot_user_id
                .zip(from_id)
                .map(|(bot_id, uid)| bot_id == uid)
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
            bot_user_id = ?bot_user_id,
            from_id = ?from_id,
            message_thread_id = ?message_thread_id,
            reply_to_message_id = ?reply_to_message_id,
            replied_to_bot,
            mention_entity_count,
            text_len = text.chars().count(),
            text_preview = %text_preview,
            "telegram inbound message"
        );
        if is_group {
            let mut diag = diag_state.lock().await;
            diag.group_messages_total += 1;
        }
        if is_group && from_is_bot {
            let mut diag = diag_state.lock().await;
            diag.group_decision_ignore_total += 1;
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

        if is_private_chat {
            match evaluate_private_chat_access(
                message.from.as_ref().map(|u| u.id),
                &command_settings.admin_user_ids,
            ) {
                PrivateChatAccess::Allowed => {}
                PrivateChatAccess::MissingAdminAllowlist => {
                    send_telegram_command_reply(
                        sender,
                        &message,
                        &format!(
                            "管理员白名单未配置，私聊功能未启用。\n{}\n\n请在 .env.realtest 中配置 TELEGRAM_ADMIN_USER_IDS，然后重启服务。",
                            telegram_whoami_hint(&message)
                        ),
                    )
                    .await
                    .context("failed to send telegram private-chat deny message")?;
                    return Ok(());
                }
                PrivateChatAccess::Unauthorized => {
                    send_telegram_command_reply(
                        sender,
                        &message,
                        &format!(
                            "无权限使用该机器人。\n私聊仅开放给管理员。\n{}\n\n如需开通，请让管理员把你的 ID 加入 TELEGRAM_ADMIN_USER_IDS。",
                            telegram_whoami_hint(&message)
                        ),
                    )
                    .await
                    .context("failed to send telegram private-chat unauthorized message")?;
                    return Ok(());
                }
            }
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

        if is_private_chat
            && command_settings.scheduler.enabled
            && command_settings.scheduler.nl_enabled
            && maybe_handle_telegram_scheduler_natural_language(
                &ctx,
                sender,
                &message,
                text.trim(),
                &command_settings.scheduler,
            )
            .await?
        {
            return Ok(());
        }

        let mut model_input_text = text.to_string();
        if is_group {
            let (needs_alias_bootstrap, needs_profile_bootstrap) = {
                let state = group_runtime_state.lock().await;
                (
                    !state.loaded_aliases_chats.contains(&chat_id),
                    !state.loaded_profile_chats.contains(&chat_id),
                )
            };
            if needs_alias_bootstrap {
                match ctx
                    .service
                    .load_group_aliases(ctx.channel_name.clone(), chat_id, 64)
                    .await
                {
                    Ok(stored_aliases) => {
                        let mut state = group_runtime_state.lock().await;
                        let chat_aliases =
                            state.learned_aliases_by_chat.entry(chat_id).or_default();
                        for alias in stored_aliases {
                            let normalized = normalize_alias_token(&alias);
                            if normalized.is_empty() || chat_aliases.len() >= 24 {
                                continue;
                            }
                            chat_aliases.insert(normalized);
                        }
                        state.loaded_aliases_chats.insert(chat_id);
                    }
                    Err(err) => {
                        warn!(
                            chat_id,
                            error = %format!("{err:#}"),
                            "failed to bootstrap learned telegram group aliases"
                        );
                        let mut state = group_runtime_state.lock().await;
                        state.loaded_aliases_chats.insert(chat_id);
                    }
                }
            }
            if needs_profile_bootstrap {
                match ctx
                    .service
                    .load_group_user_profiles(ctx.channel_name.clone(), chat_id, 128)
                    .await
                {
                    Ok(stored_profiles) => {
                        let mut state = group_runtime_state.lock().await;
                        let chat_profiles = state.user_profiles_by_chat.entry(chat_id).or_default();
                        for profile in stored_profiles {
                            if profile.preferred_name.trim().is_empty()
                                || chat_profiles.len() >= 256
                            {
                                continue;
                            }
                            chat_profiles.insert(
                                profile.user_id,
                                TelegramGroupUserProfile {
                                    preferred_name: profile.preferred_name,
                                    username: normalize_telegram_username(
                                        profile.username.as_deref(),
                                    ),
                                    updated_at: profile.updated_at.max(0) as u64,
                                },
                            );
                        }
                        state.loaded_profile_chats.insert(chat_id);
                    }
                    Err(err) => {
                        warn!(
                            chat_id,
                            error = %format!("{err:#}"),
                            "failed to bootstrap telegram group user profiles"
                        );
                        let mut state = group_runtime_state.lock().await;
                        state.loaded_profile_chats.insert(chat_id);
                    }
                }
            }

            let sender_user_id = message.from.as_ref().map(|u| u.id).unwrap_or(chat_id);
            let sender_username = normalize_telegram_username(
                message.from.as_ref().and_then(|u| u.username.as_deref()),
            );
            let sender_observed_name = derive_telegram_user_display_name(message.from.as_ref())
                .unwrap_or_else(|| format!("用户{sender_user_id}"));
            let corrected_name = extract_realtime_name_correction(text);
            let now = current_unix_timestamp();

            let mut profile_persist_payload: Option<(i64, String, Option<String>)> = None;
            let (sender_profile, roster_entries) = {
                let mut state = group_runtime_state.lock().await;
                let chat_profiles = state.user_profiles_by_chat.entry(chat_id).or_default();
                let profile = chat_profiles.entry(sender_user_id).or_insert_with(|| {
                    TelegramGroupUserProfile {
                        preferred_name: sender_observed_name.clone(),
                        username: sender_username.clone(),
                        updated_at: now,
                    }
                });

                let mut changed = false;
                if profile.preferred_name.trim().is_empty() {
                    profile.preferred_name = sender_observed_name.clone();
                    changed = true;
                }
                if profile.username.is_none() && sender_username.is_some() {
                    profile.username = sender_username.clone();
                    changed = true;
                }
                if let Some(name) = corrected_name.as_ref()
                    && profile.preferred_name != *name
                {
                    profile.preferred_name = name.clone();
                    changed = true;
                }
                profile.updated_at = now;
                if changed {
                    profile_persist_payload = Some((
                        sender_user_id,
                        profile.preferred_name.clone(),
                        profile.username.clone(),
                    ));
                }

                let sender_profile = profile.clone();
                let mut roster = chat_profiles
                    .iter()
                    .map(|(uid, p)| (*uid, p.clone()))
                    .collect::<Vec<_>>();
                roster.sort_by(|a, b| {
                    b.1.updated_at
                        .cmp(&a.1.updated_at)
                        .then_with(|| a.0.cmp(&b.0))
                });
                (sender_profile, roster)
            };

            if let Some((profile_user_id, preferred_name, profile_username)) =
                profile_persist_payload
                && let Err(err) = ctx
                    .service
                    .upsert_group_user_profile(
                        ctx.channel_name.clone(),
                        chat_id,
                        profile_user_id,
                        preferred_name.clone(),
                        profile_username.clone(),
                    )
                    .await
            {
                warn!(
                    chat_id,
                    user_id = profile_user_id,
                    preferred_name = %preferred_name,
                    username = ?profile_username,
                    error = %format!("{err:#}"),
                    "failed to persist telegram group user profile"
                );
            }
            if let Some(name) = corrected_name.as_ref() {
                info!(
                    chat_id,
                    message_id,
                    user_id = sender_user_id,
                    corrected_name = %name,
                    "telegram group sender preferred name corrected in real time"
                );
            }
            model_input_text = build_group_member_identity_context(
                text,
                sender_user_id,
                &sender_profile,
                &roster_entries,
                8,
            );

            let bot_username = group_mention_bot_username.as_deref().unwrap_or("");
            let mentioned = if bot_username.is_empty() {
                false
            } else {
                message_mentions_bot(&text, bot_username)
            };
            if (mentioned || replied_to_bot) && !bot_username.is_empty() {
                let learned = extract_dynamic_alias_candidates(text, bot_username);
                if !learned.is_empty() {
                    let mut newly_added = Vec::new();
                    let mut state = group_runtime_state.lock().await;
                    let chat_aliases = state.learned_aliases_by_chat.entry(chat_id).or_default();
                    for alias in learned {
                        if chat_aliases.len() >= 24 {
                            break;
                        }
                        if chat_aliases.insert(alias.clone()) {
                            newly_added.push(alias);
                        }
                    }
                    drop(state);
                    if !newly_added.is_empty() {
                        if let Err(err) = ctx
                            .service
                            .upsert_group_aliases(
                                ctx.channel_name.clone(),
                                chat_id,
                                newly_added.clone(),
                            )
                            .await
                        {
                            warn!(
                                chat_id,
                                aliases = ?newly_added,
                                error = %format!("{err:#}"),
                                "failed to persist learned telegram group aliases"
                            );
                        }
                    }
                }
            }
            let (
                recent_bot_participation,
                cooldown_active,
                seconds_since_last_bot_response,
                learned_aliases,
            ) = {
                let state = group_runtime_state.lock().await;
                let elapsed = state
                    .last_bot_response_unix_by_chat
                    .get(&chat_id)
                    .map(|last| now.saturating_sub(*last));
                let recent = elapsed
                    .map(|secs| secs <= group_trigger_settings.followup_window_secs)
                    .unwrap_or(false);
                let cooldown = elapsed
                    .map(|secs| secs <= group_trigger_settings.cooldown_secs)
                    .unwrap_or(false);
                let aliases = state
                    .learned_aliases_by_chat
                    .get(&chat_id)
                    .map(|items| items.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                (recent, cooldown, elapsed, aliases)
            };
            let alias_hit = message_contains_any_alias(text, &learned_aliases);
            let has_question_marker = message_has_question_marker(text);
            let points_to_other_bot = if bot_username.is_empty() {
                false
            } else {
                message_mentions_other_handle(text, bot_username)
            };
            let low_signal_noise = is_low_signal_group_noise(text);
            let decision = evaluate_group_decision(
                group_trigger_settings.mode,
                &GroupSignalInput {
                    mentioned,
                    replied_to_bot,
                    recent_bot_participation,
                    alias_hit,
                    has_question_marker,
                    points_to_other_bot,
                    low_signal_noise,
                    cooldown_active,
                },
                group_trigger_settings.rule_min_score,
            );
            {
                let mut diag = diag_state.lock().await;
                match decision.kind {
                    GroupDecisionKind::Respond => diag.group_decision_respond_total += 1,
                    GroupDecisionKind::ObserveOnly => diag.group_decision_observe_total += 1,
                    GroupDecisionKind::Ignore => diag.group_decision_ignore_total += 1,
                }
            }
            debug!(
                chat_id,
                message_id = message.message_id,
                bot_username = %bot_username,
                mentioned,
                replied_to_bot,
                alias_hit,
                aliases_count = learned_aliases.len(),
                has_question_marker,
                recent_bot_participation,
                cooldown_active,
                seconds_since_last_bot_response = ?seconds_since_last_bot_response,
                decision = %decision.kind.as_str(),
                decision_score = decision.score,
                reasons = ?decision.reasons,
                text_len = text.chars().count(),
                sender_user_id,
                sender_preferred_name = %sender_profile.preferred_name,
                "telegram group mention check"
            );
            info!(
                chat_id,
                message_id,
                bot_username = %bot_username,
                mentioned,
                replied_to_bot,
                alias_hit,
                aliases_count = learned_aliases.len(),
                has_question_marker,
                recent_bot_participation,
                cooldown_active,
                seconds_since_last_bot_response = ?seconds_since_last_bot_response,
                decision = %decision.kind.as_str(),
                decision_score = decision.score,
                reasons = ?decision.reasons,
                mention_entity_count,
                sender_user_id,
                sender_preferred_name = %sender_profile.preferred_name,
                "telegram group trigger decision"
            );
            match decision.kind {
                GroupDecisionKind::Respond => {}
                GroupDecisionKind::ObserveOnly => {
                    let user_id = message
                        .from
                        .as_ref()
                        .map(|u| u.id.to_string())
                        .unwrap_or_else(|| chat_id.to_string());
                    let observe_session_id = format!(
                        "{}:observe",
                        telegram_session_id(chat_id, message_thread_id, reply_to_message_id)
                    );
                    ctx.service
                        .observe(IncomingMessage {
                            channel: ctx.channel_name.clone(),
                            session_id: observe_session_id.clone(),
                            user_id,
                            text: model_input_text.clone(),
                            reply_target: None,
                        })
                        .await
                        .with_context(|| {
                            format!(
                                "failed to persist telegram observe-only message (session={observe_session_id})"
                            )
                        })?;
                    info!(
                        chat_id,
                        message_id,
                        observe_session_id = %observe_session_id,
                        "telegram group message observed silently: decision=observe_only"
                    );
                    return Ok(());
                }
                GroupDecisionKind::Ignore => {
                    if bot_username.is_empty() {
                        info!(
                            chat_id,
                            message_id,
                            "telegram group message skipped: bot username unavailable and message is not a reply to bot"
                        );
                    }
                    info!(
                        chat_id,
                        message_id, "telegram group message skipped by trigger decision"
                    );
                    return Ok(());
                }
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
            text: model_input_text,
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
        if is_group {
            let mut state = group_runtime_state.lock().await;
            state
                .last_bot_response_unix_by_chat
                .insert(chat_id, current_unix_timestamp());
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramSlashCommand {
    Start,
    Help,
    WhoAmI,
    Mcp { tail: String },
    Task { tail: String },
    Unknown { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpCommandAccess {
    Allowed,
    RequirePrivateChat,
    MissingAdminAllowlist,
    Unauthorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateChatAccess {
    Allowed,
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
        TelegramSlashCommand::Task { tail } => {
            handle_telegram_task_command(
                ctx,
                sender,
                message,
                tail.as_str(),
                is_private_chat,
                command_settings,
            )
            .await?;
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

async fn handle_telegram_task_command(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    tail: &str,
    is_private_chat: bool,
    command_settings: &TelegramCommandSettings,
) -> anyhow::Result<()> {
    if !command_settings.scheduler.enabled {
        send_telegram_command_reply(sender, message, "定时任务功能未启用。").await?;
        return Ok(());
    }

    if !is_private_chat {
        send_telegram_command_reply(sender, message, "请私聊使用 /task 定时任务命令。").await?;
        return Ok(());
    }

    match evaluate_private_chat_access(
        message.from.as_ref().map(|u| u.id),
        &command_settings.admin_user_ids,
    ) {
        PrivateChatAccess::Allowed => {}
        PrivateChatAccess::MissingAdminAllowlist => {
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "管理员白名单未配置，定时任务不可用。\n{}\n\n请在 .env.realtest 中配置 TELEGRAM_ADMIN_USER_IDS。",
                    telegram_whoami_hint(message)
                ),
            )
            .await?;
            return Ok(());
        }
        PrivateChatAccess::Unauthorized => {
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "无权限管理定时任务。\n{}\n\n如需开通，请让管理员把你的 ID 加入 TELEGRAM_ADMIN_USER_IDS。",
                    telegram_whoami_hint(message)
                ),
            )
            .await?;
            return Ok(());
        }
    }

    let owner_user_id = message
        .from
        .as_ref()
        .map(|u| u.id)
        .unwrap_or(message.chat.id);
    let chat_id = message.chat.id;
    let tail = tail.trim();
    if tail.is_empty() {
        send_telegram_command_reply(
            sender,
            message,
            &telegram_task_help_text(&command_settings.scheduler.default_timezone),
        )
        .await?;
        return Ok(());
    }

    let mut tokens = tail.split_whitespace();
    let action = tokens
        .next()
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let rest = tail[action.len()..].trim();

    match action.as_str() {
        "list" => {
            let jobs = ctx
                .service
                .list_telegram_scheduler_jobs_by_owner(
                    ctx.channel_name.clone(),
                    chat_id,
                    owner_user_id,
                    64,
                )
                .await?;
            let text = telegram_task_list_text(&jobs, &command_settings.scheduler.default_timezone);
            send_telegram_command_reply(sender, message, &text).await?;
        }
        "add" => {
            let (when_raw, payload) = parse_task_schedule_and_payload(rest)?;
            let run_at_unix = parse_scheduler_once_time(
                when_raw.as_str(),
                &command_settings.scheduler.default_timezone,
            )?;
            let now = current_unix_timestamp_i64();
            if run_at_unix <= now {
                send_telegram_command_reply(sender, message, "目标时间已过，请设置未来时间。")
                    .await?;
                return Ok(());
            }
            let draft = TelegramSchedulerIntentDraft {
                task_kind: TelegramSchedulerTaskKind::Reminder,
                schedule_kind: TelegramSchedulerScheduleKind::Once,
                payload: payload.to_string(),
                timezone: command_settings.scheduler.default_timezone.clone(),
                run_at_unix: Some(run_at_unix),
                cron_expr: None,
                source_text: tail.to_string(),
            };
            let (job_id, next_run_at_unix) = create_scheduler_job_from_draft(
                ctx,
                chat_id,
                message.message_thread_id,
                owner_user_id,
                &command_settings.scheduler,
                &draft,
            )
            .await?;
            let time_text = format_scheduler_time(
                Some(next_run_at_unix),
                &command_settings.scheduler.default_timezone,
            );
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "已创建一次性任务。\nID: {job_id}\n执行时间: {time_text}\n内容: {}",
                    payload
                ),
            )
            .await?;
        }
        "every" => {
            let (cron_raw, payload) = parse_task_schedule_and_payload(rest)?;
            let draft = TelegramSchedulerIntentDraft {
                task_kind: TelegramSchedulerTaskKind::Reminder,
                schedule_kind: TelegramSchedulerScheduleKind::Cron,
                payload: payload.to_string(),
                timezone: command_settings.scheduler.default_timezone.clone(),
                run_at_unix: None,
                cron_expr: Some(cron_raw.to_string()),
                source_text: tail.to_string(),
            };
            let (job_id, next_run_at_unix) = create_scheduler_job_from_draft(
                ctx,
                chat_id,
                message.message_thread_id,
                owner_user_id,
                &command_settings.scheduler,
                &draft,
            )
            .await?;
            let next_time = format_scheduler_time(
                Some(next_run_at_unix),
                &command_settings.scheduler.default_timezone,
            );
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "已创建周期任务。\nID: {job_id}\nCron: {cron_raw}\n下次执行: {next_time}\n内容: {}",
                    payload
                ),
            )
            .await?;
        }
        "pause" | "resume" | "del" => {
            let job_id = rest.trim();
            if job_id.is_empty() {
                send_telegram_command_reply(
                    sender,
                    message,
                    "缺少任务 ID。\n示例: /task pause <job_id>",
                )
                .await?;
                return Ok(());
            }
            let status = match action.as_str() {
                "pause" => TelegramSchedulerJobStatus::Paused,
                "resume" => TelegramSchedulerJobStatus::Active,
                _ => TelegramSchedulerJobStatus::Canceled,
            };
            let updated = ctx
                .service
                .update_telegram_scheduler_job_status(
                    ctx.channel_name.clone(),
                    chat_id,
                    owner_user_id,
                    job_id.to_string(),
                    status,
                )
                .await?;
            let text = if updated {
                match action.as_str() {
                    "pause" => format!("任务已暂停: {job_id}"),
                    "resume" => format!("任务已恢复: {job_id}"),
                    _ => format!("任务已删除: {job_id}"),
                }
            } else {
                format!("未找到可更新的任务: {job_id}")
            };
            send_telegram_command_reply(sender, message, &text).await?;
        }
        _ => {
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "未知 task 子命令: {action}\n\n{}",
                    telegram_task_help_text(&command_settings.scheduler.default_timezone)
                ),
            )
            .await?;
        }
    }

    Ok(())
}

async fn maybe_handle_telegram_scheduler_natural_language(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    text: &str,
    scheduler_settings: &TelegramSchedulerSettings,
) -> anyhow::Result<bool> {
    let owner_user_id = message
        .from
        .as_ref()
        .map(|u| u.id)
        .unwrap_or(message.chat.id);
    let chat_id = message.chat.id;
    let now_unix = current_unix_timestamp_i64();
    let channel_name = ctx.channel_name.clone();

    let pending = ctx
        .service
        .load_telegram_scheduler_pending_intent(
            channel_name.clone(),
            chat_id,
            owner_user_id,
            now_unix,
        )
        .await?;

    if let Some(pending) = pending {
        if is_scheduler_confirm_text(text) {
            let draft: TelegramSchedulerIntentDraft =
                match serde_json::from_str(pending.draft_json.as_str()) {
                    Ok(v) => v,
                    Err(err) => {
                        let _ = ctx
                            .service
                            .delete_telegram_scheduler_pending_intent(
                                channel_name.clone(),
                                chat_id,
                                owner_user_id,
                            )
                            .await;
                        send_telegram_command_reply(
                            sender,
                            message,
                            &format!("待确认任务草案已损坏，已清理。请重新描述任务。\n错误: {err}"),
                        )
                        .await?;
                        return Ok(true);
                    }
                };

            match create_scheduler_job_from_draft(
                ctx,
                chat_id,
                message.message_thread_id,
                owner_user_id,
                scheduler_settings,
                &draft,
            )
            .await
            {
                Ok((job_id, next_run_at_unix)) => {
                    let _ = ctx
                        .service
                        .delete_telegram_scheduler_pending_intent(
                            channel_name.clone(),
                            chat_id,
                            owner_user_id,
                        )
                        .await;
                    let next_time = format_scheduler_time(Some(next_run_at_unix), &draft.timezone);
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!(
                            "已确认并创建任务。\nID: {job_id}\n类型: {}\n下次执行: {next_time}\n内容: {}",
                            draft.schedule_kind.as_str(),
                            draft.payload
                        ),
                    )
                    .await?;
                }
                Err(err) => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!("确认失败: {err}\n你可以修改后重试，或回复“取消”丢弃草案。"),
                    )
                    .await?;
                }
            }
            return Ok(true);
        }

        if is_scheduler_cancel_text(text) {
            let _ = ctx
                .service
                .delete_telegram_scheduler_pending_intent(
                    channel_name.clone(),
                    chat_id,
                    owner_user_id,
                )
                .await;
            send_telegram_command_reply(sender, message, "已取消当前待确认定时任务。").await?;
            return Ok(true);
        }

        if try_handle_scheduler_management_text(
            ctx,
            sender,
            message,
            text,
            scheduler_settings,
            chat_id,
            owner_user_id,
        )
        .await?
        {
            return Ok(true);
        }

        if let Some(intent) = ctx
            .service
            .detect_telegram_scheduler_intent(
                text,
                scheduler_settings.default_timezone.as_str(),
                now_unix,
                Some(pending.draft_json.as_str()),
            )
            .await?
            && intent.confidence >= scheduler_settings.nl_min_confidence
            && matches!(intent.action.as_str(), "create" | "update")
            && let Some(updated_draft) = build_scheduler_draft_from_intent(
                &intent,
                text,
                scheduler_settings.default_timezone.as_str(),
                now_unix,
            )?
        {
            upsert_scheduler_pending_intent(
                ctx,
                chat_id,
                owner_user_id,
                &updated_draft,
                now_unix + TELEGRAM_SCHEDULER_PENDING_TTL_SECS,
            )
            .await?;
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "已更新任务草案:\n{}\n\n回复“确认”创建任务，回复“取消”放弃草案。",
                    describe_scheduler_draft(&updated_draft)
                ),
            )
            .await?;
            return Ok(true);
        }
    }

    if try_handle_scheduler_management_text(
        ctx,
        sender,
        message,
        text,
        scheduler_settings,
        chat_id,
        owner_user_id,
    )
    .await?
    {
        return Ok(true);
    }

    let Some(intent) = ctx
        .service
        .detect_telegram_scheduler_intent(
            text,
            scheduler_settings.default_timezone.as_str(),
            now_unix,
            None,
        )
        .await?
    else {
        return Ok(false);
    };

    if intent.confidence < scheduler_settings.nl_min_confidence {
        return Ok(false);
    }

    match intent.action.as_str() {
        "list" => {
            let jobs = ctx
                .service
                .list_telegram_scheduler_jobs_by_owner(channel_name, chat_id, owner_user_id, 64)
                .await?;
            let text = telegram_task_list_text(&jobs, &scheduler_settings.default_timezone);
            send_telegram_command_reply(sender, message, &text).await?;
            Ok(true)
        }
        "delete" | "pause" | "resume" | "cancel" => {
            let op = resolve_scheduler_job_operation_from_intent(&intent).or_else(|| {
                if intent.action == "cancel" {
                    Some(SchedulerJobOperation::Delete)
                } else {
                    None
                }
            });
            let Some(op) = op else {
                return Ok(false);
            };
            let explicit_job_id = intent
                .job_id
                .as_deref()
                .and_then(normalize_scheduler_job_id)
                .or_else(|| extract_scheduler_job_id(text));
            let resolved = resolve_scheduler_job_target(
                ctx,
                chat_id,
                owner_user_id,
                op,
                explicit_job_id,
                text,
            )
            .await?;
            execute_scheduler_job_operation_with_resolution(
                ctx,
                sender,
                message,
                scheduler_settings,
                chat_id,
                owner_user_id,
                op,
                resolved,
            )
            .await?;
            Ok(true)
        }
        "create" | "update" => {
            let Some(draft) = build_scheduler_draft_from_intent(
                &intent,
                text,
                scheduler_settings.default_timezone.as_str(),
                now_unix,
            )?
            else {
                return Ok(false);
            };

            if scheduler_settings.require_confirm {
                upsert_scheduler_pending_intent(
                    ctx,
                    chat_id,
                    owner_user_id,
                    &draft,
                    now_unix + TELEGRAM_SCHEDULER_PENDING_TTL_SECS,
                )
                .await?;
                send_telegram_command_reply(
                    sender,
                    message,
                    &format!(
                        "识别到定时任务草案:\n{}\n\n回复“确认”创建任务，回复“取消”放弃草案。",
                        describe_scheduler_draft(&draft)
                    ),
                )
                .await?;
                Ok(true)
            } else {
                let (job_id, next_run_at_unix) = create_scheduler_job_from_draft(
                    ctx,
                    chat_id,
                    message.message_thread_id,
                    owner_user_id,
                    scheduler_settings,
                    &draft,
                )
                .await?;
                send_telegram_command_reply(
                    sender,
                    message,
                    &format!(
                        "已创建任务。\nID: {job_id}\n下次执行: {}\n内容: {}",
                        format_scheduler_time(Some(next_run_at_unix), &draft.timezone),
                        draft.payload
                    ),
                )
                .await?;
                Ok(true)
            }
        }
        _ => Ok(false),
    }
}

async fn try_handle_scheduler_management_text(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    text: &str,
    scheduler_settings: &TelegramSchedulerSettings,
    chat_id: i64,
    owner_user_id: i64,
) -> anyhow::Result<bool> {
    if looks_like_scheduler_list_text(text) {
        let jobs = ctx
            .service
            .list_telegram_scheduler_jobs_by_owner(
                ctx.channel_name.clone(),
                chat_id,
                owner_user_id,
                64,
            )
            .await?;
        let text = telegram_task_list_text(&jobs, &scheduler_settings.default_timezone);
        send_telegram_command_reply(sender, message, &text).await?;
        return Ok(true);
    }

    let Some(op) = detect_scheduler_job_operation_keyword(text) else {
        return Ok(false);
    };
    let explicit_job_id = extract_scheduler_job_id(text);
    let resolved =
        resolve_scheduler_job_target(ctx, chat_id, owner_user_id, op, explicit_job_id, text)
            .await?;
    execute_scheduler_job_operation_with_resolution(
        ctx,
        sender,
        message,
        scheduler_settings,
        chat_id,
        owner_user_id,
        op,
        resolved,
    )
    .await?;
    Ok(true)
}

async fn resolve_scheduler_job_target(
    ctx: &ChannelContext,
    chat_id: i64,
    owner_user_id: i64,
    op: SchedulerJobOperation,
    explicit_job_id: Option<String>,
    raw_text: &str,
) -> anyhow::Result<SchedulerJobTargetResolution> {
    if let Some(job_id) = explicit_job_id {
        return Ok(SchedulerJobTargetResolution::Selected {
            job_id,
            inferred: false,
        });
    }

    let jobs = ctx
        .service
        .list_telegram_scheduler_jobs_by_owner(ctx.channel_name.clone(), chat_id, owner_user_id, 64)
        .await?;

    let mut candidates = jobs
        .into_iter()
        .filter(|job| match op {
            SchedulerJobOperation::Pause => job.status == TelegramSchedulerJobStatus::Active,
            SchedulerJobOperation::Resume => job.status == TelegramSchedulerJobStatus::Paused,
            SchedulerJobOperation::Delete => job.status != TelegramSchedulerJobStatus::Canceled,
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return Ok(SchedulerJobTargetResolution::Empty);
    }
    if candidates.len() == 1 {
        return Ok(SchedulerJobTargetResolution::Selected {
            job_id: candidates.remove(0).job_id,
            inferred: true,
        });
    }

    if text_implies_latest_target(raw_text)
        || matches!(
            op,
            SchedulerJobOperation::Pause | SchedulerJobOperation::Resume
        )
    {
        return Ok(SchedulerJobTargetResolution::Selected {
            job_id: candidates.remove(0).job_id,
            inferred: true,
        });
    }

    Ok(SchedulerJobTargetResolution::Ambiguous {
        options: candidates.into_iter().take(5).collect(),
    })
}

async fn execute_scheduler_job_operation_with_resolution(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    scheduler_settings: &TelegramSchedulerSettings,
    chat_id: i64,
    owner_user_id: i64,
    op: SchedulerJobOperation,
    resolved: SchedulerJobTargetResolution,
) -> anyhow::Result<()> {
    match resolved {
        SchedulerJobTargetResolution::Empty => {
            send_telegram_command_reply(
                sender,
                message,
                "没有找到可操作的任务。可先发送 `/task list` 查看。",
            )
            .await?;
        }
        SchedulerJobTargetResolution::Ambiguous { options } => {
            let hint = format_scheduler_operation_hint(op);
            let choices = format_scheduler_operation_candidates(
                &options,
                &scheduler_settings.default_timezone,
            );
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "匹配到多个任务，暂未自动执行 {}。\n{}\n\n请补充任务 ID（例如：`{} task-...`）。",
                    hint,
                    choices,
                    hint
                ),
            )
            .await?;
        }
        SchedulerJobTargetResolution::Selected { job_id, inferred } => {
            execute_scheduler_job_operation(
                ctx,
                sender,
                message,
                chat_id,
                owner_user_id,
                op,
                &job_id,
                inferred,
            )
            .await?;
        }
    }
    Ok(())
}

async fn execute_scheduler_job_operation(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    chat_id: i64,
    owner_user_id: i64,
    op: SchedulerJobOperation,
    job_id: &str,
    inferred: bool,
) -> anyhow::Result<()> {
    let status = match op {
        SchedulerJobOperation::Pause => TelegramSchedulerJobStatus::Paused,
        SchedulerJobOperation::Resume => TelegramSchedulerJobStatus::Active,
        SchedulerJobOperation::Delete => TelegramSchedulerJobStatus::Canceled,
    };

    let text_prefix = match op {
        SchedulerJobOperation::Pause => "任务已暂停",
        SchedulerJobOperation::Resume => "任务已恢复",
        SchedulerJobOperation::Delete => "任务已删除",
    };

    let updated = ctx
        .service
        .update_telegram_scheduler_job_status(
            ctx.channel_name.clone(),
            chat_id,
            owner_user_id,
            job_id.to_string(),
            status,
        )
        .await?;

    let text = if updated {
        if inferred {
            format!("{text_prefix}: {job_id}\n已自动定位目标任务。")
        } else {
            format!("{text_prefix}: {job_id}")
        }
    } else {
        format!("未找到可更新的任务: {job_id}")
    };
    send_telegram_command_reply(sender, message, &text).await?;
    Ok(())
}

async fn upsert_scheduler_pending_intent(
    ctx: &ChannelContext,
    chat_id: i64,
    owner_user_id: i64,
    draft: &TelegramSchedulerIntentDraft,
    expires_at_unix: i64,
) -> anyhow::Result<()> {
    let intent_id = format!("pending:{}:{}:{}", ctx.channel_name, chat_id, owner_user_id);
    let draft_json = serde_json::to_string(draft).context("failed to encode scheduler draft")?;
    ctx.service
        .upsert_telegram_scheduler_pending_intent(UpsertTelegramSchedulerPendingIntentRequest {
            intent_id,
            channel: ctx.channel_name.clone(),
            chat_id,
            owner_user_id,
            draft_json,
            expires_at_unix,
        })
        .await
}

fn describe_scheduler_draft(draft: &TelegramSchedulerIntentDraft) -> String {
    let schedule = match draft.schedule_kind {
        TelegramSchedulerScheduleKind::Once => {
            format!(
                "一次性 @ {}",
                format_scheduler_time(draft.run_at_unix, &draft.timezone)
            )
        }
        TelegramSchedulerScheduleKind::Cron => format!(
            "周期 cron({}) next @ {}",
            draft.cron_expr.clone().unwrap_or_else(|| "-".to_string()),
            format_scheduler_time(draft.run_at_unix, &draft.timezone)
        ),
    };
    format!(
        "类型: {}\n时区: {}\n内容: {}\n原始: {}",
        schedule, draft.timezone, draft.payload, draft.source_text
    )
}

fn is_scheduler_confirm_text(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "确认" | "确定" | "yes" | "y" | "ok" | "okay" | "confirm"
    )
}

fn is_scheduler_cancel_text(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "取消" | "不用了" | "算了" | "no" | "n" | "cancel" | "stop"
    )
}

fn looks_like_scheduler_list_text(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let zh_hit = ["任务列表", "查看任务", "列出任务", "有哪些任务", "看看任务"]
        .iter()
        .any(|k| text.contains(k));
    if zh_hit {
        return true;
    }
    let en_hit = ["list task", "list tasks", "show tasks", "show task"]
        .iter()
        .any(|k| lower.contains(k));
    en_hit
}

fn detect_scheduler_job_operation_keyword(text: &str) -> Option<SchedulerJobOperation> {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let pause_hit = ["暂停任务", "停用任务", "pause task", "pause任务"]
        .iter()
        .any(|k| text.contains(k) || lower.contains(k));
    if pause_hit {
        return Some(SchedulerJobOperation::Pause);
    }
    let resume_hit = [
        "恢复任务",
        "继续任务",
        "启用任务",
        "resume task",
        "start task",
    ]
    .iter()
    .any(|k| text.contains(k) || lower.contains(k));
    if resume_hit {
        return Some(SchedulerJobOperation::Resume);
    }
    let delete_hit = [
        "删除任务",
        "删掉任务",
        "取消任务",
        "delete task",
        "remove task",
        "cancel task",
    ]
    .iter()
    .any(|k| text.contains(k) || lower.contains(k));
    if delete_hit {
        return Some(SchedulerJobOperation::Delete);
    }
    None
}

fn extract_scheduler_job_id(text: &str) -> Option<String> {
    if let Some(found) = normalize_scheduler_job_id(text) {
        return Some(found);
    }
    for (start, _) in text.match_indices("task-") {
        let mut end = start;
        for (idx, ch) in text[start..].char_indices() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':') {
                end = start + idx + ch.len_utf8();
            } else {
                break;
            }
        }
        if end > start {
            let candidate = &text[start..end];
            if let Some(normalized) = normalize_scheduler_job_id(candidate) {
                return Some(normalized);
            }
        }
    }
    None
}

fn normalize_scheduler_job_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '"' | '\'' | ',' | '，' | '.' | '。' | ';' | '；' | ')' | '(' | '[' | ']'
            )
    });
    if !trimmed.starts_with("task-") || trimmed.len() <= 5 {
        return None;
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':'))
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn resolve_scheduler_job_operation_from_intent(
    intent: &TelegramSchedulerIntent,
) -> Option<SchedulerJobOperation> {
    if let Some(op) = intent.job_operation.as_deref() {
        match op {
            "pause" => return Some(SchedulerJobOperation::Pause),
            "resume" => return Some(SchedulerJobOperation::Resume),
            "delete" => return Some(SchedulerJobOperation::Delete),
            _ => {}
        }
    }
    match intent.action.as_str() {
        "pause" => Some(SchedulerJobOperation::Pause),
        "resume" => Some(SchedulerJobOperation::Resume),
        "delete" => Some(SchedulerJobOperation::Delete),
        _ => None,
    }
}

fn text_implies_latest_target(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    [
        "最新",
        "最近",
        "刚刚",
        "上一个",
        "上次",
        "latest",
        "recent",
        "last",
    ]
    .iter()
    .any(|k| text.contains(k) || lower.contains(k))
}

fn format_scheduler_operation_hint(op: SchedulerJobOperation) -> &'static str {
    match op {
        SchedulerJobOperation::Pause => "pause",
        SchedulerJobOperation::Resume => "resume",
        SchedulerJobOperation::Delete => "del",
    }
}

fn format_scheduler_operation_candidates(
    options: &[TelegramSchedulerJobRecord],
    timezone: &str,
) -> String {
    if options.is_empty() {
        return "-".to_string();
    }
    let mut lines = vec!["候选任务:".to_string()];
    for item in options.iter().take(5) {
        let schedule = match item.schedule_kind {
            TelegramSchedulerScheduleKind::Once => {
                format!(
                    "once@{}",
                    format_scheduler_time(item.next_run_at_unix, timezone)
                )
            }
            TelegramSchedulerScheduleKind::Cron => format!(
                "cron({}) next@{}",
                item.cron_expr.clone().unwrap_or_else(|| "-".to_string()),
                format_scheduler_time(item.next_run_at_unix, timezone)
            ),
        };
        lines.push(format!(
            "- {} [{}] {} | {}",
            item.job_id,
            item.status.as_str(),
            schedule,
            truncate_chars(item.payload.trim(), 60)
        ));
    }
    lines.join("\n")
}

fn build_scheduler_draft_from_intent(
    intent: &TelegramSchedulerIntent,
    source_text: &str,
    default_timezone: &str,
    now_unix: i64,
) -> anyhow::Result<Option<TelegramSchedulerIntentDraft>> {
    let schedule_kind = match intent
        .schedule_kind
        .as_deref()
        .map(|v| v.trim().to_ascii_lowercase())
    {
        Some(v) if v == "cron" => TelegramSchedulerScheduleKind::Cron,
        Some(v) if v == "once" => TelegramSchedulerScheduleKind::Once,
        _ => return Ok(None),
    };
    let task_kind = match intent
        .task_kind
        .as_deref()
        .map(|v| v.trim().to_ascii_lowercase())
    {
        Some(v) if v == "agent" => TelegramSchedulerTaskKind::Agent,
        _ => TelegramSchedulerTaskKind::Reminder,
    };
    let payload = match intent.payload.as_deref().map(str::trim) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => return Ok(None),
    };
    let timezone = intent
        .timezone
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(default_timezone)
        .to_string();

    let (run_at_unix, cron_expr) = match schedule_kind {
        TelegramSchedulerScheduleKind::Once => {
            let Some(run_at_raw) = intent.run_at.as_deref() else {
                return Ok(None);
            };
            let run_at_unix = parse_scheduler_once_time(run_at_raw, timezone.as_str())?;
            if run_at_unix <= now_unix {
                return Ok(None);
            }
            (Some(run_at_unix), None)
        }
        TelegramSchedulerScheduleKind::Cron => {
            let Some(expr) = intent.cron_expr.as_deref().map(str::trim) else {
                return Ok(None);
            };
            if expr.is_empty() {
                return Ok(None);
            }
            let next = compute_next_run_at_unix(
                &ScheduleSpec::Cron {
                    expr: expr.to_string(),
                },
                now_unix,
                timezone.as_str(),
            )?;
            let Some(next_run) = next else {
                return Ok(None);
            };
            (Some(next_run), Some(expr.to_string()))
        }
    };

    Ok(Some(TelegramSchedulerIntentDraft {
        task_kind,
        schedule_kind,
        payload,
        timezone,
        run_at_unix,
        cron_expr,
        source_text: source_text.trim().to_string(),
    }))
}

async fn create_scheduler_job_from_draft(
    ctx: &ChannelContext,
    chat_id: i64,
    message_thread_id: Option<i64>,
    owner_user_id: i64,
    scheduler_settings: &TelegramSchedulerSettings,
    draft: &TelegramSchedulerIntentDraft,
) -> anyhow::Result<(String, i64)> {
    enforce_scheduler_job_quota(
        ctx,
        chat_id,
        owner_user_id,
        scheduler_settings.max_jobs_per_owner,
    )
    .await?;

    let next_run_at_unix = match draft.schedule_kind {
        TelegramSchedulerScheduleKind::Once => {
            draft.run_at_unix.context("once draft missing run_at")?
        }
        TelegramSchedulerScheduleKind::Cron => {
            let expr = draft
                .cron_expr
                .clone()
                .context("cron draft missing cron_expr")?;
            compute_next_run_at_unix(
                &ScheduleSpec::Cron { expr },
                current_unix_timestamp_i64(),
                draft.timezone.as_str(),
            )?
            .context("failed to compute next run time for cron draft")?
        }
    };
    let job_id = build_scheduler_job_id(chat_id, owner_user_id);

    ctx.service
        .create_telegram_scheduler_job(CreateTelegramSchedulerJobRequest {
            job_id: job_id.clone(),
            channel: ctx.channel_name.clone(),
            chat_id,
            message_thread_id,
            owner_user_id,
            status: TelegramSchedulerJobStatus::Active,
            task_kind: draft.task_kind,
            payload: draft.payload.clone(),
            schedule_kind: draft.schedule_kind,
            timezone: draft.timezone.clone(),
            run_at_unix: if matches!(draft.schedule_kind, TelegramSchedulerScheduleKind::Once) {
                Some(next_run_at_unix)
            } else {
                None
            },
            cron_expr: draft.cron_expr.clone(),
            next_run_at_unix: Some(next_run_at_unix),
            max_runs: if matches!(draft.schedule_kind, TelegramSchedulerScheduleKind::Once) {
                Some(1)
            } else {
                None
            },
        })
        .await?;

    Ok((job_id, next_run_at_unix))
}

async fn enforce_scheduler_job_quota(
    ctx: &ChannelContext,
    chat_id: i64,
    owner_user_id: i64,
    max_jobs_per_owner: usize,
) -> anyhow::Result<()> {
    let jobs = ctx
        .service
        .list_telegram_scheduler_jobs_by_owner(
            ctx.channel_name.clone(),
            chat_id,
            owner_user_id,
            max_jobs_per_owner.saturating_add(1),
        )
        .await?;
    let used = jobs
        .iter()
        .filter(|job| {
            matches!(
                job.status,
                TelegramSchedulerJobStatus::Active | TelegramSchedulerJobStatus::Paused
            )
        })
        .count();
    if used >= max_jobs_per_owner {
        bail!("任务数量已达上限({max_jobs_per_owner})，请先删除/暂停部分任务后再创建。");
    }
    Ok(())
}

fn parse_task_schedule_and_payload(raw: &str) -> anyhow::Result<(String, String)> {
    let (left, right) = raw
        .split_once('|')
        .context("格式错误，缺少 `|` 分隔符。示例: /task add 2026-03-01 09:00 | 提醒我开会")?;
    let schedule = left.trim();
    let payload = right.trim();
    if schedule.is_empty() || payload.is_empty() {
        bail!("格式错误，时间和内容都不能为空。");
    }
    Ok((schedule.to_string(), payload.to_string()))
}

fn parse_scheduler_once_time(raw: &str, default_timezone: &str) -> anyhow::Result<i64> {
    let input = raw.trim();
    if input.is_empty() {
        bail!("缺少执行时间。");
    }
    if let Ok(unix) = input.parse::<i64>() {
        return Ok(unix);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Ok(dt.timestamp());
    }

    let timezone = default_timezone
        .parse::<Tz>()
        .with_context(|| format!("invalid timezone '{default_timezone}'"))?;
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
    ] {
        if let Ok(local_naive) = NaiveDateTime::parse_from_str(input, fmt)
            && let Some(local_dt) = timezone.from_local_datetime(&local_naive).single()
        {
            return Ok(local_dt.with_timezone(&Utc).timestamp());
        }
    }

    bail!(
        "不支持的时间格式。支持: Unix 秒时间戳 / RFC3339 / YYYY-MM-DD HH:MM（默认时区 {}）",
        default_timezone
    );
}

fn build_scheduler_job_id(chat_id: i64, owner_user_id: i64) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("task-{chat_id}-{owner_user_id}-{millis}")
}

fn format_scheduler_time(unix: Option<i64>, timezone: &str) -> String {
    let Some(unix) = unix else {
        return "-".to_string();
    };
    let tz: Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
    match tz.timestamp_opt(unix, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
        None => unix.to_string(),
    }
}

fn telegram_task_list_text(jobs: &[TelegramSchedulerJobRecord], timezone: &str) -> String {
    if jobs.is_empty() {
        return "当前没有定时任务。".to_string();
    }
    let mut lines = vec!["你的定时任务:".to_string()];
    for job in jobs.iter().take(64) {
        let schedule_text = match job.schedule_kind {
            TelegramSchedulerScheduleKind::Once => {
                format!(
                    "once@{}",
                    format_scheduler_time(job.next_run_at_unix, timezone)
                )
            }
            TelegramSchedulerScheduleKind::Cron => {
                format!(
                    "cron({}) next@{}",
                    job.cron_expr.clone().unwrap_or_else(|| "-".to_string()),
                    format_scheduler_time(job.next_run_at_unix, timezone)
                )
            }
        };
        lines.push(format!(
            "- {} [{}] {} | {}",
            job.job_id,
            job.status.as_str(),
            schedule_text,
            truncate_chars(job.payload.trim(), 80)
        ));
    }
    lines.join("\n")
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

fn evaluate_private_chat_access(
    caller_user_id: Option<i64>,
    admin_user_ids: &[i64],
) -> PrivateChatAccess {
    if admin_user_ids.is_empty() {
        return PrivateChatAccess::MissingAdminAllowlist;
    }
    if caller_user_id.is_none_or(|id| !admin_user_ids.contains(&id)) {
        return PrivateChatAccess::Unauthorized;
    }
    PrivateChatAccess::Allowed
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
        "task" => Some(TelegramSlashCommand::Task { tail }),
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
        "/task - 管理定时任务（仅私聊管理员）",
    ]
    .join("\n")
}

fn telegram_task_help_text(default_timezone: &str) -> String {
    [
        "定时任务命令:",
        "/task list",
        "/task add <时间> | <内容>",
        "/task every <cron> | <内容>",
        "/task pause <job_id>",
        "/task resume <job_id>",
        "/task del <job_id>",
        "",
        "时间支持:",
        "- Unix 秒时间戳",
        "- RFC3339，例如 2026-03-01T09:00:00+08:00",
        &format!("- YYYY-MM-DD HH:MM（默认时区 {default_timezone}）"),
        "",
        "示例:",
        "/task add 2026-03-01 09:00 | 提醒我参加晨会",
        "/task every 0 9 * * * | 每天提醒我写日报",
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
        ("task", "管理定时任务"),
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

fn message_contains_any_alias(text: &str, aliases: &[String]) -> bool {
    if aliases.is_empty() {
        return false;
    }
    let lowered = text.to_lowercase();
    aliases.iter().any(|alias| {
        let trimmed = alias.trim().to_lowercase();
        if trimmed.is_empty() {
            return false;
        }
        lowered.contains(&trimmed)
    })
}

fn message_has_question_marker(text: &str) -> bool {
    text.contains('?') || text.contains('？')
}

fn message_mentions_other_handle(text: &str, bot_username: &str) -> bool {
    let own = bot_username.trim().trim_start_matches('@').to_lowercase();
    if own.is_empty() {
        return false;
    }
    text.split(|ch: char| ch.is_whitespace() || ",.!?，。！？;:()[]{}<>\"'".contains(ch))
        .filter_map(|token| token.strip_prefix('@'))
        .map(|name| {
            name.trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .to_lowercase()
        })
        .any(|name| !name.is_empty() && name != own)
}

fn is_low_signal_group_noise(text: &str) -> bool {
    let compact = text.trim();
    if compact.is_empty() {
        return true;
    }
    let len = compact.chars().count();
    len <= 2
}

fn evaluate_group_decision(
    mode: TelegramGroupTriggerMode,
    input: &GroupSignalInput,
    min_score: u32,
) -> GroupDecision {
    if input.mentioned {
        return GroupDecision {
            kind: GroupDecisionKind::Respond,
            score: 100,
            reasons: vec!["explicit_mention"],
        };
    }
    if input.replied_to_bot {
        return GroupDecision {
            kind: GroupDecisionKind::Respond,
            score: 100,
            reasons: vec!["reply_to_bot"],
        };
    }
    if matches!(mode, TelegramGroupTriggerMode::Strict) {
        return GroupDecision {
            kind: GroupDecisionKind::Ignore,
            score: 0,
            reasons: vec!["strict_no_explicit_trigger"],
        };
    }

    let mut score: i32 = 0;
    let mut reasons = Vec::new();
    if input.recent_bot_participation {
        score += 30;
        reasons.push("recent_bot_participation");
    }
    if input.alias_hit {
        score += 25;
        reasons.push("alias_hit");
    }
    if input.has_question_marker {
        score += 20;
        reasons.push("question_marker");
    }
    if input.points_to_other_bot {
        score -= 35;
        reasons.push("points_to_other_bot");
    }
    if input.low_signal_noise {
        score -= 25;
        reasons.push("low_signal_noise");
    }

    let min = min_score.clamp(1, 100) as i32;
    let observe_min = (min / 2).max(30);
    let mut kind = if score >= min {
        GroupDecisionKind::Respond
    } else if score >= observe_min {
        GroupDecisionKind::ObserveOnly
    } else {
        GroupDecisionKind::Ignore
    };
    if input.cooldown_active && matches!(kind, GroupDecisionKind::Respond) {
        kind = GroupDecisionKind::ObserveOnly;
        reasons.push("cooldown_active");
    }

    GroupDecision {
        kind,
        score,
        reasons,
    }
}

fn is_group_chat_type(kind: Option<&str>) -> bool {
    matches!(
        kind.map(|v| v.trim().to_ascii_lowercase()),
        Some(v) if v == "group" || v == "supergroup"
    )
}

fn is_reply_to_bot_message(reply: Option<&TelegramReplyMessage>, bot_user_id: Option<i64>) -> bool {
    reply
        .and_then(|msg| msg.from.as_ref())
        .map(|from| {
            from.is_bot.unwrap_or(false)
                || bot_user_id.map(|bot_id| bot_id == from.id).unwrap_or(false)
        })
        .unwrap_or(false)
}

fn normalize_telegram_username(raw: Option<&str>) -> Option<String> {
    raw.map(|v| v.trim().trim_start_matches('@').to_string())
        .filter(|v| !v.is_empty())
}

fn normalize_display_name(raw: &str) -> Option<String> {
    let normalized = raw
        .trim()
        .trim_start_matches('@')
        .trim_matches(|ch: char| ",，。.!！？?;；:：()[]{}<>\"'`~|/\\ ".contains(ch))
        .trim();
    if normalized.is_empty() {
        return None;
    }
    let len = normalized.chars().count();
    if !(1..=24).contains(&len) {
        return None;
    }
    if normalized.contains('\n') || normalized.contains('\r') || normalized.contains('\t') {
        return None;
    }
    let lowered = normalized.to_ascii_lowercase();
    if matches!(
        lowered.as_str(),
        "me" | "myself" | "bot" | "robot" | "assistant" | "question" | "reply" | "answer" | "sleep"
    ) {
        return None;
    }
    if matches!(
        normalized,
        "我" | "我们"
            | "你"
            | "你们"
            | "他"
            | "她"
            | "它"
            | "大家"
            | "群友"
            | "机器人"
            | "助手"
            | "问题"
            | "一下"
            | "起床"
            | "看看"
            | "帮忙"
            | "分析"
            | "解释"
            | "回复"
            | "回答"
            | "处理"
            | "来问"
            | "问下"
    ) {
        return None;
    }
    if [
        "来问", "请教", "问题", "帮忙", "看看", "分析", "解释", "回复", "回答", "处理",
    ]
    .iter()
    .any(|kw| normalized.contains(kw))
    {
        return None;
    }
    Some(normalized.to_string())
}

fn derive_telegram_user_display_name(user: Option<&TelegramUser>) -> Option<String> {
    let Some(user) = user else {
        return None;
    };

    let first = user.first_name.as_deref().unwrap_or_default().trim();
    let last = user.last_name.as_deref().unwrap_or_default().trim();
    if !first.is_empty() || !last.is_empty() {
        let full = if !first.is_empty() && !last.is_empty() {
            format!("{first} {last}")
        } else if !first.is_empty() {
            first.to_string()
        } else {
            last.to_string()
        };
        if let Some(name) = normalize_display_name(&full) {
            return Some(name);
        }
    }

    if let Some(username) = normalize_telegram_username(user.username.as_deref()) {
        return normalize_display_name(&username);
    }

    None
}

fn extract_name_candidate_from_tail(tail: &str) -> Option<String> {
    let trimmed = tail.trim_start_matches(|ch: char| {
        ch.is_whitespace() || "，,。.!！？?;；:：()[]{}<>\"'`~|/\\-=".contains(ch)
    });
    if trimmed.is_empty() {
        return None;
    }

    let end = trimmed
        .char_indices()
        .find_map(|(idx, ch)| {
            (ch.is_whitespace() || "，,。.!！？?;；:：()[]{}<>\"'`~|/\\=".contains(ch))
                .then_some(idx)
        })
        .unwrap_or(trimmed.len());
    normalize_display_name(&trimmed[..end])
}

fn extract_realtime_name_correction(text: &str) -> Option<String> {
    let compact = text.trim();
    if compact.is_empty() {
        return None;
    }

    let markers = [
        ("请叫我", true),
        ("叫我", true),
        ("我叫", false),
        ("我是", false),
    ];
    for (marker, strong_signal) in markers {
        let Some(start) = compact.rfind(marker) else {
            continue;
        };
        if !strong_signal {
            let allow_weak = compact.chars().count() <= 20
                || compact.contains("不是")
                || compact.contains("别叫")
                || compact.contains("叫错");
            if !allow_weak {
                continue;
            }
        }
        let tail = &compact[start + marker.len()..];
        if let Some(candidate) = extract_name_candidate_from_tail(tail) {
            return Some(candidate);
        }
    }
    None
}

fn build_group_member_identity_context(
    raw_text: &str,
    sender_user_id: i64,
    sender_profile: &TelegramGroupUserProfile,
    roster: &[(i64, TelegramGroupUserProfile)],
    max_roster: usize,
) -> String {
    let mut lines = Vec::new();
    lines.push("[群成员身份映射]".to_string());
    lines.push(format!("当前发言账号: uid={sender_user_id}"));
    lines.push(format!("当前发言称呼: {}", sender_profile.preferred_name));
    if let Some(username) = sender_profile.username.as_deref() {
        lines.push(format!("当前发言用户名: @{username}"));
    }
    lines.push("账号称呼映射:".to_string());

    for (uid, profile) in roster.iter().take(max_roster.max(1)) {
        if let Some(username) = profile.username.as_deref() {
            lines.push(format!(
                "uid={uid} -> {} (@{username})",
                profile.preferred_name
            ));
        } else {
            lines.push(format!("uid={uid} -> {}", profile.preferred_name));
        }
    }
    lines.push(
        "规则: 回复点名时，称呼必须和 uid 对应；若用户纠正称呼，立即使用最新称呼。".to_string(),
    );
    lines.push(format!("原始消息: {}", raw_text.trim()));
    lines.push("[/群成员身份映射]".to_string());
    lines.join("\n")
}

fn normalize_alias_token(raw: &str) -> String {
    raw.trim()
        .trim_matches(|ch: char| ch.is_whitespace() || ",，:：;；.!?！？()[]{}<>\"'@".contains(ch))
        .to_lowercase()
}

fn extract_dynamic_alias_candidates(text: &str, bot_username: &str) -> Vec<String> {
    let own = bot_username.trim().trim_start_matches('@').to_lowercase();
    if own.is_empty() {
        return vec![];
    }
    let own_handle = format!("@{own}");
    let tokens = text
        .split(|ch: char| ch.is_whitespace() || ",，:：;；.!?！？()[]{}<>\"'".contains(ch))
        .map(normalize_alias_token)
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return vec![];
    }

    let mut out = Vec::new();
    for (idx, token) in tokens.iter().enumerate() {
        if token != &own_handle {
            continue;
        }
        if idx > 0 {
            let candidate = &tokens[idx - 1];
            if is_dynamic_alias_candidate(candidate, &own) && !out.contains(candidate) {
                out.push(candidate.clone());
            }
        }
        if idx + 1 < tokens.len() {
            let candidate = &tokens[idx + 1];
            if is_dynamic_alias_candidate(candidate, &own) && !out.contains(candidate) {
                out.push(candidate.clone());
            }
        }
    }

    if out.is_empty() {
        let first = &tokens[0];
        if is_dynamic_alias_candidate(first, &own) {
            out.push(first.clone());
        }
    }
    out
}

fn is_dynamic_alias_candidate(candidate: &str, bot_username: &str) -> bool {
    if candidate.is_empty() || candidate.starts_with('@') || candidate == bot_username {
        return false;
    }
    let len = candidate.chars().count();
    if !(1..=16).contains(&len) {
        return false;
    }
    let has_alpha = candidate
        .chars()
        .any(|ch| ch.is_alphabetic() || ('\u{4e00}'..='\u{9fff}').contains(&ch));
    if !has_alpha {
        return false;
    }
    !matches!(
        candidate,
        "hi" | "hello" | "hey" | "你好" | "您好" | "请问" | "请教" | "bot" | "机器人" | "助手"
    )
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

fn current_unix_timestamp_i64() -> i64 {
    current_unix_timestamp() as i64
}

fn build_draft_message_id(chat_id: i64, message_thread_id: Option<i64>) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let thread = message_thread_id.unwrap_or(0);
    format!("xm-{chat_id}-{thread}-{millis}")
}

#[derive(Debug, Clone, Copy)]
struct MarkdownListState {
    ordered: bool,
    next_index: usize,
}

fn render_telegram_text_parts(raw: &str, max_chars: usize) -> Vec<TelegramRenderedText> {
    let (think, body) = split_think_and_body(raw);
    let source = if think.trim().is_empty() {
        raw.trim()
    } else {
        body.trim()
    };
    if source.is_empty() {
        return Vec::new();
    }

    let source_chunk_max = (max_chars.max(1) / 2).max(512);
    let mut parts = Vec::new();
    for chunk in split_text_chunks(source, source_chunk_max) {
        let rendered = render_markdown_to_telegram_markdown_v2(&chunk);
        if rendered.trim().is_empty() {
            continue;
        }
        if rendered.chars().count() <= max_chars.max(1) {
            parts.push(TelegramRenderedText {
                text: rendered,
                parse_mode: Some("MarkdownV2"),
            });
            continue;
        }

        for split in split_text_chunks(&rendered, max_chars.max(1)) {
            if split.trim().is_empty() {
                continue;
            }
            parts.push(TelegramRenderedText {
                text: split,
                parse_mode: Some("MarkdownV2"),
            });
        }
    }

    parts
}

fn render_markdown_to_telegram_markdown_v2(input: &str) -> String {
    if input.trim().is_empty() {
        return String::new();
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(input, options);
    let mut out = String::new();
    let mut line_start = true;
    let mut quote_depth = 0usize;
    let mut in_code_block = false;
    let mut list_stack: Vec<MarkdownListState> = Vec::new();
    let mut link_stack: Vec<String> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {
                    ensure_blank_line(&mut out, &mut line_start);
                    out.push('*');
                    line_start = false;
                }
                Tag::Strong => {
                    out.push('*');
                    line_start = false;
                }
                Tag::Emphasis => {
                    out.push('_');
                    line_start = false;
                }
                Tag::Strikethrough => {
                    out.push('~');
                    line_start = false;
                }
                Tag::CodeBlock(kind) => {
                    ensure_blank_line(&mut out, &mut line_start);
                    in_code_block = true;
                    out.push_str("```");
                    if let CodeBlockKind::Fenced(lang) = kind {
                        let language = sanitize_code_fence_language(lang.as_ref());
                        if !language.is_empty() {
                            out.push_str(&language);
                        }
                    }
                    out.push('\n');
                    line_start = true;
                }
                Tag::List(start) => {
                    ensure_line_break(&mut out, &mut line_start);
                    list_stack.push(MarkdownListState {
                        ordered: start.is_some(),
                        next_index: start.unwrap_or(1) as usize,
                    });
                }
                Tag::Item => {
                    ensure_line_break(&mut out, &mut line_start);
                    if let Some(top) = list_stack.last_mut() {
                        if top.ordered {
                            out.push_str(&format!("{}\\. ", top.next_index));
                            top.next_index += 1;
                        } else {
                            out.push_str("• ");
                        }
                    } else {
                        out.push_str("• ");
                    }
                    line_start = false;
                }
                Tag::Link { dest_url, .. } => {
                    out.push('[');
                    line_start = false;
                    link_stack.push(dest_url.to_string());
                }
                Tag::BlockQuote(_) => {
                    ensure_line_break(&mut out, &mut line_start);
                    quote_depth += 1;
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => ensure_blank_line(&mut out, &mut line_start),
                TagEnd::Heading(..) => {
                    out.push('*');
                    line_start = false;
                    ensure_blank_line(&mut out, &mut line_start);
                }
                TagEnd::Strong => {
                    out.push('*');
                    line_start = false;
                }
                TagEnd::Emphasis => {
                    out.push('_');
                    line_start = false;
                }
                TagEnd::Strikethrough => {
                    out.push('~');
                    line_start = false;
                }
                TagEnd::CodeBlock => {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("```");
                    line_start = false;
                    in_code_block = false;
                    ensure_blank_line(&mut out, &mut line_start);
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    ensure_blank_line(&mut out, &mut line_start);
                }
                TagEnd::Item => ensure_line_break(&mut out, &mut line_start),
                TagEnd::Link => {
                    let dest = link_stack.pop().unwrap_or_default();
                    out.push_str("](");
                    out.push_str(&escape_markdown_v2_link_destination(&dest));
                    out.push(')');
                    line_start = false;
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                    ensure_line_break(&mut out, &mut line_start);
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    append_code_text(&mut out, text.as_ref(), &mut line_start, quote_depth, true);
                } else {
                    append_markdown_v2_text(&mut out, text.as_ref(), &mut line_start, quote_depth);
                }
            }
            Event::Code(text) => {
                maybe_push_quote_prefix(&mut out, &mut line_start, quote_depth);
                out.push('`');
                out.push_str(&escape_markdown_v2_code(text.as_ref()));
                out.push('`');
                line_start = false;
            }
            Event::SoftBreak | Event::HardBreak => {
                out.push('\n');
                line_start = true;
            }
            Event::Rule => {
                ensure_blank_line(&mut out, &mut line_start);
                append_markdown_v2_text(&mut out, "────────", &mut line_start, quote_depth);
                ensure_blank_line(&mut out, &mut line_start);
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑ " } else { "☐ " };
                append_markdown_v2_text(&mut out, marker, &mut line_start, quote_depth);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                append_markdown_v2_text(&mut out, html.as_ref(), &mut line_start, quote_depth);
            }
            Event::FootnoteReference(name) => {
                append_markdown_v2_text(
                    &mut out,
                    format!("[{}]", name).as_str(),
                    &mut line_start,
                    quote_depth,
                );
            }
            _ => {}
        }
    }

    out.trim().to_string()
}

fn sanitize_code_fence_language(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+' | '.'))
        .collect()
}

fn ensure_line_break(out: &mut String, line_start: &mut bool) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    *line_start = true;
}

fn ensure_blank_line(out: &mut String, line_start: &mut bool) {
    if out.is_empty() {
        *line_start = true;
        return;
    }
    if out.ends_with("\n\n") {
        *line_start = true;
        return;
    }
    if out.ends_with('\n') {
        out.push('\n');
    } else {
        out.push_str("\n\n");
    }
    *line_start = true;
}

fn maybe_push_quote_prefix(out: &mut String, line_start: &mut bool, quote_depth: usize) {
    if *line_start && quote_depth > 0 {
        for _ in 0..quote_depth {
            out.push_str("> ");
        }
        *line_start = false;
    }
}

fn append_code_text(
    out: &mut String,
    input: &str,
    line_start: &mut bool,
    quote_depth: usize,
    in_code_block: bool,
) {
    for ch in input.chars() {
        if ch == '\n' {
            out.push('\n');
            *line_start = true;
            continue;
        }
        maybe_push_quote_prefix(out, line_start, quote_depth);
        if in_code_block {
            match ch {
                '\\' => out.push_str("\\\\"),
                '`' => out.push_str("\\`"),
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
        *line_start = false;
    }
}

fn append_markdown_v2_text(
    out: &mut String,
    input: &str,
    line_start: &mut bool,
    quote_depth: usize,
) {
    for ch in input.chars() {
        if ch == '\n' {
            out.push('\n');
            *line_start = true;
            continue;
        }
        maybe_push_quote_prefix(out, line_start, quote_depth);
        if needs_markdown_v2_escape(ch) {
            out.push('\\');
        }
        out.push(ch);
        *line_start = false;
    }
}

fn needs_markdown_v2_escape(ch: char) -> bool {
    matches!(
        ch,
        '\\' | '_'
            | '*'
            | '['
            | ']'
            | '('
            | ')'
            | '~'
            | '`'
            | '>'
            | '#'
            | '+'
            | '-'
            | '='
            | '|'
            | '{'
            | '}'
            | '.'
            | '!'
    )
}

fn escape_markdown_v2_code(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_markdown_v2_link_destination(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ')' => out.push_str("\\)"),
            _ => out.push(ch),
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

fn truncate_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars.max(1)).collect()
}

async fn run_telegram_scheduler_loop(
    channel_name: String,
    service: Arc<MessageService>,
    sender: TelegramSender,
    scheduler_settings: TelegramSchedulerSettings,
    mut shutdown: watch::Receiver<bool>,
    scheduler_diag_state: Arc<tokio::sync::Mutex<TelegramSchedulerDiagState>>,
) {
    info!(
        channel = %channel_name,
        tick_secs = scheduler_settings.tick_secs,
        batch_size = scheduler_settings.batch_size,
        lease_secs = scheduler_settings.lease_secs,
        "telegram scheduler worker started"
    );
    {
        let mut diag = scheduler_diag_state.lock().await;
        diag.worker_started_at_unix = Some(current_unix_timestamp());
    }

    loop {
        if *shutdown.borrow() {
            break;
        }

        let now_unix = current_unix_timestamp_i64();
        let lease_token = format!(
            "lease-{}-{}-{}",
            channel_name,
            now_unix,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );

        let claimed = match service
            .claim_due_telegram_scheduler_jobs(
                channel_name.clone(),
                now_unix,
                scheduler_settings.batch_size,
                scheduler_settings.lease_secs as i64,
                lease_token.clone(),
            )
            .await
        {
            Ok(items) => items,
            Err(err) => {
                let err_text = format!("{err:#}");
                {
                    let mut diag = scheduler_diag_state.lock().await;
                    diag.last_tick_at_unix = Some(current_unix_timestamp());
                    diag.last_error = Some(err_text.clone());
                    diag.total_runs_err += 1;
                    diag.last_run_err_at_unix = Some(current_unix_timestamp());
                }
                warn!(
                    channel = %channel_name,
                    error = %err_text,
                    "telegram scheduler claim failed"
                );
                if wait_for_shutdown_or_timeout(
                    &mut shutdown,
                    Duration::from_secs(scheduler_settings.tick_secs),
                )
                .await
                {
                    break;
                }
                continue;
            }
        };

        {
            let mut diag = scheduler_diag_state.lock().await;
            diag.last_tick_at_unix = Some(current_unix_timestamp());
            diag.last_claimed_jobs = claimed.len();
            diag.total_claimed_jobs += claimed.len() as u64;
        }

        match service
            .query_telegram_scheduler_stats(channel_name.clone(), now_unix)
            .await
        {
            Ok(stats) => {
                let mut diag = scheduler_diag_state.lock().await;
                diag.jobs_total = stats.jobs_total;
                diag.jobs_active = stats.jobs_active;
                diag.jobs_paused = stats.jobs_paused;
                diag.jobs_completed = stats.jobs_completed;
                diag.jobs_canceled = stats.jobs_canceled;
                diag.jobs_due = stats.jobs_due;
            }
            Err(err) => {
                let err_text = format!("{err:#}");
                {
                    let mut diag = scheduler_diag_state.lock().await;
                    diag.last_error = Some(err_text.clone());
                }
                warn!(
                    channel = %channel_name,
                    error = %err_text,
                    "telegram scheduler stats query failed"
                );
            }
        }

        for job in claimed {
            let started_at_unix = current_unix_timestamp_i64();
            let execution: anyhow::Result<()> = async {
                let outbound_text = match job.task_kind {
                    TelegramSchedulerTaskKind::Reminder => job.payload.clone(),
                    TelegramSchedulerTaskKind::Agent => {
                        let session_id = format!("tg:{}:scheduler:{}", job.chat_id, job.job_id);
                        let user_id = format!("scheduler:{}", job.owner_user_id);
                        let reply = service
                            .handle(IncomingMessage {
                                channel: channel_name.clone(),
                                session_id,
                                user_id,
                                text: job.payload.clone(),
                                reply_target: Some(ReplyTarget::Telegram {
                                    chat_id: job.chat_id,
                                    message_thread_id: job.message_thread_id,
                                }),
                            })
                            .await
                            .context("failed to generate scheduler agent output")?;
                        reply.text
                    }
                };

                sender
                    .send_message(job.chat_id, job.message_thread_id, None, &outbound_text)
                    .await
                    .context("failed to send scheduler telegram message")?;

                let finished_at_unix = current_unix_timestamp_i64();
                let next_run = match job.schedule_kind {
                    TelegramSchedulerScheduleKind::Once => None,
                    TelegramSchedulerScheduleKind::Cron => {
                        let expr = job
                            .cron_expr
                            .clone()
                            .context("cron scheduler job missing cron_expr")?;
                        let timezone = if job.timezone.trim().is_empty() {
                            scheduler_settings.default_timezone.clone()
                        } else {
                            job.timezone.clone()
                        };
                        compute_next_run_at_unix(
                            &ScheduleSpec::Cron { expr },
                            finished_at_unix,
                            &timezone,
                        )?
                    }
                };
                let run_count_after = job.run_count.saturating_add(1);
                let max_runs_reached = job
                    .max_runs
                    .map(|max| run_count_after >= max)
                    .unwrap_or(false);
                let mark_completed =
                    matches!(job.schedule_kind, TelegramSchedulerScheduleKind::Once)
                        || max_runs_reached
                        || next_run.is_none();

                service
                    .complete_telegram_scheduler_job_run(CompleteTelegramSchedulerJobRunRequest {
                        channel: channel_name.clone(),
                        chat_id: job.chat_id,
                        job_id: job.job_id.clone(),
                        lease_token: lease_token.clone(),
                        started_at_unix,
                        finished_at_unix,
                        next_run_at_unix: if mark_completed { None } else { next_run },
                        mark_completed,
                    })
                    .await
                    .context("failed to mark scheduler job success")?;
                Ok(())
            }
            .await;

            match execution {
                Ok(()) => {
                    let mut diag = scheduler_diag_state.lock().await;
                    diag.total_runs_ok += 1;
                    diag.last_run_ok_at_unix = Some(current_unix_timestamp());
                    diag.last_error = None;
                }
                Err(err) => {
                    let err_text = format!("{err:#}");
                    let finished_at_unix = current_unix_timestamp_i64();
                    let attempt = (job.run_count.max(0) as u32).saturating_add(1);
                    let backoff = compute_retry_backoff_secs(attempt);
                    let pause_job = attempt >= 8;
                    let next_run_at_unix = if pause_job {
                        None
                    } else {
                        Some(finished_at_unix + backoff)
                    };

                    if let Err(mark_err) = service
                        .fail_telegram_scheduler_job_run(FailTelegramSchedulerJobRunRequest {
                            channel: channel_name.clone(),
                            chat_id: job.chat_id,
                            job_id: job.job_id.clone(),
                            lease_token: lease_token.clone(),
                            started_at_unix,
                            finished_at_unix,
                            next_run_at_unix,
                            pause_job,
                            error: err_text.clone(),
                        })
                        .await
                    {
                        warn!(
                            channel = %channel_name,
                            job_id = %job.job_id,
                            error = %format!("{mark_err:#}"),
                            "failed to mark scheduler job failure"
                        );
                    }

                    {
                        let mut diag = scheduler_diag_state.lock().await;
                        diag.total_runs_err += 1;
                        diag.last_run_err_at_unix = Some(current_unix_timestamp());
                        diag.last_error = Some(err_text.clone());
                    }
                    warn!(
                        channel = %channel_name,
                        job_id = %job.job_id,
                        error = %err_text,
                        "telegram scheduler job execution failed"
                    );
                }
            }
        }

        if wait_for_shutdown_or_timeout(
            &mut shutdown,
            Duration::from_secs(scheduler_settings.tick_secs),
        )
        .await
        {
            break;
        }
    }

    info!(channel = %channel_name, "telegram scheduler worker stopped");
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
        GroupDecisionKind, GroupSignalInput, McpCommandAccess, PrivateChatAccess,
        TELEGRAM_MAX_TEXT_CHARS, TelegramCommandSettings, TelegramGroupTriggerMode,
        TelegramGroupUserProfile, TelegramReplyMessage, TelegramSchedulerSettings,
        TelegramSlashCommand, TelegramUser, build_draft_message_id,
        build_group_member_identity_context, detect_scheduler_job_operation_keyword,
        evaluate_group_decision, evaluate_mcp_command_access, evaluate_private_chat_access,
        extract_dynamic_alias_candidates, extract_realtime_name_correction,
        extract_scheduler_job_id, is_reply_to_bot_message, looks_like_scheduler_list_text,
        message_mentions_bot, parse_admin_user_ids, parse_telegram_slash_command,
        render_telegram_text_parts, short_description_payload, telegram_help_text,
        telegram_registered_commands, telegram_session_id, text_implies_latest_target,
        truncate_chars, typing_action_payload,
    };

    fn test_scheduler_settings() -> TelegramSchedulerSettings {
        TelegramSchedulerSettings {
            enabled: true,
            tick_secs: 2,
            batch_size: 8,
            lease_secs: 30,
            default_timezone: "Asia/Shanghai".to_string(),
            nl_enabled: true,
            nl_min_confidence: 0.78,
            require_confirm: true,
            max_jobs_per_owner: 64,
        }
    }

    #[test]
    fn think_block_is_stripped_and_only_body_is_sent() {
        let raw = "<think>内部推理A\n内部推理B</think>\n\n最终回答";
        let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
        assert_eq!(parts.len(), 1);
        let rendered = &parts[0];
        assert_eq!(rendered.parse_mode, Some("MarkdownV2"));
        assert_eq!(rendered.text, "最终回答");
    }

    #[test]
    fn plain_text_keeps_original_mode() {
        let raw = "你好，这里是正文。";
        let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
        let rendered = &parts[0];
        assert_eq!(rendered.parse_mode, Some("MarkdownV2"));
        assert_eq!(rendered.text, raw);
    }

    #[test]
    fn markdownv2_renderer_supports_common_markdown_features() {
        let raw = "# 标题\n\n**加粗** `code` [链接](https://example.com)\n- 条目";
        let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
        assert_eq!(parts.len(), 1);
        let rendered = &parts[0];
        assert_eq!(rendered.parse_mode, Some("MarkdownV2"));
        assert!(rendered.text.contains("*标题*"));
        assert!(rendered.text.contains("*加粗*"));
        assert!(rendered.text.contains("`code`"));
        assert!(rendered.text.contains("[链接](https://example.com)"));
        assert!(rendered.text.contains("• 条目"));
    }

    #[test]
    fn markdownv2_renderer_escapes_special_chars() {
        let raw = "a+b=c.";
        let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
        assert_eq!(parts.len(), 1);
        let rendered = &parts[0];
        assert_eq!(rendered.parse_mode, Some("MarkdownV2"));
        assert_eq!(rendered.text, "a\\+b\\=c\\.");
    }

    #[test]
    fn unclosed_think_is_suppressed() {
        let raw = "<think>这段还没闭合";
        let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
        assert!(parts.is_empty());
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
                username: None,
                first_name: None,
                last_name: None,
            }),
        };
        let reply_from_user = TelegramReplyMessage {
            message_id: 2,
            from: Some(TelegramUser {
                id: 7,
                is_bot: Some(false),
                username: None,
                first_name: None,
                last_name: None,
            }),
        };
        let reply_without_from = TelegramReplyMessage {
            message_id: 3,
            from: None,
        };

        assert!(is_reply_to_bot_message(Some(&reply_from_bot), None));
        assert!(!is_reply_to_bot_message(Some(&reply_from_user), None));
        assert!(!is_reply_to_bot_message(Some(&reply_without_from), None));
        assert!(!is_reply_to_bot_message(None, None));
    }

    #[test]
    fn reply_to_bot_detection_falls_back_to_bot_user_id_when_is_bot_missing() {
        let reply_without_flag = TelegramReplyMessage {
            message_id: 7,
            from: Some(TelegramUser {
                id: 12345,
                is_bot: None,
                username: None,
                first_name: None,
                last_name: None,
            }),
        };
        assert!(is_reply_to_bot_message(
            Some(&reply_without_flag),
            Some(12345)
        ));
        assert!(!is_reply_to_bot_message(
            Some(&reply_without_flag),
            Some(99999)
        ));
    }

    #[test]
    fn extract_dynamic_alias_candidates_learns_prefix_name() {
        let aliases =
            extract_dynamic_alias_candidates("小绿，@myxiaomaolvbot 你看看这个", "myxiaomaolvbot");
        assert!(aliases.iter().any(|v| v == "小绿"));
    }

    #[test]
    fn extract_realtime_name_correction_supports_jiaowo_pattern() {
        let corrected = extract_realtime_name_correction("请叫我阿青");
        assert_eq!(corrected, Some("阿青".to_string()));
    }

    #[test]
    fn extract_realtime_name_correction_supports_wojiao_pattern() {
        let corrected = extract_realtime_name_correction("我叫小陈");
        assert_eq!(corrected, Some("小陈".to_string()));
    }

    #[test]
    fn extract_realtime_name_correction_ignores_non_name_sentence() {
        let corrected = extract_realtime_name_correction("我是来问一个问题的");
        assert_eq!(corrected, None);
    }

    #[test]
    fn group_member_identity_context_includes_sender_and_roster_mapping() {
        let sender_profile = TelegramGroupUserProfile {
            preferred_name: "阿青".to_string(),
            username: Some("aqing_99".to_string()),
            updated_at: 1700000000,
        };
        let roster = vec![
            (
                1001_i64,
                TelegramGroupUserProfile {
                    preferred_name: "阿青".to_string(),
                    username: Some("aqing_99".to_string()),
                    updated_at: 1700000000,
                },
            ),
            (
                1002_i64,
                TelegramGroupUserProfile {
                    preferred_name: "小米".to_string(),
                    username: Some("xiaomi".to_string()),
                    updated_at: 1699999999,
                },
            ),
        ];

        let context = build_group_member_identity_context(
            "我们今天要评审需求",
            1001,
            &sender_profile,
            &roster,
            6,
        );

        assert!(context.contains("当前发言账号: uid=1001"));
        assert!(context.contains("当前发言称呼: 阿青"));
        assert!(context.contains("uid=1001 -> 阿青"));
        assert!(context.contains("uid=1002 -> 小米"));
        assert!(context.contains("原始消息: 我们今天要评审需求"));
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
            assert_eq!(part.parse_mode, Some("MarkdownV2"));
        }
    }

    #[test]
    fn long_think_text_only_keeps_final_body() {
        let think = "推理".repeat(TELEGRAM_MAX_TEXT_CHARS);
        let raw = format!("<think>{think}</think>\n\n最终结论");
        let parts = render_telegram_text_parts(&raw, TELEGRAM_MAX_TEXT_CHARS);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].parse_mode, Some("MarkdownV2"));
        assert_eq!(parts[0].text, "最终结论");
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
    fn parse_slash_command_supports_task() {
        let parsed = parse_telegram_slash_command(
            "/task add 2026-03-01T09:00:00+08:00 | 会议提醒",
            Some("xiaomaolv_bot"),
        );
        assert_eq!(
            parsed,
            Some(TelegramSlashCommand::Task {
                tail: "add 2026-03-01T09:00:00+08:00 | 会议提醒".to_string()
            })
        );
    }

    #[test]
    fn telegram_help_and_registered_commands_include_task() {
        let help = telegram_help_text();
        assert!(help.contains("/task"));
        let commands = telegram_registered_commands();
        assert!(commands.iter().any(|(name, _)| *name == "task"));
    }

    #[test]
    fn parse_task_schedule_and_payload_supports_pipe_format() {
        let (schedule, payload) =
            super::parse_task_schedule_and_payload("2026-03-01 09:00 | 提醒开会").expect("parse");
        assert_eq!(schedule, "2026-03-01 09:00");
        assert_eq!(payload, "提醒开会");
    }

    #[test]
    fn parse_scheduler_once_time_supports_default_timezone_local_format() {
        let unix = super::parse_scheduler_once_time("2026-03-01 09:00", "Asia/Shanghai")
            .expect("parse time");
        assert!(unix > 0);
    }

    #[test]
    fn detect_scheduler_management_keywords_and_job_id() {
        let text = "请帮我暂停任务 task-100-200-300";
        let op = detect_scheduler_job_operation_keyword(text).expect("op");
        assert_eq!(op, super::SchedulerJobOperation::Pause);
        let job_id = extract_scheduler_job_id(text).expect("job id");
        assert_eq!(job_id, "task-100-200-300");
    }

    #[test]
    fn looks_like_scheduler_list_text_supports_cn_and_en() {
        assert!(looks_like_scheduler_list_text("帮我看看任务列表"));
        assert!(looks_like_scheduler_list_text("list tasks"));
    }

    #[test]
    fn text_implies_latest_target_supports_cn_and_en() {
        assert!(text_implies_latest_target("暂停最近那个任务"));
        assert!(text_implies_latest_target("delete latest task"));
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
            scheduler: test_scheduler_settings(),
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
            scheduler: test_scheduler_settings(),
        };
        let access = evaluate_mcp_command_access(true, Some(7), &settings);
        assert_eq!(access, McpCommandAccess::Unauthorized);
    }

    #[test]
    fn private_chat_access_allows_admin() {
        let access = evaluate_private_chat_access(Some(42), &[42, 99]);
        assert_eq!(access, PrivateChatAccess::Allowed);
    }

    #[test]
    fn private_chat_access_rejects_non_admin() {
        let access = evaluate_private_chat_access(Some(7), &[42, 99]);
        assert_eq!(access, PrivateChatAccess::Unauthorized);
    }

    #[test]
    fn private_chat_access_rejects_when_allowlist_missing() {
        let access = evaluate_private_chat_access(Some(7), &[]);
        assert_eq!(access, PrivateChatAccess::MissingAdminAllowlist);
    }

    #[test]
    fn group_decision_strict_only_allows_mention_or_reply() {
        let decision = evaluate_group_decision(
            TelegramGroupTriggerMode::Strict,
            &GroupSignalInput {
                mentioned: false,
                replied_to_bot: false,
                recent_bot_participation: true,
                alias_hit: true,
                has_question_marker: true,
                points_to_other_bot: false,
                low_signal_noise: false,
                cooldown_active: false,
            },
            70,
        );
        assert_eq!(decision.kind, GroupDecisionKind::Ignore);
    }

    #[test]
    fn group_decision_strict_allows_explicit_mention() {
        let decision = evaluate_group_decision(
            TelegramGroupTriggerMode::Strict,
            &GroupSignalInput {
                mentioned: true,
                replied_to_bot: false,
                recent_bot_participation: false,
                alias_hit: false,
                has_question_marker: false,
                points_to_other_bot: false,
                low_signal_noise: false,
                cooldown_active: true,
            },
            70,
        );
        assert_eq!(decision.kind, GroupDecisionKind::Respond);
    }

    #[test]
    fn group_decision_strict_allows_reply_to_bot() {
        let decision = evaluate_group_decision(
            TelegramGroupTriggerMode::Strict,
            &GroupSignalInput {
                mentioned: false,
                replied_to_bot: true,
                recent_bot_participation: false,
                alias_hit: false,
                has_question_marker: false,
                points_to_other_bot: false,
                low_signal_noise: false,
                cooldown_active: true,
            },
            70,
        );
        assert_eq!(decision.kind, GroupDecisionKind::Respond);
    }

    #[test]
    fn group_decision_smart_promotes_contextual_question_to_observe_or_respond() {
        let decision = evaluate_group_decision(
            TelegramGroupTriggerMode::Smart,
            &GroupSignalInput {
                mentioned: false,
                replied_to_bot: false,
                recent_bot_participation: true,
                alias_hit: true,
                has_question_marker: true,
                points_to_other_bot: false,
                low_signal_noise: false,
                cooldown_active: false,
            },
            70,
        );
        assert_eq!(decision.kind, GroupDecisionKind::Respond);
    }

    #[test]
    fn group_decision_smart_suppresses_other_bot_target() {
        let decision = evaluate_group_decision(
            TelegramGroupTriggerMode::Smart,
            &GroupSignalInput {
                mentioned: false,
                replied_to_bot: false,
                recent_bot_participation: false,
                alias_hit: false,
                has_question_marker: true,
                points_to_other_bot: true,
                low_signal_noise: false,
                cooldown_active: false,
            },
            70,
        );
        assert_eq!(decision.kind, GroupDecisionKind::Ignore);
    }
}
