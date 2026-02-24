use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::domain::{IncomingMessage, ReplyTarget};
use crate::provider::StreamSink;
use crate::service::MessageService;

pub use crate::config::ChannelPluginConfig;

#[derive(Debug, Deserialize)]
pub struct TelegramUpdate {
    pub message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramMessage {
    pub chat: TelegramChat,
    pub from: Option<TelegramUser>,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
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
}

#[derive(Debug, Deserialize)]
struct TelegramSentMessage {
    message_id: i64,
}

#[derive(Debug, Clone)]
struct TelegramRenderedText {
    text: String,
    parse_mode: Option<&'static str>,
}

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

    pub async fn send_message(&self, chat_id: i64, text: &str) -> anyhow::Result<()> {
        let _ = self.send_message_with_id(chat_id, text).await?;
        Ok(())
    }

    pub async fn send_message_with_id(&self, chat_id: i64, text: &str) -> anyhow::Result<i64> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let rendered = render_telegram_text(text);
        let mut payload = serde_json::json!({ "chat_id": chat_id, "text": rendered.text });
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

    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> anyhow::Result<()> {
        let url = format!(
            "https://api.telegram.org/bot{}/editMessageText",
            self.bot_token
        );
        let rendered = render_telegram_text(text);
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

        Ok(Arc::new(TelegramChannelPlugin {
            sender: TelegramSender::new(bot_token.clone()),
            bot_token,
            webhook_secret,
            mode,
            polling_timeout_secs,
            streaming_enabled,
            streaming_edit_interval_ms,
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
        )
        .await
        .map_err(ChannelPluginError::internal)?;

        Ok(ChannelResponse::json(serde_json::json!({ "ok": true })))
    }

    async fn start_background(
        &self,
        ctx: ChannelRuntimeContext,
    ) -> anyhow::Result<Option<ChannelWorker>> {
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
        let worker_name = format!("{channel_name}-polling");

        let task = tokio::spawn(async move {
            info!(channel = %channel_name, "telegram polling worker started");
            let client = Client::new();
            let mut offset: i64 = 0;

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
                        for update in updates {
                            offset = offset.max(update.update_id + 1);

                            let payload = TelegramUpdate {
                                message: update.message,
                            };

                            if let Err(err) = process_telegram_update(
                                ChannelContext {
                                    service: service.clone(),
                                    channel_name: channel_name.clone(),
                                },
                                &sender,
                                payload,
                                streaming_enabled,
                                streaming_edit_interval_ms,
                            )
                            .await
                            {
                                warn!(channel = %channel_name, error = %err, "failed to process telegram update in polling mode");
                            }
                        }
                    }
                    Err(err) => {
                        warn!(channel = %channel_name, error = %err, "telegram polling request failed");
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
) -> anyhow::Result<()> {
    if let Some(message) = update.message
        && let Some(text) = message.text
    {
        let chat_id = message.chat.id;
        let user_id = message
            .from
            .as_ref()
            .map(|u| u.id.to_string())
            .unwrap_or_else(|| chat_id.to_string());

        let incoming = IncomingMessage {
            channel: ctx.channel_name,
            session_id: format!("tg:{chat_id}"),
            user_id,
            text,
            reply_target: Some(ReplyTarget::Telegram { chat_id }),
        };

        if streaming_enabled {
            let mut sink = TelegramStreamSink::new(
                sender,
                chat_id,
                Duration::from_millis(streaming_edit_interval_ms),
            );
            let reply = ctx
                .service
                .handle_stream(incoming, &mut sink)
                .await
                .context("failed to process telegram streaming message")?;
            sink.finalize(&reply.text).await?;
        } else {
            let reply = ctx
                .service
                .handle(incoming)
                .await
                .context("failed to process telegram message")?;

            sender
                .send_message(chat_id, &reply.text)
                .await
                .context("failed to send telegram response")?;
        }
    }

    Ok(())
}

struct TelegramStreamSink<'a> {
    sender: &'a TelegramSender,
    chat_id: i64,
    min_edit_interval: Duration,
    message_id: Option<i64>,
    pending_text: String,
    rendered_text: String,
    last_push: Option<Instant>,
}

impl<'a> TelegramStreamSink<'a> {
    fn new(sender: &'a TelegramSender, chat_id: i64, min_edit_interval: Duration) -> Self {
        Self {
            sender,
            chat_id,
            min_edit_interval,
            message_id: None,
            pending_text: String::new(),
            rendered_text: String::new(),
            last_push: None,
        }
    }

    async fn finalize(&mut self, full_text: &str) -> anyhow::Result<()> {
        if full_text.is_empty() {
            return Ok(());
        }

        self.pending_text = full_text.to_string();
        if self.pending_text == self.rendered_text {
            return Ok(());
        }

        if let Err(err) = self.flush(true).await {
            warn!(error = %err, "telegram stream finalize failed, fallback to sendMessage");
            self.sender
                .send_message(self.chat_id, full_text)
                .await
                .context("telegram stream fallback sendMessage failed")?;
            self.rendered_text = full_text.to_string();
        }

        Ok(())
    }

    async fn flush(&mut self, force: bool) -> anyhow::Result<()> {
        if self.pending_text.is_empty() || self.pending_text == self.rendered_text {
            return Ok(());
        }

        if !force
            && let Some(last) = self.last_push
            && last.elapsed() < self.min_edit_interval
        {
            return Ok(());
        }

        if let Some(message_id) = self.message_id {
            self.sender
                .edit_message(self.chat_id, message_id, &self.pending_text)
                .await
                .context("failed to edit telegram stream message")?;
        } else {
            let message_id = self
                .sender
                .send_message_with_id(self.chat_id, &self.pending_text)
                .await
                .context("failed to send telegram stream message")?;
            self.message_id = Some(message_id);
        }

        self.rendered_text = self.pending_text.clone();
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

fn render_telegram_text(raw: &str) -> TelegramRenderedText {
    let (think, body) = split_think_and_body(raw);
    if think.trim().is_empty() {
        return TelegramRenderedText {
            text: raw.to_string(),
            parse_mode: None,
        };
    }

    let mut rendered = String::new();
    rendered.push_str("<blockquote>");
    rendered.push_str("🧠 思考草稿（点击展开）");
    rendered.push_str("</blockquote>\n");
    rendered.push_str("<tg-spoiler>");
    rendered.push_str(&escape_html(think.trim()));
    rendered.push_str("</tg-spoiler>");

    if !body.trim().is_empty() {
        rendered.push_str("\n\n");
        rendered.push_str(&escape_html(body.trim()));
    }

    TelegramRenderedText {
        text: rendered,
        parse_mode: Some("HTML"),
    }
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
        .context("failed to call telegram getUpdates")?
        .error_for_status()
        .context("telegram getUpdates returned error")?;

    let body: TelegramApiResponse<Vec<TelegramPollUpdate>> = response
        .json()
        .await
        .context("failed to decode telegram getUpdates payload")?;

    if !body.ok {
        bail!(
            "telegram getUpdates returned ok=false: {}",
            body.description
                .unwrap_or_else(|| "unknown error".to_string())
        );
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
    use super::render_telegram_text;

    #[test]
    fn think_block_is_rendered_as_spoiler_with_body() {
        let raw = "<think>内部推理A\n内部推理B</think>\n\n最终回答";
        let rendered = render_telegram_text(raw);

        assert_eq!(rendered.parse_mode, Some("HTML"));
        assert!(rendered.text.contains("<tg-spoiler>"));
        assert!(rendered.text.contains("最终回答"));
        assert!(!rendered.text.contains("<think>"));
        assert!(!rendered.text.contains("</think>"));
    }

    #[test]
    fn plain_text_keeps_original_mode() {
        let raw = "你好，这里是正文。";
        let rendered = render_telegram_text(raw);
        assert_eq!(rendered.parse_mode, None);
        assert_eq!(rendered.text, raw);
    }

    #[test]
    fn unclosed_think_is_hidden_in_spoiler() {
        let raw = "<think>这段还没闭合";
        let rendered = render_telegram_text(raw);
        assert_eq!(rendered.parse_mode, Some("HTML"));
        assert!(rendered.text.contains("<tg-spoiler>"));
        assert!(rendered.text.contains("这段还没闭合"));
    }
}
