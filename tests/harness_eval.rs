use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::RwLock;
use xiaomaolv::config::{AgentHarnessConfig, ToolVerificationMode};
use xiaomaolv::domain::{IncomingMessage, MessageRole, StoredMessage};
use xiaomaolv::harness::trajectory::{TrajectoryExitReason, TrajectoryFilter, TrajectoryRecord};
use xiaomaolv::mcp::{BUILTIN_MCP_SERVER_NAME, BUILTIN_MCP_TOOL_CURRENT_TIME, McpRuntime};
use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::{AgentMcpSettings, AgentSwarmSettings, MessageService};

#[derive(Default)]
struct EvalProvider {
    replies: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<Vec<StoredMessage>>>,
}

impl EvalProvider {
    fn new(replies: impl IntoIterator<Item = String>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Vec<StoredMessage>> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

#[async_trait]
impl ChatProvider for EvalProvider {
    fn model_name(&self) -> Option<&str> {
        Some("harness-eval-model")
    }

    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let is_summary = req
            .messages
            .first()
            .is_some_and(|msg| msg.content.starts_with("Summarize the following"));
        self.requests
            .lock()
            .expect("requests mutex")
            .push(req.messages);
        if is_summary {
            return Ok("eval compacted summary".to_string());
        }
        Ok(self
            .replies
            .lock()
            .expect("replies mutex")
            .pop_front()
            .unwrap_or_else(|| "eval fallback final".to_string()))
    }
}

struct EvalFixture {
    service: MessageService,
    store: SqliteMemoryStore,
    provider: Arc<EvalProvider>,
}

async fn fixture(
    replies: Vec<String>,
    harness: AgentHarnessConfig,
    mcp: AgentMcpSettings,
    max_recent_turns: usize,
) -> anyhow::Result<EvalFixture> {
    let provider = Arc::new(EvalProvider::new(replies));
    let store = SqliteMemoryStore::new("sqlite::memory:").await?;
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store.clone()));
    let runtime = Arc::new(RwLock::new(McpRuntime::new(HashMap::new())));
    let service = MessageService::new_with_backend(
        provider.clone(),
        backend,
        Some(runtime),
        mcp,
        max_recent_turns,
        0,
        0,
    )
    .with_agent_swarm(AgentSwarmSettings {
        enabled: false,
        ..Default::default()
    })
    .with_harness_config(&harness);
    Ok(EvalFixture {
        service,
        store,
        provider,
    })
}

fn default_mcp(max_iterations: usize) -> AgentMcpSettings {
    AgentMcpSettings {
        enabled: true,
        max_iterations,
        max_tool_result_chars: 4000,
    }
}

fn incoming(session_id: &str) -> IncomingMessage {
    IncomingMessage {
        channel: "eval".to_string(),
        session_id: session_id.to_string(),
        user_id: "user-eval".to_string(),
        text: "run the harness eval".to_string(),
        reply_target: None,
    }
}

fn time_tool_call() -> String {
    serde_json::json!({
        "server": BUILTIN_MCP_SERVER_NAME,
        "tool": BUILTIN_MCP_TOOL_CURRENT_TIME,
        "arguments": {}
    })
    .to_string()
}

fn unknown_tool_call() -> &'static str {
    r#"{"server":"missing","tool":"not_available","arguments":{}}"#
}

async fn single_trajectory(
    service: &MessageService,
    session_id: &str,
) -> anyhow::Result<TrajectoryRecord> {
    let trajectories = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some(session_id.to_string()),
            channel: Some("eval".to_string()),
            user_id: Some("user-eval".to_string()),
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await?;
    assert_eq!(trajectories.len(), 1, "expected one trajectory");
    Ok(trajectories.into_iter().next().expect("trajectory"))
}

async fn seed_history(
    store: &SqliteMemoryStore,
    session_id: &str,
    count: usize,
) -> anyhow::Result<()> {
    for idx in 0..count {
        store
            .append(
                session_id,
                StoredMessage {
                    role: if idx % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    },
                    content: format!("history {idx} {}", "long context ".repeat(16)),
                },
            )
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn eval_mcp_tool_loop_scenarios_are_deterministic() {
    let cases = vec![
        (
            "eval-no-tool",
            vec!["no tool final".to_string()],
            3,
            "no tool final",
            TrajectoryExitReason::FinalAnswer,
            0,
            None,
        ),
        (
            "eval-valid-tool",
            vec![time_tool_call(), "valid tool final".to_string()],
            3,
            "valid tool final",
            TrajectoryExitReason::FinalAnswer,
            1,
            None,
        ),
        (
            "eval-malformed-tool",
            vec![
                r#"{"server":"xiaomaolv_builtin","tool":"get_current_time","arguments":"#
                    .to_string(),
                "malformed recovery final".to_string(),
            ],
            3,
            "malformed recovery final",
            TrajectoryExitReason::FinalAnswer,
            0,
            None,
        ),
        (
            "eval-tool-error",
            vec![
                unknown_tool_call().to_string(),
                unknown_tool_call().to_string(),
                "tool error final".to_string(),
            ],
            3,
            "tool error final",
            TrajectoryExitReason::ToolError,
            2,
            Some("UNKNOWN_TOOL"),
        ),
        (
            "eval-max-iterations",
            vec![time_tool_call()],
            1,
            "",
            TrajectoryExitReason::MaxIterations,
            1,
            None,
        ),
    ];

    for (session, replies, max_iterations, expected_final, expected_exit, tool_calls, issue) in
        cases
    {
        let fx = fixture(
            replies,
            AgentHarnessConfig {
                enable_trajectory: true,
                ..Default::default()
            },
            default_mcp(max_iterations),
            20,
        )
        .await
        .expect("fixture");

        let out = fx.service.handle(incoming(session)).await.expect("handle");
        if expected_final.is_empty() {
            assert!(
                !out.text.trim().is_empty(),
                "{session} should produce fallback text"
            );
        } else {
            assert_eq!(out.text, expected_final, "{session} final answer");
        }

        let trajectory = single_trajectory(&fx.service, session)
            .await
            .expect("trajectory");
        assert_eq!(trajectory.exit_reason, expected_exit, "{session} exit");
        assert_eq!(trajectory.tool_calls.len(), tool_calls, "{session} calls");
        if let Some(issue) = issue {
            assert!(
                trajectory
                    .tool_calls
                    .iter()
                    .any(|call| call.result.to_string().contains(issue)),
                "{session} should expose {issue}"
            );
        }
        if session == "eval-malformed-tool" {
            let request_text = fx
                .provider
                .requests()
                .last()
                .expect("retry request")
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(request_text.contains("MALFORMED_TOOL_CALL_JSON"));
        }
    }
}

#[tokio::test]
async fn eval_compaction_scenarios_are_deterministic() {
    let cases = vec![
        (
            "eval-compact-none",
            AgentHarnessConfig {
                enable_trajectory: true,
                enable_compaction: true,
                compaction_message_threshold: 100,
                ..Default::default()
            },
            4,
            "compact none final",
            false,
        ),
        (
            "eval-compact-head-tail",
            AgentHarnessConfig {
                enable_trajectory: true,
                enable_compaction: true,
                compaction_strategy: "head_tail".to_string(),
                compaction_head_count: 1,
                compaction_tail_count: 4,
                compaction_message_threshold: 4,
                ..Default::default()
            },
            10,
            "compact head-tail final",
            true,
        ),
        (
            "eval-compact-budget",
            AgentHarnessConfig {
                enable_trajectory: true,
                enable_compaction: true,
                compaction_strategy: "budget_based".to_string(),
                compaction_budget_max_tokens: 200,
                compaction_message_threshold: 4,
                ..Default::default()
            },
            12,
            "compact budget final",
            true,
        ),
    ];

    for (session, harness, seed_count, final_answer, should_compact) in cases {
        let fx = fixture(vec![final_answer.to_string()], harness, default_mcp(3), 24)
            .await
            .expect("fixture");
        seed_history(&fx.store, session, seed_count)
            .await
            .expect("seed");

        let out = fx.service.handle(incoming(session)).await.expect("handle");
        assert_eq!(out.text, final_answer);
        let trajectory = single_trajectory(&fx.service, session)
            .await
            .expect("trajectory");
        assert!(matches!(
            trajectory.exit_reason,
            TrajectoryExitReason::FinalAnswer
        ));
        assert_eq!(trajectory.tool_calls.len(), 0);

        let requests = fx.provider.requests();
        let final_request = requests.last().expect("final request");
        let final_prompt = final_request
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            final_prompt.contains("eval compacted summary"),
            should_compact,
            "{session} compaction visibility"
        );
    }
}

#[tokio::test]
async fn eval_verification_modes_are_deterministic() {
    let cases = vec![
        (
            "eval-verify-observe",
            ToolVerificationMode::Observe,
            "observe verification final",
            TrajectoryExitReason::FinalAnswer,
            false,
        ),
        (
            "eval-verify-retry",
            ToolVerificationMode::Retry,
            "retry verification final",
            TrajectoryExitReason::FinalAnswer,
            true,
        ),
        (
            "eval-verify-block",
            ToolVerificationMode::Block,
            "block verification final",
            TrajectoryExitReason::ToolError,
            true,
        ),
    ];

    for (session, mode, final_answer, expected_exit, retry_prompt_expected) in cases {
        let mut mcp = default_mcp(3);
        mcp.max_tool_result_chars = 1;
        let fx = fixture(
            vec![time_tool_call(), final_answer.to_string()],
            AgentHarnessConfig {
                enable_trajectory: true,
                enable_verification: true,
                verification_mode: mode,
                ..Default::default()
            },
            mcp,
            20,
        )
        .await
        .expect("fixture");

        let out = fx.service.handle(incoming(session)).await.expect("handle");
        assert_eq!(out.text, final_answer);
        let trajectory = single_trajectory(&fx.service, session)
            .await
            .expect("trajectory");
        assert_eq!(trajectory.exit_reason, expected_exit);
        assert_eq!(trajectory.tool_calls.len(), 1);
        assert!(
            trajectory.tool_calls[0]
                .result
                .to_string()
                .contains("TRUNCATED_RESULT")
        );

        let request_text = fx
            .provider
            .requests()
            .last()
            .expect("verification request")
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            request_text.contains("MCP_TOOL_VERIFICATION_FAILED_JSON"),
            retry_prompt_expected
        );
    }
}
