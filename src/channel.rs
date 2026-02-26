use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

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
use crate::memory::{
    CompleteTelegramSchedulerJobRunRequest, FailTelegramSchedulerJobRunRequest,
    TelegramSchedulerJobRecord, TelegramSchedulerJobStatus, TelegramSchedulerScheduleKind,
    TelegramSchedulerTaskKind, UpsertTelegramSchedulerPendingIntentRequest,
};
use crate::provider::StreamSink;
use crate::scheduler::{
    SchedulerExecutionEvent, SchedulerExecutionState, apply_scheduler_state_transition,
    compute_retry_backoff_secs, plan_failure_transition, plan_success_transition,
    scheduler_policy_json_schema,
};
use crate::service::{MessageService, TelegramSchedulerIntent};
use crate::skills_commands::{
    discover_skill_registry, execute_skills_command, parse_telegram_skills_command,
    skills_help_text,
};

mod group_pipeline;
mod scheduler_helpers;
mod telegram_commands;
mod telegram_markdown;
mod telegram_rules;
mod telegram_stream;
mod update_pipeline;
mod workers;

use self::scheduler_helpers::*;
use self::telegram_commands::*;
use self::telegram_markdown::*;
use self::telegram_rules::*;
use self::telegram_stream::*;
use self::update_pipeline::*;
use self::workers::*;

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

impl SchedulerJobOperation {
    fn as_event(self) -> SchedulerExecutionEvent {
        match self {
            Self::Pause => SchedulerExecutionEvent::ManualPause,
            Self::Resume => SchedulerExecutionEvent::ManualResume,
            Self::Delete => SchedulerExecutionEvent::ManualCancel,
        }
    }
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

struct SchedulerJobOperationContext<'a> {
    ctx: &'a ChannelContext,
    sender: &'a TelegramSender,
    message: &'a TelegramMessage,
    scheduler_settings: &'a TelegramSchedulerSettings,
    chat_id: i64,
    owner_user_id: i64,
}

fn scheduler_state_from_job_status(status: TelegramSchedulerJobStatus) -> SchedulerExecutionState {
    match status {
        TelegramSchedulerJobStatus::Active => SchedulerExecutionState::Active,
        TelegramSchedulerJobStatus::Paused => SchedulerExecutionState::Paused,
        TelegramSchedulerJobStatus::Completed => SchedulerExecutionState::Completed,
        TelegramSchedulerJobStatus::Canceled => SchedulerExecutionState::Canceled,
    }
}

fn scheduler_state_to_job_status(state: SchedulerExecutionState) -> TelegramSchedulerJobStatus {
    match state {
        SchedulerExecutionState::Active => TelegramSchedulerJobStatus::Active,
        SchedulerExecutionState::Paused => TelegramSchedulerJobStatus::Paused,
        SchedulerExecutionState::Completed => TelegramSchedulerJobStatus::Completed,
        SchedulerExecutionState::Canceled => TelegramSchedulerJobStatus::Canceled,
    }
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

        let group_mention_bot_username = config
            .settings
            .get("bot_username")
            .map(|value| value.trim().trim_start_matches('@').to_string())
            .filter(|value| !value.is_empty());

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

#[derive(Clone)]
struct TelegramUpdateProcessingContext {
    streaming_enabled: bool,
    streaming_edit_interval_ms: u64,
    streaming_prefer_draft: bool,
    group_mention_bot_username: Option<String>,
    bot_user_id: Option<i64>,
    command_settings: TelegramCommandSettings,
    group_trigger_settings: TelegramGroupTriggerSettings,
    group_runtime_state: Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
    diag_state: Arc<tokio::sync::Mutex<TelegramPollingDiagState>>,
}

impl TelegramChannelPlugin {
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
        _ctx: ChannelContext,
        _inbound: ChannelInbound,
    ) -> Result<ChannelResponse, ChannelPluginError> {
        Err(ChannelPluginError::BadRequest(
            "telegram inbound webhook is no longer supported; use polling mode".to_string(),
        ))
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
                                TelegramUpdateProcessingContext {
                                    streaming_enabled,
                                    streaming_edit_interval_ms,
                                    streaming_prefer_draft,
                                    group_mention_bot_username: group_mention_bot_username.clone(),
                                    bot_user_id,
                                    command_settings: command_settings.clone(),
                                    group_trigger_settings: group_trigger_settings.clone(),
                                    group_runtime_state: group_runtime_state.clone(),
                                    diag_state: polling_diag_state.clone(),
                                },
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
                "policy_schema": scheduler_policy_json_schema(),
                "diag": scheduler
            },
            "polling": polling,
            "get_me": get_me
        })))
    }

    fn mode(&self) -> Option<&'static str> {
        Some("polling")
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

fn truncate_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars.max(1)).collect()
}

#[cfg(test)]
mod tests;
