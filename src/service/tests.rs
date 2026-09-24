fn truncate_json_value(value: &serde_json::Value, max_chars: usize) -> serde_json::Value {
    crate::harness::tool_protocol::truncate_json_value(value, max_chars)
}

use super::completion::{
    CodeModeCircuitChange, build_mcp_system_prompt, chunk_text_for_stream_replay,
    code_mode_timeout_ratio, next_code_mode_timeout_streak, parse_mcp_tool_call,
    should_open_code_mode_timeout_circuit, should_probe_code_mode_timeout_circuit,
    should_warn_code_mode_timeouts,
};
use super::delegates::parse_scheduler_intent_json;
use super::time_query::{
    extract_primary_user_text, infer_timezone_from_time_query, is_explicit_current_time_query,
    looks_like_time_query, render_fast_time_answer, weekday_to_chinese, zodiac_for_year,
};
use super::{
    AgentCompactionSettings, AgentMcpSettings, BUILTIN_MCP_SERVER_NAME,
    BUILTIN_MCP_TOOL_CURRENT_TIME, MessageService, apply_context_budget,
    build_compaction_source_key,
};
use crate::code_mode::{AgentCodeModeSettings, CodeModeAuditRecord};
use crate::domain::{MessageRole, StoredMessage};
use crate::harness::compactor::{CompactionMessageMetadata, CompactionStrategy};
use crate::mcp::McpRuntime;
use crate::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
use crate::provider::{ChatProvider, CompletionRequest, StreamSink};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

struct FakeProvider;

#[async_trait::async_trait]
impl ChatProvider for FakeProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }
}

struct CountingSummaryProvider {
    summary_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ChatProvider for CountingSummaryProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        if req
            .messages
            .first()
            .is_some_and(|message| message.content.starts_with("Summarize the following"))
        {
            self.summary_calls.fetch_add(1, Ordering::SeqCst);
            return Ok("cached reusable summary".to_string());
        }
        Ok("ok".to_string())
    }
}

struct NeverProvider;

#[async_trait::async_trait]
impl ChatProvider for NeverProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        anyhow::bail!("provider should not be called in time fast path")
    }

    async fn complete_stream(
        &self,
        _req: CompletionRequest,
        _sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        anyhow::bail!("provider stream should not be called in time fast path")
    }
}

struct SequenceStreamProvider {
    replies: Arc<Mutex<Vec<String>>>,
}

impl SequenceStreamProvider {
    fn next_reply(&self) -> String {
        let mut guard = self.replies.lock().expect("reply lock");
        if guard.is_empty() {
            String::new()
        } else {
            guard.remove(0)
        }
    }
}

#[async_trait::async_trait]
impl ChatProvider for SequenceStreamProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok(self.next_reply())
    }

    async fn complete_stream(
        &self,
        _req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        let reply = self.next_reply();
        let chars = reply.chars().collect::<Vec<_>>();
        for chunk in chars.chunks(6) {
            let delta = chunk.iter().collect::<String>();
            sink.on_delta(&delta).await?;
        }
        Ok(reply)
    }
}

struct SequenceNoDeltaStreamProvider {
    replies: Arc<Mutex<Vec<String>>>,
}

impl SequenceNoDeltaStreamProvider {
    fn next_reply(&self) -> String {
        let mut guard = self.replies.lock().expect("reply lock");
        if guard.is_empty() {
            String::new()
        } else {
            guard.remove(0)
        }
    }
}

#[async_trait::async_trait]
impl ChatProvider for SequenceNoDeltaStreamProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok(self.next_reply())
    }

    async fn complete_stream(
        &self,
        _req: CompletionRequest,
        _sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        Ok(self.next_reply())
    }
}

#[derive(Default)]
struct CaptureSink {
    text: String,
}

#[async_trait::async_trait]
impl StreamSink for CaptureSink {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        self.text.push_str(delta);
        Ok(())
    }
}

#[test]
fn parse_tool_call_plain_json() {
    let parsed =
        parse_mcp_tool_call(r#"{"server":"search","tool":"web","arguments":{"q":"rust mcp"}}"#)
            .expect("parsed");
    assert_eq!(parsed.server, "search");
    assert_eq!(parsed.tool, "web");
}

#[test]
fn stream_replay_chunker_preserves_text_and_bounds_chunk_size() {
    let text = "abcdefghijklmnopqrstuvwxyz";
    let chunks = chunk_text_for_stream_replay(text, 5);
    assert_eq!(chunks.concat(), text);
    assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 5));
    assert!(chunks.len() > 1);
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
    let parsed = parse_mcp_tool_call(r#"{"name":"s2::t2","arguments":{"x":1}}"#).expect("parsed");
    assert_eq!(parsed.server, "s2");
    assert_eq!(parsed.tool, "t2");
}

#[test]
fn mcp_system_prompt_includes_builtin_time_rule() {
    let prompt = build_mcp_system_prompt(&[]).expect("prompt");
    assert!(prompt.contains(BUILTIN_MCP_SERVER_NAME));
    assert!(prompt.contains(BUILTIN_MCP_TOOL_CURRENT_TIME));
    assert!(prompt.contains("Time rule"));
}

#[test]
fn looks_like_time_query_detects_cn_and_en_intents() {
    assert!(looks_like_time_query("现在几点了？"));
    assert!(looks_like_time_query("今天几号"));
    assert!(looks_like_time_query("今年是什么生肖"));
    assert!(looks_like_time_query("what time is it now"));
}

#[test]
fn looks_like_time_query_ignores_regular_reminder_text() {
    assert!(!looks_like_time_query("5分钟后提醒我喝水"));
    assert!(!looks_like_time_query("驴哥，帮我总结一下上面的讨论"));
}

#[test]
fn extract_primary_user_text_reads_group_wrapped_message() {
    let wrapped =
        "[群成员身份映射]\n当前发言账号: uid=1\n原始消息: 现在美国时间是几点？\n[/群成员身份映射]";
    assert_eq!(extract_primary_user_text(wrapped), "现在美国时间是几点？");
}

#[test]
fn time_query_timezone_inference_defaults_us_to_eastern() {
    let inferred = infer_timezone_from_time_query("现在美国时间是几点？").expect("tz");
    assert_eq!(inferred.timezone, "America/New_York");
    assert_eq!(inferred.source, "us_default");
}

#[test]
fn explicit_time_query_filter_ignores_scheduler_sentence() {
    assert!(!is_explicit_current_time_query("2分钟后提醒我看看时间"));
    assert!(is_explicit_current_time_query("今天是星期几？"));
}

#[test]
fn render_fast_time_answer_formats_weekday_and_year() {
    let answer = render_fast_time_answer(
        "今天是星期几，今年什么生肖？",
        &serde_json::json!({
            "local_date": "2026-02-26",
            "local_time": "09:32:00",
            "local_weekday": "Thursday",
            "local_year": 2026,
            "timezone": "Asia/Shanghai"
        }),
        None,
    );
    assert!(answer.contains("星期四"));
    assert!(answer.contains("2026"));
    assert!(answer.contains("马年"));
}

#[test]
fn weekday_and_zodiac_helpers_return_expected_values() {
    assert_eq!(weekday_to_chinese("Monday"), "一");
    assert_eq!(weekday_to_chinese("Thursday"), "四");
    assert_eq!(zodiac_for_year(2025), "蛇年");
    assert_eq!(zodiac_for_year(2026), "马年");
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
fn parse_scheduler_intent_json_extracts_object_from_wrapped_text() {
    let parsed = parse_scheduler_intent_json(
        "好的，已解析如下：\n```json\n{\"action\":\"create\",\"confidence\":0.92,\"task_kind\":\"reminder\",\"payload\":\"喝水\",\"schedule_kind\":\"once\",\"run_at\":\"1772067600\",\"cron_expr\":null,\"timezone\":\"Asia/Shanghai\",\"job_id\":null,\"job_operation\":null}\n```\n请确认",
    )
    .expect("parsed");
    assert_eq!(parsed.action, "create");
    assert_eq!(parsed.payload.as_deref(), Some("喝水"));
}

#[test]
fn parse_mcp_tool_call_extracts_object_from_wrapped_text() {
    let parsed = parse_mcp_tool_call(
        "我准备调用工具：\n{\"server\":\"builtin\",\"tool\":\"current_time\",\"arguments\":{\"timezone\":\"Asia/Shanghai\"}}\n然后返回结果",
    )
    .expect("parsed");
    assert_eq!(parsed.server, "builtin");
    assert_eq!(parsed.tool, "current_time");
    assert_eq!(parsed.arguments["timezone"], "Asia/Shanghai");
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

#[tokio::test]
async fn compaction_reuses_persisted_summary_for_same_source_window() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    for idx in 0..12 {
        store
            .append(
                "session-compact-cache",
                StoredMessage {
                    role: if idx % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    },
                    content: format!("Message {idx} {}", "long context ".repeat(24)),
                },
            )
            .await
            .expect("append message");
    }

    let summary_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingSummaryProvider {
        summary_calls: summary_calls.clone(),
    });
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store.clone()));
    let service = MessageService::new_with_backend(
        provider,
        memory,
        None,
        AgentMcpSettings::default(),
        12,
        0,
        0,
    )
    .with_context_budget(4000, 512, 35, 4)
    .with_compaction(AgentCompactionSettings {
        enabled: true,
        strategy: CompactionStrategy::HeadTail {
            head_count: 1,
            tail_count: 1,
        },
        head_count: 1,
        tail_count: 1,
        message_count_threshold: 4,
    });
    let history = store
        .load_recent("session-compact-cache", 12)
        .await
        .expect("load history");

    let first = service
        .apply_compaction(history.clone(), "session-compact-cache")
        .await
        .expect("first compaction");
    let second = service
        .apply_compaction(history, "session-compact-cache")
        .await
        .expect("second compaction");

    assert_eq!(summary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(first, second);
    assert!(
        second
            .iter()
            .any(|message| message.content.contains("cached reusable summary"))
    );
}

#[test]
fn compaction_source_key_uses_stable_sha256_digest() {
    let history = vec![
        StoredMessage {
            role: MessageRole::User,
            content: "first".to_string(),
        },
        StoredMessage {
            role: MessageRole::Assistant,
            content: "second".to_string(),
        },
    ];
    let metadata = vec![
        CompactionMessageMetadata {
            source_id: Some(10),
            created_at: Some(1000),
        },
        CompactionMessageMetadata {
            source_id: Some(11),
            created_at: Some(1001),
        },
    ];

    let key = build_compaction_source_key(
        &history,
        &CompactionStrategy::HeadTail {
            head_count: 1,
            tail_count: 2,
        },
        &metadata,
    );

    assert_eq!(
        key.source_hash,
        "391ded3c62b5ac3c864f424fbf622c15bd2381556a366ef0e88e312731cccc3e"
    );
    assert_eq!(key.source_message_ids, vec![10, 11]);
    assert_eq!(key.source_first_message_id, Some(10));
    assert_eq!(key.source_last_message_id, Some(11));
    assert_eq!(key.source_message_count, 2);
}

#[test]
fn code_mode_timeout_ratio_warning_threshold_works() {
    let low = CodeModeAuditRecord {
        planner: "p".to_string(),
        used: true,
        fallback: false,
        reason: None,
        planned_calls: 5,
        executed_calls: 5,
        failed_calls: 1,
        timed_out_calls: 1,
        elapsed_ms: 100,
    };
    assert_eq!(code_mode_timeout_ratio(&low), Some(0.2));
    assert!(!should_warn_code_mode_timeouts(&low, 0.4));

    let high = CodeModeAuditRecord {
        timed_out_calls: 2,
        ..low.clone()
    };
    assert_eq!(code_mode_timeout_ratio(&high), Some(0.4));
    assert!(should_warn_code_mode_timeouts(&high, 0.4));

    let no_exec = CodeModeAuditRecord {
        executed_calls: 0,
        failed_calls: 0,
        timed_out_calls: 0,
        ..low
    };
    assert_eq!(code_mode_timeout_ratio(&no_exec), None);
    assert!(!should_warn_code_mode_timeouts(&no_exec, 0.4));
}

#[test]
fn code_mode_timeout_streak_and_circuit_rules_work() {
    let settings = AgentCodeModeSettings {
        shadow_mode: false,
        timeout_auto_shadow_enabled: true,
        timeout_auto_shadow_streak: 3,
        ..AgentCodeModeSettings::default()
    };
    assert!(!should_open_code_mode_timeout_circuit(2, &settings));
    assert!(should_open_code_mode_timeout_circuit(3, &settings));

    let disabled = AgentCodeModeSettings {
        timeout_auto_shadow_enabled: false,
        timeout_auto_shadow_streak: 1,
        ..AgentCodeModeSettings::default()
    };
    assert!(!should_open_code_mode_timeout_circuit(99, &disabled));

    let forced_shadow = AgentCodeModeSettings {
        shadow_mode: true,
        timeout_auto_shadow_enabled: true,
        timeout_auto_shadow_streak: 1,
        ..AgentCodeModeSettings::default()
    };
    assert!(!should_open_code_mode_timeout_circuit(99, &forced_shadow));

    let high = CodeModeAuditRecord {
        planner: "p".to_string(),
        used: true,
        fallback: false,
        reason: None,
        planned_calls: 5,
        executed_calls: 5,
        failed_calls: 2,
        timed_out_calls: 2,
        elapsed_ms: 100,
    };
    let low = CodeModeAuditRecord {
        timed_out_calls: 0,
        ..high.clone()
    };

    assert_eq!(next_code_mode_timeout_streak(0, &high, 0.4), 1);
    assert_eq!(next_code_mode_timeout_streak(2, &high, 0.4), 3);
    assert_eq!(next_code_mode_timeout_streak(2, &low, 0.4), 0);
}

#[test]
fn code_mode_probe_schedule_works() {
    assert!(!should_probe_code_mode_timeout_circuit(1, 5));
    assert!(!should_probe_code_mode_timeout_circuit(4, 5));
    assert!(should_probe_code_mode_timeout_circuit(5, 5));
    assert!(should_probe_code_mode_timeout_circuit(10, 5));
    assert!(should_probe_code_mode_timeout_circuit(1, 0));
}

#[tokio::test]
async fn code_mode_diagnostics_counters_accumulate() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(FakeProvider), store, 8);

    let audit = CodeModeAuditRecord {
        planner: "p".to_string(),
        used: false,
        fallback: true,
        reason: Some("timeout circuit probe".to_string()),
        planned_calls: 2,
        executed_calls: 2,
        failed_calls: 1,
        timed_out_calls: 1,
        elapsed_ms: 42,
    };
    service.record_code_mode_counters(&audit, true, CodeModeCircuitChange::Opened);

    let diag = service.code_mode_diagnostics();
    assert_eq!(diag.runtime.counters.attempts_total, 1);
    assert_eq!(diag.runtime.counters.used_total, 0);
    assert_eq!(diag.runtime.counters.fallback_total, 1);
    assert_eq!(diag.runtime.counters.failed_calls_total, 1);
    assert_eq!(diag.runtime.counters.timed_out_calls_total, 1);
    assert_eq!(diag.runtime.counters.probe_attempt_total, 1);
    assert_eq!(diag.runtime.counters.circuit_open_total, 1);
    assert_eq!(diag.runtime.counters.circuit_close_total, 0);
}

#[tokio::test]
async fn code_mode_prometheus_metrics_render_counter_values() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(FakeProvider), store, 8);

    let audit = CodeModeAuditRecord {
        planner: "p".to_string(),
        used: false,
        fallback: true,
        reason: Some("timeout circuit probe".to_string()),
        planned_calls: 2,
        executed_calls: 2,
        failed_calls: 1,
        timed_out_calls: 1,
        elapsed_ms: 42,
    };
    service.record_code_mode_counters(&audit, true, CodeModeCircuitChange::Opened);

    let body = service.code_mode_metrics_prometheus();
    assert!(body.contains("xiaomaolv_code_mode_attempts_total 1"));
    assert!(body.contains("xiaomaolv_code_mode_fallback_total 1"));
    assert!(body.contains("xiaomaolv_code_mode_timed_out_calls_total 1"));
    assert!(body.contains("xiaomaolv_code_mode_circuit_open_total 1"));
    assert!(body.contains("xiaomaolv_code_mode_timeout_warn_ratio 0.400000"));
    assert!(body.contains("xiaomaolv_code_mode_timeout_auto_shadow_probe_every 5"));
}

#[tokio::test]
async fn handle_stream_supports_mcp_tool_loop() {
    let provider = Arc::new(SequenceStreamProvider {
        replies: Arc::new(Mutex::new(vec![
            format!(
                r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"{BUILTIN_MCP_TOOL_CURRENT_TIME}","arguments":{{"timezone":"Asia/Shanghai"}}}}"#
            ),
            "现在是正确的当前时间。".to_string(),
        ])),
    });
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
    let service = MessageService::new_with_backend(
        provider,
        memory,
        Some(Arc::new(RwLock::new(McpRuntime::default()))),
        AgentMcpSettings::default(),
        8,
        0,
        0,
    );

    let mut sink = CaptureSink::default();
    let out = service
        .handle_stream(
            crate::domain::IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:test:stream".to_string(),
                user_id: "u1".to_string(),
                text: "请调用工具并给我最终结论".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("handle stream with mcp loop");

    assert_eq!(out.text, "现在是正确的当前时间。");
    assert_eq!(sink.text, "现在是正确的当前时间。");
    assert!(!sink.text.contains("\"server\""));
}

#[tokio::test]
async fn handle_stream_supports_multi_iteration_mcp_tool_loop() {
    let provider = Arc::new(SequenceStreamProvider {
        replies: Arc::new(Mutex::new(vec![
            format!(
                r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"{BUILTIN_MCP_TOOL_CURRENT_TIME}","arguments":{{"timezone":"Asia/Shanghai"}}}}"#
            ),
            format!(
                r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"{BUILTIN_MCP_TOOL_CURRENT_TIME}","arguments":{{"timezone":"America/New_York"}}}}"#
            ),
            "这是多轮工具调用后的最终回答。".to_string(),
        ])),
    });
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
    let service = MessageService::new_with_backend(
        provider,
        memory,
        Some(Arc::new(RwLock::new(McpRuntime::default()))),
        AgentMcpSettings::default(),
        8,
        0,
        0,
    );

    let mut sink = CaptureSink::default();
    let out = service
        .handle_stream(
            crate::domain::IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:test:stream-multi-loop".to_string(),
                user_id: "u1".to_string(),
                text: "请连续调用两次工具后再回答".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("handle stream with multi mcp loop");

    assert_eq!(out.text, "这是多轮工具调用后的最终回答。");
    assert_eq!(sink.text, "这是多轮工具调用后的最终回答。");
    assert!(!sink.text.contains("\"server\""));
}

#[tokio::test]
async fn handle_stream_supports_fenced_json_tool_call() {
    let provider = Arc::new(SequenceStreamProvider {
        replies: Arc::new(Mutex::new(vec![
            format!(
                "```json\n{{\"tool_call\":{{\"server\":\"{BUILTIN_MCP_SERVER_NAME}\",\"tool\":\"{BUILTIN_MCP_TOOL_CURRENT_TIME}\",\"arguments\":{{\"timezone\":\"Asia/Shanghai\"}}}}}}\n```"
            ),
            "fenced json tool call 已成功执行。".to_string(),
        ])),
    });
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
    let service = MessageService::new_with_backend(
        provider,
        memory,
        Some(Arc::new(RwLock::new(McpRuntime::default()))),
        AgentMcpSettings::default(),
        8,
        0,
        0,
    );

    let mut sink = CaptureSink::default();
    let out = service
        .handle_stream(
            crate::domain::IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:test:stream-fenced".to_string(),
                user_id: "u1".to_string(),
                text: "请用 fenced json 发起工具调用".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("handle stream with fenced json tool call");

    assert_eq!(out.text, "fenced json tool call 已成功执行。");
    assert_eq!(sink.text, "fenced json tool call 已成功执行。");
    assert!(!sink.text.contains("\"tool_call\""));
}

#[tokio::test]
async fn handle_stream_continues_after_mcp_tool_error() {
    let provider = Arc::new(SequenceStreamProvider {
        replies: Arc::new(Mutex::new(vec![
            format!(
                r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"no_such_tool","arguments":{{}}}}"#
            ),
            "我捕获到了工具调用失败，并继续给出最终回答。".to_string(),
        ])),
    });
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
    let service = MessageService::new_with_backend(
        provider,
        memory,
        Some(Arc::new(RwLock::new(McpRuntime::default()))),
        AgentMcpSettings::default(),
        8,
        0,
        0,
    );

    let mut sink = CaptureSink::default();
    let out = service
        .handle_stream(
            crate::domain::IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:test:stream-tool-error".to_string(),
                user_id: "u1".to_string(),
                text: "请调用一个不存在的工具后继续回答".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("handle stream should continue after tool error");

    assert_eq!(out.text, "我捕获到了工具调用失败，并继续给出最终回答。");
    assert_eq!(sink.text, "我捕获到了工具调用失败，并继续给出最终回答。");
}

#[tokio::test]
async fn handle_stream_parses_tool_call_even_when_provider_emits_no_deltas() {
    let provider = Arc::new(SequenceNoDeltaStreamProvider {
        replies: Arc::new(Mutex::new(vec![
            format!(
                r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"{BUILTIN_MCP_TOOL_CURRENT_TIME}","arguments":{{"timezone":"Asia/Shanghai"}}}}"#
            ),
            "无delta流式也能完成工具链路。".to_string(),
        ])),
    });
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
    let service = MessageService::new_with_backend(
        provider,
        memory,
        Some(Arc::new(RwLock::new(McpRuntime::default()))),
        AgentMcpSettings::default(),
        8,
        0,
        0,
    );

    let mut sink = CaptureSink::default();
    let out = service
        .handle_stream(
            crate::domain::IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:test:stream-no-delta".to_string(),
                user_id: "u1".to_string(),
                text: "即使没有delta也要完成工具调用".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("handle stream should parse tool call from fallback reply text");

    assert_eq!(out.text, "无delta流式也能完成工具链路。");
    assert_eq!(sink.text, "无delta流式也能完成工具链路。");
}

#[tokio::test]
async fn handle_uses_builtin_time_fast_path_without_provider_call() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
    let service = MessageService::new_with_backend(
        Arc::new(NeverProvider),
        memory,
        Some(Arc::new(RwLock::new(McpRuntime::default()))),
        AgentMcpSettings::default(),
        8,
        0,
        0,
    );

    let out = service
        .handle(crate::domain::IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "tg:test:fast-time".to_string(),
            user_id: "u1".to_string(),
            text: "现在美国时间是几点？".to_string(),
            reply_target: None,
        })
        .await
        .expect("fast time query should bypass provider");

    assert!(out.text.contains("美国东部时间"));
    assert!(out.text.contains("America/New_York"));
}

#[tokio::test]
async fn handle_stream_uses_builtin_time_fast_path_without_provider_call() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
    let service = MessageService::new_with_backend(
        Arc::new(NeverProvider),
        memory,
        Some(Arc::new(RwLock::new(McpRuntime::default()))),
        AgentMcpSettings::default(),
        8,
        0,
        0,
    );

    let mut sink = CaptureSink::default();
    let out = service
        .handle_stream(
            crate::domain::IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:test:fast-time-stream".to_string(),
                user_id: "u1".to_string(),
                text: "今天星期几？".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("stream time query should bypass provider");

    assert_eq!(sink.text, out.text);
    assert!(out.text.contains("星期"));
}
