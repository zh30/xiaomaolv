use std::sync::Arc;

use anyhow::Context;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::warn;

use crate::domain::{IncomingMessage, MessageRole, OutgoingMessage, StoredMessage};
use crate::mcp::{McpRuntime, McpToolInfo};
use crate::memory::{
    MemoryBackend, MemoryContextRequest, MemoryWriteRequest, SqliteMemoryBackend, SqliteMemoryStore,
};
use crate::provider::{ChatProvider, CompletionRequest, StreamSink};

#[derive(Debug, Clone)]
pub struct AgentMcpSettings {
    pub enabled: bool,
    pub max_iterations: usize,
    pub max_tool_result_chars: usize,
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
        }
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
    use super::{parse_mcp_tool_call, truncate_json_value};

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
}
