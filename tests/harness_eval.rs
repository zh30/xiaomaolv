use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::RwLock;
use xiaomaolv::config::{AgentHarnessConfig, OutputVerificationMode, ToolVerificationMode};
use xiaomaolv::domain::{IncomingMessage, MessageRole, StoredMessage};
use xiaomaolv::harness::store::SqliteHarnessStore;
use xiaomaolv::harness::trajectory::{TrajectoryExitReason, TrajectoryFilter, TrajectoryRecord};
use xiaomaolv::mcp::{BUILTIN_MCP_SERVER_NAME, BUILTIN_MCP_TOOL_CURRENT_TIME, McpRuntime};
use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::{
    AgentMcpSettings, AgentSkillsSettings, AgentSwarmSettings, MessageService,
};
use xiaomaolv::skills::{
    SkillActivationMode, SkillConfigPaths, SkillRegistry, SkillRuntime, SkillScope,
};

const CASE_AGENT_RUN_FINAL_ANSWER: &str = "agent_run_final_answer";
const CASE_TOOL_PROTOCOL_SCHEMA_RETRY: &str = "tool_protocol_schema_retry";
const CASE_OUTPUT_EXIT_BLOCK_HIDDEN_TOOL_ERROR: &str = "output_exit_block_hidden_tool_error";
const CASE_SKILL_SELECTION_VISIBLE: &str = "skill_selection_visible";

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

struct ErrorProvider;

#[async_trait]
impl ChatProvider for ErrorProvider {
    fn model_name(&self) -> Option<&str> {
        Some("harness-eval-error-model")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        anyhow::bail!("eval provider failure")
    }
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
    let harness_store = Arc::new(SqliteHarnessStore::new(store.clone()));
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
    .with_harness_store(harness_store)
    .with_harness_config(&harness);
    Ok(EvalFixture {
        service,
        store,
        provider,
    })
}

async fn fixture_with_skills(
    replies: Vec<String>,
    harness: AgentHarnessConfig,
    mcp: AgentMcpSettings,
    max_recent_turns: usize,
    skills_runtime: SkillRuntime,
) -> anyhow::Result<EvalFixture> {
    let provider = Arc::new(EvalProvider::new(replies));
    let store = SqliteMemoryStore::new("sqlite::memory:").await?;
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(store.clone()));
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
    .with_harness_store(harness_store)
    .with_harness_config(&harness)
    .with_agent_skills(
        Some(Arc::new(RwLock::new(skills_runtime))),
        AgentSkillsSettings {
            enabled: true,
            max_selected: 3,
            max_prompt_chars: 8000,
            match_min_score: 0.45,
            llm_rerank_enabled: false,
        },
    );
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

async fn assert_eval_case(
    fx: &EvalFixture,
    session_id: &str,
    expected_final: &str,
    expected_exit: TrajectoryExitReason,
    expected_tool_calls: usize,
    expected_issue_marker: Option<&str>,
) -> anyhow::Result<TrajectoryRecord> {
    let out = fx.service.handle(incoming(session_id)).await?;
    assert_eq!(out.text, expected_final, "{session_id} final answer");

    let trajectory = single_trajectory(&fx.service, session_id).await?;
    assert_eq!(trajectory.exit_reason, expected_exit, "{session_id} exit");
    assert_eq!(
        trajectory.tool_calls.len(),
        expected_tool_calls,
        "{session_id} tool calls"
    );
    if let Some(marker) = expected_issue_marker {
        assert!(
            trajectory
                .tool_calls
                .iter()
                .any(|call| call.result.to_string().contains(marker)),
            "{session_id} should expose {marker}"
        );
    }
    Ok(trajectory)
}

fn test_skill_paths(tmp: &tempfile::TempDir) -> SkillConfigPaths {
    SkillConfigPaths {
        user_config: tmp.path().join("user-skills.toml"),
        project_config: tmp.path().join("project-skills.toml"),
        user_dir: tmp.path().join("user-skills"),
        project_dir: tmp.path().join("project-skills"),
    }
}

fn create_local_skill(tmp: &tempfile::TempDir, name: &str, desc: &str) -> std::path::PathBuf {
    let dir = tmp.path().join(name);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {desc}\ntags: [assistant]\n---\n\nBe concise."),
    )
    .expect("write");
    dir
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
async fn eval_agent_run_final_answer_case_is_deterministic() {
    let fx = fixture(
        vec!["agent run final".to_string()],
        AgentHarnessConfig {
            enable_trajectory: true,
            ..Default::default()
        },
        default_mcp(3),
        20,
    )
    .await
    .expect("fixture");

    assert_eval_case(
        &fx,
        CASE_AGENT_RUN_FINAL_ANSWER,
        "agent run final",
        TrajectoryExitReason::FinalAnswer,
        0,
        None,
    )
    .await
    .expect("agent run final answer case");
}

#[tokio::test]
async fn eval_tool_protocol_schema_retry_case_is_deterministic() {
    let fx = fixture(
        vec![
            r#"{"server":"xiaomaolv_builtin","tool":"get_current_time","arguments":{"timezone":123}}"#
                .to_string(),
            "schema retry final".to_string(),
        ],
        AgentHarnessConfig {
            enable_trajectory: true,
            ..Default::default()
        },
        default_mcp(3),
        20,
    )
    .await
    .expect("fixture");

    assert_eval_case(
        &fx,
        CASE_TOOL_PROTOCOL_SCHEMA_RETRY,
        "schema retry final",
        TrajectoryExitReason::FinalAnswer,
        1,
        Some("ARGUMENT_TYPE_MISMATCH"),
    )
    .await
    .expect("tool protocol schema retry case");

    let request_text = fx
        .provider
        .requests()
        .last()
        .expect("retry request")
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(request_text.contains("MCP_TOOL_VERIFICATION_FAILED_JSON"));
    assert!(request_text.contains("ARGUMENT_TYPE_MISMATCH"));
}

#[tokio::test]
async fn eval_output_exit_block_hidden_tool_error_case_is_deterministic() {
    let fx = fixture(
        vec![
            unknown_tool_call().to_string(),
            unknown_tool_call().to_string(),
            "confident hidden tool final".to_string(),
            unknown_tool_call().to_string(),
        ],
        AgentHarnessConfig {
            enable_trajectory: true,
            output_verification_mode: OutputVerificationMode::ReviseOnce,
            output_verification_llm_enabled: true,
            ..Default::default()
        },
        default_mcp(3),
        20,
    )
    .await
    .expect("fixture");

    assert_eval_case(
        &fx,
        CASE_OUTPUT_EXIT_BLOCK_HIDDEN_TOOL_ERROR,
        "I could not produce a reliable final answer from the available tool results.",
        TrajectoryExitReason::ToolError,
        2,
        Some("UNKNOWN_TOOL"),
    )
    .await
    .expect("output exit hidden tool error case");

    let request_text = fx
        .provider
        .requests()
        .last()
        .expect("revision request")
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        request_text.contains("OUTPUT_VERIFICATION_FAILED_JSON"),
        "service should emit output verification feedback on the harness path"
    );
    assert!(request_text.contains("HIDDEN_TOOL_ERROR"));
}

#[tokio::test]
async fn eval_skill_selection_visible_case_is_deterministic() {
    let tmp = tempdir().expect("tmp");
    let registry = SkillRegistry::new(test_skill_paths(&tmp)).expect("registry");
    let skill_dir = create_local_skill(&tmp, "calendar-skill", "always visible skill selection");
    registry
        .install_local_skill(
            SkillScope::User,
            &skill_dir,
            Some("calendar-skill"),
            SkillActivationMode::Always,
        )
        .await
        .expect("install");
    let runtime = SkillRuntime::from_registry(&registry)
        .await
        .expect("runtime");
    let fx = fixture_with_skills(
        vec!["skill selection final".to_string()],
        AgentHarnessConfig {
            enable_trajectory: true,
            ..Default::default()
        },
        default_mcp(3),
        20,
        runtime,
    )
    .await
    .expect("fixture with skills");

    assert_eval_case(
        &fx,
        CASE_SKILL_SELECTION_VISIBLE,
        "skill selection final",
        TrajectoryExitReason::FinalAnswer,
        0,
        None,
    )
    .await
    .expect("skill selection visible case");

    let request_text = fx
        .provider
        .requests()
        .last()
        .expect("request")
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(request_text.contains("SKILLS_CONTEXT"));
    assert!(request_text.contains("calendar-skill"));
}

#[tokio::test]
async fn eval_existing_mcp_loop_scenarios_remain_deterministic() {
    let cases = vec![
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
        assert_eval_case(
            &fx,
            session,
            expected_final,
            expected_exit,
            tool_calls,
            issue,
        )
        .await
        .expect("existing case");

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

    let fx = fixture(
        vec![time_tool_call()],
        AgentHarnessConfig {
            enable_trajectory: true,
            ..Default::default()
        },
        default_mcp(1),
        20,
    )
    .await
    .expect("fixture");
    let out = fx
        .service
        .handle(incoming("eval-max-iterations"))
        .await
        .expect("handle");
    assert!(
        !out.text.trim().is_empty(),
        "eval-max-iterations should produce fallback text"
    );
    let trajectory = single_trajectory(&fx.service, "eval-max-iterations")
        .await
        .expect("trajectory");
    assert_eq!(trajectory.exit_reason, TrajectoryExitReason::MaxIterations);
    assert_eq!(trajectory.tool_calls.len(), 1);
}

#[tokio::test]
async fn eval_agent_run_internal_error_case_is_recorded() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(store.clone()));
    let runtime = Arc::new(RwLock::new(McpRuntime::new(HashMap::new())));
    let service = MessageService::new_with_backend(
        Arc::new(ErrorProvider),
        backend,
        Some(runtime),
        default_mcp(3),
        20,
        0,
        0,
    )
    .with_agent_swarm(AgentSwarmSettings {
        enabled: false,
        ..Default::default()
    })
    .with_harness_store(harness_store)
    .with_harness_config(&AgentHarnessConfig {
        enable_trajectory: true,
        ..Default::default()
    });

    let err = service
        .handle(incoming("eval-agent-run-internal-error"))
        .await
        .expect_err("internal error");
    assert!(!err.to_string().trim().is_empty());

    let trajectory = single_trajectory(&service, "eval-agent-run-internal-error")
        .await
        .expect("trajectory");
    assert_eq!(trajectory.exit_reason, TrajectoryExitReason::InternalError);
    assert_eq!(trajectory.tool_calls.len(), 0);
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
