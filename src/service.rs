use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::warn;

use crate::domain::{IncomingMessage, MessageRole, OutgoingMessage, StoredMessage};
use crate::mcp::{McpRuntime, McpToolInfo};
use crate::memory::{
    ClaimDueTelegramSchedulerJobsRequest, CompleteTelegramSchedulerJobRunRequest,
    CreateTelegramSchedulerJobRequest, FailTelegramSchedulerJobRunRequest, GroupAliasLoadRequest,
    GroupAliasUpsertRequest, GroupUserProfileLoadRequest, GroupUserProfileRecord,
    GroupUserProfileUpsertRequest, MemoryBackend, MemoryContextRequest, MemoryWriteRequest,
    SqliteMemoryBackend, SqliteMemoryStore, TelegramSchedulerJobListRequest,
    TelegramSchedulerJobRecord, TelegramSchedulerJobStatus, TelegramSchedulerPendingIntentRecord,
    TelegramSchedulerStats, TelegramSchedulerStatsRequest, UpdateTelegramSchedulerJobStatusRequest,
    UpsertTelegramSchedulerPendingIntentRequest,
};
use crate::provider::{ChatProvider, CompletionRequest, StreamSink};

#[derive(Debug, Clone)]
pub struct AgentMcpSettings {
    pub enabled: bool,
    pub max_iterations: usize,
    pub max_tool_result_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelegramSchedulerIntent {
    pub action: String,
    pub confidence: f32,
    pub task_kind: Option<String>,
    pub payload: Option<String>,
    pub schedule_kind: Option<String>,
    pub run_at: Option<String>,
    pub cron_expr: Option<String>,
    pub timezone: Option<String>,
    pub job_id: Option<String>,
    pub job_operation: Option<String>,
}

impl Default for AgentMcpSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_iterations: 4,
            max_tool_result_chars: 4000,
        }
    }
}

#[derive(Clone)]
pub struct MessageService {
    provider: Arc<dyn ChatProvider>,
    memory: Arc<dyn MemoryBackend>,
    mcp_runtime: Option<Arc<RwLock<McpRuntime>>>,
    agent_mcp: AgentMcpSettings,
    max_recent_turns: usize,
    max_semantic_memories: usize,
    semantic_lookback_days: u32,
    context_window_tokens: usize,
    context_reserved_tokens: usize,
    context_memory_budget_ratio: u8,
    context_min_recent_messages: usize,
}

impl MessageService {
    pub fn new(
        provider: Arc<dyn ChatProvider>,
        store: SqliteMemoryStore,
        max_history: usize,
    ) -> Self {
        Self::new_with_backend(
            provider,
            Arc::new(SqliteMemoryBackend::new(store)),
            None,
            AgentMcpSettings::default(),
            max_history,
            0,
            0,
        )
    }

    pub fn new_with_backend(
        provider: Arc<dyn ChatProvider>,
        memory: Arc<dyn MemoryBackend>,
        mcp_runtime: Option<Arc<RwLock<McpRuntime>>>,
        agent_mcp: AgentMcpSettings,
        max_recent_turns: usize,
        max_semantic_memories: usize,
        semantic_lookback_days: u32,
    ) -> Self {
        Self {
            provider,
            memory,
            mcp_runtime,
            agent_mcp,
            max_recent_turns,
            max_semantic_memories,
            semantic_lookback_days,
            context_window_tokens: 200_000,
            context_reserved_tokens: 8_192,
            context_memory_budget_ratio: 35,
            context_min_recent_messages: 8,
        }
    }

    pub fn with_context_budget(
        mut self,
        window_tokens: usize,
        reserved_tokens: usize,
        memory_budget_ratio: u8,
        min_recent_messages: usize,
    ) -> Self {
        self.context_window_tokens = window_tokens;
        self.context_reserved_tokens = reserved_tokens;
        self.context_memory_budget_ratio = memory_budget_ratio;
        self.context_min_recent_messages = min_recent_messages;
        self
    }

    pub async fn handle(&self, incoming: IncomingMessage) -> anyhow::Result<OutgoingMessage> {
        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::User,
                    content: incoming.text.clone(),
                },
            })
            .await
            .context("failed to persist user message")?;

        let history = self
            .memory
            .load_context(MemoryContextRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                query_text: incoming.text.clone(),
                max_recent_turns: self.max_recent_turns,
                max_semantic_memories: self.max_semantic_memories,
                semantic_lookback_days: self.semantic_lookback_days,
            })
            .await
            .context("failed to load history")?;
        let history = apply_context_budget(
            history,
            self.context_window_tokens,
            self.context_reserved_tokens,
            self.context_memory_budget_ratio,
            self.context_min_recent_messages,
        );

        let text = self.complete_with_optional_mcp(history).await?;

        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::Assistant,
                    content: text.clone(),
                },
            })
            .await
            .context("failed to persist assistant message")?;

        Ok(OutgoingMessage {
            channel: incoming.channel,
            session_id: incoming.session_id,
            text,
            reply_target: incoming.reply_target,
        })
    }

    pub async fn handle_stream(
        &self,
        incoming: IncomingMessage,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<OutgoingMessage> {
        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::User,
                    content: incoming.text.clone(),
                },
            })
            .await
            .context("failed to persist user message")?;

        let history = self
            .memory
            .load_context(MemoryContextRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                query_text: incoming.text.clone(),
                max_recent_turns: self.max_recent_turns,
                max_semantic_memories: self.max_semantic_memories,
                semantic_lookback_days: self.semantic_lookback_days,
            })
            .await
            .context("failed to load history")?;
        let history = apply_context_budget(
            history,
            self.context_window_tokens,
            self.context_reserved_tokens,
            self.context_memory_budget_ratio,
            self.context_min_recent_messages,
        );

        // Keep true streaming semantics on channel streaming path.
        // MCP tool loop currently runs on non-stream path only.
        let text = self
            .provider
            .complete_stream(CompletionRequest { messages: history }, sink)
            .await
            .context("provider stream completion failed")?;

        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::Assistant,
                    content: text.clone(),
                },
            })
            .await
            .context("failed to persist assistant message")?;

        Ok(OutgoingMessage {
            channel: incoming.channel,
            session_id: incoming.session_id,
            text,
            reply_target: incoming.reply_target,
        })
    }

    pub async fn observe(&self, incoming: IncomingMessage) -> anyhow::Result<()> {
        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id,
                user_id: incoming.user_id,
                channel: incoming.channel,
                message: StoredMessage {
                    role: MessageRole::User,
                    content: incoming.text,
                },
            })
            .await
            .context("failed to persist observed user message")
    }

    pub async fn upsert_group_aliases(
        &self,
        channel: String,
        chat_id: i64,
        aliases: Vec<String>,
    ) -> anyhow::Result<()> {
        if aliases.is_empty() {
            return Ok(());
        }
        self.memory
            .upsert_group_aliases(GroupAliasUpsertRequest {
                channel,
                chat_id,
                aliases,
            })
            .await
            .context("failed to persist telegram group aliases")
    }

    pub async fn load_group_aliases(
        &self,
        channel: String,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        self.memory
            .load_group_aliases(GroupAliasLoadRequest {
                channel,
                chat_id,
                limit,
            })
            .await
            .context("failed to load telegram group aliases")
    }

    pub async fn upsert_group_user_profile(
        &self,
        channel: String,
        chat_id: i64,
        user_id: i64,
        preferred_name: String,
        username: Option<String>,
    ) -> anyhow::Result<()> {
        if preferred_name.trim().is_empty() {
            return Ok(());
        }
        self.memory
            .upsert_group_user_profile(GroupUserProfileUpsertRequest {
                channel,
                chat_id,
                user_id,
                preferred_name,
                username,
            })
            .await
            .context("failed to persist telegram group user profile")
    }

    pub async fn load_group_user_profiles(
        &self,
        channel: String,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        self.memory
            .load_group_user_profiles(GroupUserProfileLoadRequest {
                channel,
                chat_id,
                limit,
            })
            .await
            .context("failed to load telegram group user profiles")
    }

    pub async fn create_telegram_scheduler_job(
        &self,
        req: CreateTelegramSchedulerJobRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .create_telegram_scheduler_job(req)
            .await
            .context("failed to create telegram scheduler job")
    }

    pub async fn list_telegram_scheduler_jobs_by_owner(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.memory
            .list_telegram_scheduler_jobs_by_owner(TelegramSchedulerJobListRequest {
                channel,
                chat_id,
                owner_user_id,
                limit,
            })
            .await
            .context("failed to list telegram scheduler jobs")
    }

    pub async fn query_telegram_scheduler_stats(
        &self,
        channel: String,
        now_unix: i64,
    ) -> anyhow::Result<TelegramSchedulerStats> {
        self.memory
            .query_telegram_scheduler_stats(TelegramSchedulerStatsRequest { channel, now_unix })
            .await
            .context("failed to query telegram scheduler stats")
    }

    pub async fn load_telegram_scheduler_job(
        &self,
        channel: String,
        job_id: String,
    ) -> anyhow::Result<Option<TelegramSchedulerJobRecord>> {
        self.memory
            .load_telegram_scheduler_job(channel, job_id)
            .await
            .context("failed to load telegram scheduler job")
    }

    pub async fn update_telegram_scheduler_job_status(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        job_id: String,
        status: TelegramSchedulerJobStatus,
    ) -> anyhow::Result<bool> {
        self.memory
            .update_telegram_scheduler_job_status(UpdateTelegramSchedulerJobStatusRequest {
                channel,
                chat_id,
                owner_user_id,
                job_id,
                status,
            })
            .await
            .context("failed to update telegram scheduler job status")
    }

    pub async fn claim_due_telegram_scheduler_jobs(
        &self,
        channel: String,
        now_unix: i64,
        limit: usize,
        lease_secs: i64,
        lease_token: String,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.memory
            .claim_due_telegram_scheduler_jobs(ClaimDueTelegramSchedulerJobsRequest {
                channel,
                now_unix,
                limit,
                lease_secs,
                lease_token,
            })
            .await
            .context("failed to claim due telegram scheduler jobs")
    }

    pub async fn complete_telegram_scheduler_job_run(
        &self,
        req: CompleteTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .complete_telegram_scheduler_job_run(req)
            .await
            .context("failed to complete telegram scheduler job run")
    }

    pub async fn fail_telegram_scheduler_job_run(
        &self,
        req: FailTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .fail_telegram_scheduler_job_run(req)
            .await
            .context("failed to fail telegram scheduler job run")
    }

    pub async fn upsert_telegram_scheduler_pending_intent(
        &self,
        req: UpsertTelegramSchedulerPendingIntentRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .upsert_telegram_scheduler_pending_intent(req)
            .await
            .context("failed to upsert telegram scheduler pending intent")
    }

    pub async fn load_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        now_unix: i64,
    ) -> anyhow::Result<Option<TelegramSchedulerPendingIntentRecord>> {
        self.memory
            .load_telegram_scheduler_pending_intent(channel, chat_id, owner_user_id, now_unix)
            .await
            .context("failed to load telegram scheduler pending intent")
    }

    pub async fn delete_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
    ) -> anyhow::Result<bool> {
        self.memory
            .delete_telegram_scheduler_pending_intent(channel, chat_id, owner_user_id)
            .await
            .context("failed to delete telegram scheduler pending intent")
    }

    pub async fn detect_telegram_scheduler_intent(
        &self,
        text: &str,
        timezone: &str,
        now_unix: i64,
        pending_draft_json: Option<&str>,
    ) -> anyhow::Result<Option<TelegramSchedulerIntent>> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }

        let mut input =
            format!("当前时间戳(now_unix): {now_unix}\n默认时区: {timezone}\n用户输入: {text}");
        if let Some(draft) = pending_draft_json
            && !draft.trim().is_empty()
        {
            input.push_str("\n待确认草案(JSON): ");
            input.push_str(draft.trim());
        }

        let reply = self
            .provider
            .complete(CompletionRequest {
                messages: vec![
                    StoredMessage {
                        role: MessageRole::System,
                        content: [
                            "你是 Telegram 定时任务意图解析器。",
                            "只输出 JSON，不要 Markdown，不要解释。",
                            "JSON schema:",
                            "{\"action\":\"create|update|delete|pause|resume|cancel|list|none\",\"confidence\":0..1,\"task_kind\":\"reminder|agent|null\",\"payload\":\"string|null\",\"schedule_kind\":\"once|cron|null\",\"run_at\":\"RFC3339或unix秒字符串|null\",\"cron_expr\":\"string|null\",\"timezone\":\"IANA时区或null\",\"job_id\":\"string|null\",\"job_operation\":\"delete|pause|resume|null\"}",
                            "规则:",
                            "1) 若不是定时任务意图，action=none, confidence<=0.4",
                            "2) 若有明确时间并要求提醒，优先 action=create",
                            "3) 对修改已存在草案可输出 action=update",
                            "4) 若表达暂停/恢复/删除已存在任务，输出 action=pause|resume|delete，并尽量给出 job_id",
                            "4) 只返回一个合法 JSON 对象",
                        ]
                        .join("\n"),
                    },
                    StoredMessage {
                        role: MessageRole::User,
                        content: input,
                    },
                ],
            })
            .await
            .context("failed to detect telegram scheduler intent")?;

        Ok(parse_scheduler_intent_json(&reply))
    }

    async fn complete_with_optional_mcp(
        &self,
        history: Vec<StoredMessage>,
    ) -> anyhow::Result<String> {
        let Some(runtime) = self.snapshot_mcp_runtime().await else {
            return self
                .provider
                .complete(CompletionRequest { messages: history })
                .await
                .context("provider completion failed");
        };
        let tools = match runtime.list_tools(None).await {
            Ok(tools) => tools,
            Err(err) => {
                warn!(error = %err, "failed to list mcp tools, fallback to plain completion");
                return self
                    .provider
                    .complete(CompletionRequest { messages: history })
                    .await
                    .context("provider completion failed");
            }
        };

        if tools.is_empty() {
            return self
                .provider
                .complete(CompletionRequest { messages: history })
                .await
                .context("provider completion failed");
        }

        self.complete_with_mcp_loop(history, tools, runtime).await
    }

    async fn complete_with_mcp_loop(
        &self,
        mut history: Vec<StoredMessage>,
        tools: Vec<McpToolInfo>,
        runtime: McpRuntime,
    ) -> anyhow::Result<String> {
        history.push(StoredMessage {
            role: MessageRole::System,
            content: build_mcp_system_prompt(&tools)?,
        });

        let max_iterations = self.agent_mcp.max_iterations.max(1);
        for _ in 0..max_iterations {
            let reply = self
                .provider
                .complete(CompletionRequest {
                    messages: history.clone(),
                })
                .await
                .context("provider completion failed")?;

            let Some(tool_call) = parse_mcp_tool_call(&reply) else {
                return Ok(reply);
            };

            let tool_result = runtime
                .call_tool(
                    &tool_call.server,
                    &tool_call.tool,
                    tool_call.arguments.clone(),
                )
                .await;

            let tool_message = match tool_result {
                Ok(value) => {
                    let result = truncate_json_value(&value, self.agent_mcp.max_tool_result_chars);
                    serde_json::json!({
                        "server": tool_call.server,
                        "tool": tool_call.tool,
                        "ok": true,
                        "result": result
                    })
                }
                Err(err) => serde_json::json!({
                    "server": tool_call.server,
                    "tool": tool_call.tool,
                    "ok": false,
                    "error": err.to_string()
                }),
            };

            history.push(StoredMessage {
                role: MessageRole::Assistant,
                content: reply,
            });
            history.push(StoredMessage {
                role: MessageRole::System,
                content: format!(
                    "MCP_TOOL_RESULT_JSON:\n{}",
                    serde_json::to_string(&tool_message)
                        .unwrap_or_else(|_| "{\"ok\":false}".to_string())
                ),
            });
        }

        history.push(StoredMessage {
            role: MessageRole::System,
            content: "MCP tool loop reached max iterations. Give a final answer based on available context."
                .to_string(),
        });
        self.provider
            .complete(CompletionRequest { messages: history })
            .await
            .context("provider completion failed")
    }

    fn is_mcp_agent_enabled(&self) -> bool {
        self.agent_mcp.enabled && self.mcp_runtime.is_some()
    }

    async fn snapshot_mcp_runtime(&self) -> Option<McpRuntime> {
        if !self.is_mcp_agent_enabled() {
            return None;
        }
        let runtime = self.mcp_runtime.as_ref()?;
        Some(runtime.read().await.clone())
    }

    pub async fn reload_mcp_runtime_from_registry(
        &self,
        registry: &crate::mcp::McpRegistry,
    ) -> anyhow::Result<()> {
        let Some(runtime) = &self.mcp_runtime else {
            return Ok(());
        };
        let next = McpRuntime::from_registry(registry).await?;
        let mut guard = runtime.write().await;
        *guard = next;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ParsedMcpToolCall {
    server: String,
    tool: String,
    arguments: Value,
}

fn build_mcp_system_prompt(tools: &[McpToolInfo]) -> anyhow::Result<String> {
    let tool_defs = tools
        .iter()
        .take(64)
        .map(|t| {
            serde_json::json!({
                "server": t.server,
                "tool": t.name,
                "description": t.description,
                "input_schema": t.input_schema
            })
        })
        .collect::<Vec<_>>();
    let serialized = serde_json::to_string(&tool_defs).context("failed to encode mcp tool list")?;
    Ok(format!(
        "You can use MCP tools.\nWhen a tool call is needed, reply with ONLY JSON (no markdown, no extra text): {{\"server\":\"<server>\",\"tool\":\"<tool>\",\"arguments\":{{...}}}}.\nAvailable tools: {serialized}\nIf no tool is needed, reply with the final answer directly."
    ))
}

fn parse_mcp_tool_call(reply: &str) -> Option<ParsedMcpToolCall> {
    let json_text = extract_json_payload(reply.trim())?;
    let value: Value = serde_json::from_str(&json_text).ok()?;
    parse_mcp_tool_call_value(&value)
}

fn parse_scheduler_intent_json(reply: &str) -> Option<TelegramSchedulerIntent> {
    let json_text = extract_json_payload(reply.trim())?;
    let mut parsed: TelegramSchedulerIntent = serde_json::from_str(&json_text).ok()?;
    parsed.action = parsed.action.trim().to_ascii_lowercase();
    parsed.confidence = parsed.confidence.clamp(0.0, 1.0);
    parsed.task_kind = parsed
        .task_kind
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    parsed.schedule_kind = parsed
        .schedule_kind
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    parsed.payload = parsed
        .payload
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.run_at = parsed
        .run_at
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.cron_expr = parsed
        .cron_expr
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.timezone = parsed
        .timezone
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.job_id = parsed
        .job_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.job_operation = parsed
        .job_operation
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    Some(parsed)
}

fn extract_json_payload(text: &str) -> Option<String> {
    if text.starts_with("```") {
        let stripped = text.trim_start_matches("```");
        let stripped = stripped.strip_prefix("json").unwrap_or(stripped).trim();
        let stripped = stripped.strip_suffix("```").unwrap_or(stripped).trim();
        return Some(stripped.to_string());
    }
    Some(text.to_string())
}

fn parse_mcp_tool_call_value(value: &Value) -> Option<ParsedMcpToolCall> {
    if let Some(inner) = value.get("tool_call") {
        return parse_mcp_tool_call_value(inner);
    }
    if let Some(inner) = value.get("mcp_tool_call") {
        return parse_mcp_tool_call_value(inner);
    }
    if let Some(items) = value.as_array() {
        return items.first().and_then(parse_mcp_tool_call_value);
    }

    let obj = value.as_object()?;
    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    if let (Some(server), Some(tool)) = (
        obj.get("server").and_then(|v| v.as_str()),
        obj.get("tool").and_then(|v| v.as_str()),
    ) {
        return Some(ParsedMcpToolCall {
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
        });
    }

    let name = obj.get("name").and_then(|v| v.as_str())?;
    if let Some((server, tool)) = name.split_once("::").or_else(|| name.split_once('/')) {
        return Some(ParsedMcpToolCall {
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
        });
    }
    None
}

fn apply_context_budget(
    messages: Vec<StoredMessage>,
    context_window_tokens: usize,
    context_reserved_tokens: usize,
    memory_budget_ratio: u8,
    min_recent_messages: usize,
) -> Vec<StoredMessage> {
    if messages.is_empty() || context_window_tokens == 0 {
        return messages;
    }

    let input_budget = context_window_tokens.saturating_sub(context_reserved_tokens);
    if input_budget == 0 {
        return messages
            .into_iter()
            .last()
            .map(|m| vec![m])
            .unwrap_or_default();
    }

    let total_tokens = messages.iter().map(estimate_message_tokens).sum::<usize>();
    if total_tokens <= input_budget {
        return messages;
    }

    let mut selected = vec![false; messages.len()];
    let mut replacements: HashMap<usize, StoredMessage> = HashMap::new();
    let mut used = 0usize;
    let capped_ratio = memory_budget_ratio.clamp(0, 80) as usize;
    let memory_budget = input_budget.saturating_mul(capped_ratio) / 100;

    let mut memory_candidates = messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            if msg.role != MessageRole::System {
                return None;
            }
            let score = parse_memory_score(&msg.content)?;
            Some((idx, score, estimate_message_tokens(msg)))
        })
        .collect::<Vec<_>>();
    memory_candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.cmp(&a.0))
    });

    for (idx, _score, tokens) in memory_candidates {
        if used + tokens > memory_budget {
            continue;
        }
        selected[idx] = true;
        used += tokens;
    }

    let mut recent_kept = 0usize;
    for idx in (0..messages.len()).rev() {
        if selected[idx] {
            continue;
        }
        let tokens = estimate_message_tokens(&messages[idx]);
        if used + tokens > input_budget {
            if recent_kept < min_recent_messages {
                let remain = input_budget.saturating_sub(used);
                if remain > 12 {
                    let truncated = truncate_message_to_token_budget(&messages[idx], remain);
                    let truncated_tokens = estimate_message_tokens(&truncated);
                    if truncated_tokens > 0 {
                        selected[idx] = true;
                        used = used.saturating_add(truncated_tokens).min(input_budget);
                        replacements.insert(idx, truncated);
                    }
                    recent_kept += 1;
                }
            }
            continue;
        }
        selected[idx] = true;
        used += tokens;
        recent_kept += 1;
        if used >= input_budget {
            break;
        }
    }

    for idx in (0..messages.len()).rev() {
        if used >= input_budget {
            break;
        }
        if selected[idx] {
            continue;
        }
        let tokens = estimate_message_tokens(&messages[idx]);
        if used + tokens > input_budget {
            continue;
        }
        selected[idx] = true;
        used += tokens;
    }

    if !selected.iter().any(|v| *v)
        && let Some(last_idx) = messages.len().checked_sub(1)
    {
        selected[last_idx] = true;
    }

    messages
        .into_iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            if !selected[idx] {
                return None;
            }
            Some(replacements.remove(&idx).unwrap_or(msg))
        })
        .collect()
}

fn estimate_message_tokens(msg: &StoredMessage) -> usize {
    estimate_text_tokens(&msg.content).saturating_add(4)
}

fn estimate_text_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    (chars.saturating_add(3) / 4).max(1)
}

fn parse_memory_score(content: &str) -> Option<f32> {
    let marker = "[memory score=";
    let start = content.find(marker)? + marker.len();
    let tail = &content[start..];
    let end = tail.find([' ', ']'])?;
    tail[..end].trim().parse::<f32>().ok()
}

fn truncate_message_to_token_budget(msg: &StoredMessage, max_tokens: usize) -> StoredMessage {
    if max_tokens == 0 {
        return StoredMessage {
            role: msg.role.clone(),
            content: String::new(),
        };
    }
    if estimate_message_tokens(msg) <= max_tokens {
        return msg.clone();
    }

    let max_chars = max_tokens.saturating_mul(4).saturating_sub(12);
    let mut content = msg
        .content
        .chars()
        .take(max_chars.max(8))
        .collect::<String>();
    if !content.is_empty() {
        content.push_str(" ...(truncated)");
    }
    StoredMessage {
        role: msg.role.clone(),
        content,
    }
}

fn truncate_json_value(value: &Value, max_chars: usize) -> Value {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    if encoded.chars().count() <= max_chars {
        return value.clone();
    }
    let mut out = encoded.chars().take(max_chars).collect::<String>();
    out.push_str("...(truncated)");
    serde_json::json!({ "truncated": out })
}

#[cfg(test)]
mod tests {
    use super::{
        apply_context_budget, parse_mcp_tool_call, parse_scheduler_intent_json, truncate_json_value,
    };
    use crate::domain::{MessageRole, StoredMessage};

    #[test]
    fn parse_tool_call_plain_json() {
        let parsed =
            parse_mcp_tool_call(r#"{"server":"search","tool":"web","arguments":{"q":"rust mcp"}}"#)
                .expect("parsed");
        assert_eq!(parsed.server, "search");
        assert_eq!(parsed.tool, "web");
    }

    #[test]
    fn parse_tool_call_from_fenced_json() {
        let parsed = parse_mcp_tool_call(
            "```json\n{\"tool_call\":{\"server\":\"s1\",\"tool\":\"t1\",\"arguments\":{}}}\n```",
        )
        .expect("parsed");
        assert_eq!(parsed.server, "s1");
        assert_eq!(parsed.tool, "t1");
    }

    #[test]
    fn parse_tool_call_name_compact_format() {
        let parsed =
            parse_mcp_tool_call(r#"{"name":"s2::t2","arguments":{"x":1}}"#).expect("parsed");
        assert_eq!(parsed.server, "s2");
        assert_eq!(parsed.tool, "t2");
    }

    #[test]
    fn truncate_json_value_caps_size() {
        let value = serde_json::json!({ "long": "abcdefghijklmnopqrstuvwxyz" });
        let truncated = truncate_json_value(&value, 12);
        assert!(truncated.get("truncated").is_some());
    }

    #[test]
    fn parse_scheduler_intent_json_normalizes_fields() {
        let parsed = parse_scheduler_intent_json(
            r#"{"action":" CREATE ","confidence":1.2,"task_kind":"Reminder","payload":"  明早开会  ","schedule_kind":"Once","run_at":" 2026-03-01T09:00:00+08:00 ","cron_expr":"","timezone":" Asia/Shanghai ","job_id":" ","job_operation":" DELETE "}"#,
        )
        .expect("parsed");
        assert_eq!(parsed.action, "create");
        assert_eq!(parsed.confidence, 1.0);
        assert_eq!(parsed.task_kind.as_deref(), Some("reminder"));
        assert_eq!(parsed.payload.as_deref(), Some("明早开会"));
        assert_eq!(parsed.schedule_kind.as_deref(), Some("once"));
        assert_eq!(parsed.run_at.as_deref(), Some("2026-03-01T09:00:00+08:00"));
        assert!(parsed.cron_expr.is_none());
        assert_eq!(parsed.timezone.as_deref(), Some("Asia/Shanghai"));
        assert!(parsed.job_id.is_none());
        assert_eq!(parsed.job_operation.as_deref(), Some("delete"));
    }

    #[test]
    fn context_budget_prefers_high_score_memory_and_latest_turns() {
        let messages = vec![
            StoredMessage {
                role: MessageRole::System,
                content: "[memory score=0.950 src=vec] Rust trait object supports dynamic dispatch"
                    .to_string(),
            },
            StoredMessage {
                role: MessageRole::System,
                content: "[memory score=0.120 src=vec]".to_string()
                    + &"irrelevant old memory".repeat(80),
            },
            StoredMessage {
                role: MessageRole::User,
                content: "first question about rust".repeat(20),
            },
            StoredMessage {
                role: MessageRole::Assistant,
                content: "first answer".repeat(20),
            },
            StoredMessage {
                role: MessageRole::User,
                content: "latest question: explain trait object".to_string(),
            },
        ];

        let trimmed = apply_context_budget(messages, 180, 40, 35, 2);

        assert!(
            trimmed
                .iter()
                .any(|m| m.content.contains("score=0.950") && m.role == MessageRole::System)
        );
        assert!(
            trimmed
                .iter()
                .any(|m| m.content.contains("latest question") && m.role == MessageRole::User)
        );
        assert!(
            !trimmed
                .iter()
                .any(|m| m.content.contains("score=0.120") && m.role == MessageRole::System)
        );
    }
}
