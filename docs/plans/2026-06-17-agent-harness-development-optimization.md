# Agent Harness Development Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn xiaomaolv from a message pipeline with agent features into a real agent harness with a first-class run lifecycle, unified tool protocol, unified output exit, and separately testable harness state.

**Architecture:** Add deep harness modules around the existing runtime before moving behavior out of `MessageService`. Preserve current provider, MCP, memory, channel, and config behavior while introducing smaller interfaces that concentrate lifecycle, protocol, output, and state semantics. Each task keeps the system shippable and leaves old paths working until the replacement path is covered by tests.

**Tech Stack:** Rust, Tokio, async-trait, SQLx/SQLite, Axum, Prometheus, Tracing, existing MCP runtime, existing `MessageService` pipeline.

## Global Constraints

- Keep existing public HTTP and Telegram behavior unless a task explicitly changes it.
- Use TDD: write or update the focused test before changing production code.
- Use existing dependencies; add a crate only after proving the standard library or existing crate set cannot cover the need.
- Keep harness features opt-in by config until Task 7 changes only documentation and examples.
- Do not remove the current MCP JSON loop until ToolProtocol integration tests pass for non-streaming and streaming flows.
- Do not remove existing `MemoryBackend` methods in the same task that introduces `HarnessStore`; keep a compatibility path until service integration is complete.
- Run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` before marking the whole plan complete.

---

## Current Pain Points

- `MessageService` owns provider, memory, MCP runtime, skills, code mode, swarm, compaction, trajectory, metrics, and verification state in one broad module.
- MCP tool calling is stronger than the first MVP, but the protocol still lives as raw JSON parsing plus duplicated stream/non-stream loops.
- Fast path, swarm path, streaming path, Code Mode path, and MCP path do not share one output exit interface.
- Code Mode policy filters capabilities, but the execution interface does not clearly express isolation level, approvals, or durable artifacts.
- Skills runtime selects and injects truncated `SKILL.md` content; it does not expose a deeper selection/execution interface.
- Harness state lives behind the broad `MemoryBackend` interface, whose default methods can silently report success for unsupported harness operations.
- Persisted trajectory call indexes are assigned by SQLite instead of using the `ToolCallRecord.call_index` set by `TrajectoryRun`.

## Target File Structure

- Create `src/harness/store.rs`: harness persistence seam for trajectories, compaction summaries, and later run events.
- Create `src/harness/run.rs`: first-class run lifecycle module that starts once, records events, and finishes once.
- Create `src/harness/tool_protocol.rs`: parsing, validation, execution result envelope, and retry/block feedback for MCP tool calls.
- Create `src/harness/output_exit.rs`: final answer verification, optional revision/blocking, streaming replay, and persistence handoff.
- Create `src/harness/execution_environment.rs`: explicit Code Mode execution environment interface and local/subprocess adapters.
- Modify `src/harness/mod.rs`: export the new harness modules.
- Modify `src/service.rs`: shrink orchestration by delegating to the new harness modules.
- Modify `src/memory.rs`: preserve existing memory behavior while adding `HarnessStore` implementations.
- Modify `src/code_mode.rs`: route Code Mode execution through `ExecutionEnvironment`.
- Modify `src/skills.rs`: split skill selection from prompt rendering and emit selected-skill metadata.
- Modify `src/http.rs`: wire separate harness store/runtime handles and expose future run IDs consistently.
- Add tests under `tests/harness_store.rs`, `tests/harness_agent_run.rs`, `tests/harness_tool_protocol.rs`, `tests/harness_output_exit.rs`, `tests/code_mode_execution_environment.rs`, and update existing service harness tests.

## Priority Overview

| Priority | Task | Result |
|---|---|---|
| P0 | HarnessStore seam and trajectory index fix | Harness persistence becomes explicit and call order is deterministic |
| P0 | AgentRun lifecycle module | Start/log/finish semantics move out of `MessageService` |
| P1 | ToolProtocol module | MCP parse/validate/execute semantics live in one module |
| P1 | OutputExit module | All final answers cross the same verifier/persist/stream interface |
| P2 | ExecutionEnvironment for Code Mode | Code Mode states its isolation guarantees honestly at the interface |
| P2 | Skills selection interface | Skills become structured run inputs instead of only prompt text |
| P3 | Eval and documentation alignment | The new harness shape is documented and regression-tested |

---

### Task 1: Add HarnessStore Seam And Preserve Trajectory Call Index

**Priority:** P0

**Problem:** Harness state is buried in `MemoryBackend`, and `insert_trajectory_tool_call` ignores `ToolCallRecord.call_index` by recomputing the index in SQL.

**Files:**
- Create: `src/harness/store.rs`
- Modify: `src/harness/mod.rs`
- Modify: `src/memory.rs`
- Test: `tests/harness_store.rs`

**Interfaces:**
- Consumes: `ToolCallRecord`, `TrajectoryRecord`, `TrajectoryExitReason`, `TrajectoryFilter`, compaction summary request/record types from `src/memory.rs`.
- Produces:
  - `pub trait HarnessStore: Send + Sync`
  - `pub struct SqliteHarnessStore`
  - `pub async fn insert_trajectory_tool_call(&self, trajectory_id: &str, record: ToolCallRecord) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing test for explicit call indexes**

Add `tests/harness_store.rs`:

```rust
use xiaomaolv::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryFilter,
};
use xiaomaolv::memory::SqliteMemoryStore;

#[tokio::test]
async fn trajectory_store_preserves_explicit_call_index() {
    let store = SqliteMemoryStore::new("sqlite::memory:").await.expect("store");
    store
        .start_trajectory("traj-index", "session-a", "http", "user-a", "model-a")
        .await
        .expect("start trajectory");

    let first = ToolCallRecord {
        call_index: 7,
        server: "server-a".to_string(),
        tool: "tool-a".to_string(),
        arguments: serde_json::json!({"q": "first"}),
        result: serde_json::json!({"ok": true}),
        ok: true,
        duration_ms: 12,
        iteration: 0,
    };
    let second = ToolCallRecord {
        call_index: 3,
        server: "server-a".to_string(),
        tool: "tool-a".to_string(),
        arguments: serde_json::json!({"q": "second"}),
        result: serde_json::json!({"ok": true}),
        ok: true,
        duration_ms: 15,
        iteration: 0,
    };

    store
        .insert_trajectory_tool_call("traj-index", first)
        .await
        .expect("insert first");
    store
        .insert_trajectory_tool_call("traj-index", second)
        .await
        .expect("insert second");
    store
        .finish_trajectory(
            "traj-index",
            Some("done".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish trajectory");

    let records = store
        .query_trajectories(TrajectoryFilter {
            session_id: Some("session-a".to_string()),
            channel: Some("http".to_string()),
            user_id: None,
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");

    let calls = &records[0].tool_calls;
    assert_eq!(calls.iter().map(|c| c.call_index).collect::<Vec<_>>(), vec![3, 7]);
    assert_eq!(calls[0].arguments["q"], "second");
    assert_eq!(calls[1].arguments["q"], "first");
}
```

- [ ] **Step 2: Run the focused test and confirm failure**

Run:

```bash
cargo test --test harness_store trajectory_store_preserves_explicit_call_index -- --nocapture
```

Expected: FAIL because the persisted call indexes are `0` and `1`, not `3` and `7`.

- [ ] **Step 3: Add the `HarnessStore` interface**

Create `src/harness/store.rs`:

```rust
use async_trait::async_trait;

use crate::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryFilter, TrajectoryRecord,
};
use crate::memory::{
    CompactionSummaryLoadRequest, CompactionSummaryRecord, CompactionSummaryUpsertRequest,
    SqliteMemoryStore,
};

#[async_trait]
pub trait HarnessStore: Send + Sync {
    async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()>;

    async fn insert_trajectory_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()>;

    async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()>;

    async fn get_trajectory(&self, trajectory_id: &str) -> anyhow::Result<Option<TrajectoryRecord>>;

    async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>>;

    async fn load_compaction_summary(
        &self,
        req: CompactionSummaryLoadRequest,
    ) -> anyhow::Result<Option<CompactionSummaryRecord>>;

    async fn upsert_compaction_summary(
        &self,
        req: CompactionSummaryUpsertRequest,
    ) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct SqliteHarnessStore {
    store: SqliteMemoryStore,
}

impl SqliteHarnessStore {
    pub fn new(store: SqliteMemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl HarnessStore for SqliteHarnessStore {
    async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        self.store
            .start_trajectory(trajectory_id, session_id, channel, user_id, model)
            .await
    }

    async fn insert_trajectory_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()> {
        self.store
            .insert_trajectory_tool_call(trajectory_id, record)
            .await
    }

    async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()> {
        self.store
            .finish_trajectory(trajectory_id, final_answer, exit_reason)
            .await
    }

    async fn get_trajectory(&self, trajectory_id: &str) -> anyhow::Result<Option<TrajectoryRecord>> {
        self.store.get_trajectory(trajectory_id).await
    }

    async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>> {
        self.store.query_trajectories(filter).await
    }

    async fn load_compaction_summary(
        &self,
        req: CompactionSummaryLoadRequest,
    ) -> anyhow::Result<Option<CompactionSummaryRecord>> {
        self.store.load_compaction_summary(req).await
    }

    async fn upsert_compaction_summary(
        &self,
        req: CompactionSummaryUpsertRequest,
    ) -> anyhow::Result<()> {
        self.store.upsert_compaction_summary(req).await
    }
}
```

Modify `src/harness/mod.rs`:

```rust
pub mod compactor;
pub mod observability;
pub mod store;
pub mod trajectory;
pub mod verifier;
```

- [ ] **Step 4: Preserve explicit `call_index` in SQLite**

Change `SqliteMemoryStore::insert_trajectory_tool_call` in `src/memory.rs` so the SQL inserts `record.call_index` directly:

```rust
sqlx::query(
    "INSERT INTO mcp_trajectory_tool_calls
     (trajectory_id, call_index, iteration, server, tool, arguments, result, ok, duration_ms)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
)
.bind(trajectory_id)
.bind(record.call_index as i64)
.bind(record.iteration as i64)
.bind(&record.server)
.bind(&record.tool)
.bind(serde_json::to_string(&record.arguments).unwrap_or_default())
.bind(serde_json::to_string(&record.result).unwrap_or_default())
.bind(record.ok)
.bind(record.duration_ms as i64)
.execute(&self.pool)
.await
.context("failed to insert trajectory tool call")?;
```

- [ ] **Step 5: Run the focused test**

Run:

```bash
cargo test --test harness_store trajectory_store_preserves_explicit_call_index -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Run related trajectory tests**

Run:

```bash
cargo test --test harness_trajectory -- --nocapture
cargo test --test service_harness_trajectory -- --nocapture
```

Expected: both PASS.

- [ ] **Step 7: Commit**

```bash
git add src/harness/mod.rs src/harness/store.rs src/memory.rs tests/harness_store.rs
git commit -m "refactor: add harness store seam"
```

**Acceptance Criteria:**
- Persisted trajectory calls use the `call_index` produced by `TrajectoryRun`.
- `HarnessStore` exists without removing the current `MemoryBackend` compatibility path.
- All existing trajectory query behavior still works.

---

### Task 2: Introduce AgentRun Lifecycle Module

**Priority:** P0

**Problem:** The harness lifecycle is spread across `MessageService`, `TrajectoryRun`, verifier calls, metrics, and manual error handling.

**Files:**
- Create: `src/harness/run.rs`
- Modify: `src/harness/mod.rs`
- Modify: `src/harness/trajectory.rs`
- Test: `tests/harness_agent_run.rs`

**Interfaces:**
- Consumes: `TrajectoryLogger`, `TrajectoryMetrics`, `ToolCallRecord`, `TrajectoryExitReason`.
- Produces:
  - `pub struct AgentRun`
  - `pub struct AgentRunStart`
  - `pub enum AgentRunExit`
  - `pub async fn finish(&mut self, exit: AgentRunExit)`

- [ ] **Step 1: Write the failing lifecycle test**

Add `tests/harness_agent_run.rs`:

```rust
use prometheus::Registry;
use xiaomaolv::harness::observability::TrajectoryMetrics;
use xiaomaolv::harness::run::{AgentRun, AgentRunExit, AgentRunStart};
use xiaomaolv::harness::trajectory::ToolCallRecord;
use xiaomaolv::memory::{SqliteMemoryBackend, SqliteMemoryStore};

#[tokio::test]
async fn agent_run_finishes_once_and_records_tool_call() {
    let store = SqliteMemoryStore::new("sqlite::memory:").await.expect("store");
    let backend = std::sync::Arc::new(SqliteMemoryBackend::new(store.clone()));
    let logger = xiaomaolv::harness::trajectory::TrajectoryLogger::new(backend, true);
    let metrics = TrajectoryMetrics::new(&Registry::new());

    let mut run = AgentRun::start(AgentRunStart {
        logger: Some(logger),
        metrics: Some(metrics),
        session_id: "session-run".to_string(),
        channel: "http".to_string(),
        user_id: "user-run".to_string(),
        model: "model-run".to_string(),
    })
    .await;

    run.record_tool_call(ToolCallRecord {
        call_index: 0,
        server: "s".to_string(),
        tool: "t".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"ok": true}),
        ok: true,
        duration_ms: 8,
        iteration: 0,
    })
    .await;

    run.finish(AgentRunExit::FinalAnswer("first".to_string())).await;
    run.finish(AgentRunExit::InternalError).await;

    let record = store
        .get_trajectory(run.id())
        .await
        .expect("get trajectory")
        .expect("trajectory exists");

    assert_eq!(record.final_answer.as_deref(), Some("first"));
    assert_eq!(record.tool_calls.len(), 1);
    assert!(record.finished_at.is_some());
}
```

- [ ] **Step 2: Run the focused test and confirm failure**

Run:

```bash
cargo test --test harness_agent_run agent_run_finishes_once_and_records_tool_call -- --nocapture
```

Expected: FAIL because `src/harness/run.rs` does not exist.

- [ ] **Step 3: Add `AgentRun`**

Create `src/harness/run.rs`:

```rust
use crate::harness::observability::TrajectoryMetrics;
use crate::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryLogger, TrajectoryRun,
};

pub struct AgentRunStart {
    pub logger: Option<TrajectoryLogger>,
    pub metrics: Option<TrajectoryMetrics>,
    pub session_id: String,
    pub channel: String,
    pub user_id: String,
    pub model: String,
}

pub enum AgentRunExit {
    FinalAnswer(String),
    MaxIterations(String),
    ToolError(String),
    Timeout,
    InternalError,
}

pub struct AgentRun {
    trajectory: TrajectoryRun,
    finished: bool,
}

impl AgentRun {
    pub async fn start(start: AgentRunStart) -> Self {
        let trajectory = TrajectoryRun::start(
            start.logger,
            start.metrics,
            &start.session_id,
            &start.channel,
            &start.user_id,
            &start.model,
        )
        .await;
        Self {
            trajectory,
            finished: false,
        }
    }

    pub fn id(&self) -> &str {
        self.trajectory.id()
    }

    pub fn observe_iteration(&mut self, iteration: usize) {
        self.trajectory.observe_iteration(iteration);
    }

    pub async fn record_tool_call(&mut self, record: ToolCallRecord) -> ToolCallRecord {
        self.trajectory.log_tool_call(record).await
    }

    pub async fn finish(&mut self, exit: AgentRunExit) {
        if self.finished {
            return;
        }
        self.finished = true;
        let (answer, reason) = match exit {
            AgentRunExit::FinalAnswer(answer) => (Some(answer), TrajectoryExitReason::FinalAnswer),
            AgentRunExit::MaxIterations(answer) => {
                (Some(answer), TrajectoryExitReason::MaxIterations)
            }
            AgentRunExit::ToolError(answer) => (Some(answer), TrajectoryExitReason::ToolError),
            AgentRunExit::Timeout => (None, TrajectoryExitReason::Timeout),
            AgentRunExit::InternalError => (None, TrajectoryExitReason::InternalError),
        };
        self.trajectory.finish(answer, reason).await;
    }
}
```

Modify `src/harness/mod.rs`:

```rust
pub mod compactor;
pub mod observability;
pub mod run;
pub mod store;
pub mod trajectory;
pub mod verifier;
```

- [ ] **Step 4: Run the focused test**

Run:

```bash
cargo test --test harness_agent_run agent_run_finishes_once_and_records_tool_call -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/harness/mod.rs src/harness/run.rs tests/harness_agent_run.rs
git commit -m "feat: add agent run lifecycle"
```

**Acceptance Criteria:**
- `AgentRun` has one lifecycle interface for start, iteration, tool event, and finish.
- Finishing twice keeps the first terminal state.
- No `MessageService` behavior changes yet.

---

### Task 3: Extract MCP ToolProtocol

**Priority:** P1

**Problem:** Tool call parsing, validation, execution result envelopes, retry feedback, and verification records live inside `src/service.rs` and are duplicated across stream/non-stream paths.

**Files:**
- Create: `src/harness/tool_protocol.rs`
- Modify: `src/harness/mod.rs`
- Modify: `src/service.rs`
- Test: `tests/harness_tool_protocol.rs`
- Test: update `tests/service_mcp_loop.rs`

**Interfaces:**
- Consumes: `McpRuntime`, `McpToolInfo`, `ToolCallRecord`, `ToolCallVerifier`, `ToolVerificationMode`.
- Produces:
  - `pub struct ToolProtocol`
  - `pub enum ToolProposal`
  - `pub struct ToolExecutionEnvelope`
  - `pub fn parse_reply(&self, reply: &str) -> ToolProposal`
  - `pub async fn execute_validated(&self, runtime: &McpRuntime, call: ParsedToolCall, iteration: usize) -> anyhow::Result<ToolExecutionEnvelope>`

- [ ] **Step 1: Write parsing and validation tests**

Add `tests/harness_tool_protocol.rs`:

```rust
use xiaomaolv::harness::tool_protocol::{ParsedToolCall, ToolProposal, ToolProtocol};
use xiaomaolv::mcp::McpToolInfo;

fn demo_tools() -> Vec<McpToolInfo> {
    vec![McpToolInfo {
        server: "demo".to_string(),
        name: "search".to_string(),
        description: Some("search demo".to_string()),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"q": {"type": "string"}},
            "required": ["q"],
            "additionalProperties": false
        }),
        code_mode_capabilities: None,
    }]
}

#[test]
fn tool_protocol_parses_valid_tool_call() {
    let protocol = ToolProtocol::new(demo_tools(), 4000);
    let proposal = protocol.parse_reply(r#"{"server":"demo","tool":"search","arguments":{"q":"rust"}}"#);
    match proposal {
        ToolProposal::Tool(call) => {
            assert_eq!(call.server, "demo");
            assert_eq!(call.tool, "search");
            assert_eq!(call.arguments["q"], "rust");
        }
        other => panic!("expected tool proposal, got {other:?}"),
    }
}

#[test]
fn tool_protocol_rejects_schema_invalid_arguments() {
    let protocol = ToolProtocol::new(demo_tools(), 4000);
    let call = ParsedToolCall {
        server: "demo".to_string(),
        tool: "search".to_string(),
        arguments: serde_json::json!({"unknown": true}),
    };
    let result = protocol.validate_call(&call);
    assert!(result.is_err());
    let verification = result.err().expect("verification");
    assert!(verification.issues.iter().any(|issue| issue.code == "MISSING_REQUIRED_ARGUMENT"));
    assert!(verification.issues.iter().any(|issue| issue.code == "UNKNOWN_ARGUMENT"));
}
```

- [ ] **Step 2: Run the focused tests and confirm failure**

Run:

```bash
cargo test --test harness_tool_protocol -- --nocapture
```

Expected: FAIL because `tool_protocol` does not exist.

- [ ] **Step 3: Move parser and validation logic into `tool_protocol.rs`**

Create `src/harness/tool_protocol.rs` with public equivalents of the current service-local parsing and validation types. Keep the wire shape unchanged:

```rust
use std::time::Instant;

use serde_json::Value;

use crate::harness::trajectory::ToolCallRecord;
use crate::harness::verifier::{
    IssueSeverity, ToolSchemaVerifier, VerificationIssue, VerificationResult,
};
use crate::mcp::{McpRuntime, McpToolInfo};

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCall {
    pub server: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolProposal {
    Tool(ParsedToolCall),
    FinalAnswer,
    ParseError(VerificationResult),
}

#[derive(Debug, Clone)]
pub struct ToolExecutionEnvelope {
    pub message_json: Value,
    pub record: ToolCallRecord,
}

#[derive(Clone)]
pub struct ToolProtocol {
    tools: Vec<McpToolInfo>,
    max_result_chars: usize,
}

impl ToolProtocol {
    pub fn new(tools: Vec<McpToolInfo>, max_result_chars: usize) -> Self {
        Self {
            tools,
            max_result_chars,
        }
    }

    pub fn parse_reply(&self, reply: &str) -> ToolProposal {
        parse_tool_call_attempt(reply)
    }

    pub fn validate_call(&self, call: &ParsedToolCall) -> Result<&McpToolInfo, VerificationResult> {
        let tool_info = self
            .tools
            .iter()
            .find(|tool| tool.server == call.server && tool.name == call.tool)
            .ok_or_else(|| verification_failure_result(
                "UNKNOWN_TOOL",
                format!("Requested MCP tool is not available: {}::{}", call.server, call.tool),
            ))?;
        let verification = ToolSchemaVerifier::new().verify_arguments(tool_info, &call.arguments);
        if verification.passed {
            Ok(tool_info)
        } else {
            Err(verification)
        }
    }

    pub async fn execute_validated(
        &self,
        runtime: &McpRuntime,
        call: ParsedToolCall,
        iteration: usize,
    ) -> ToolExecutionEnvelope {
        let started = Instant::now();
        let result = runtime
            .call_tool(&call.server, &call.tool, call.arguments.clone())
            .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(value) => {
                let result = truncate_json_value(&value, self.max_result_chars);
                let record = ToolCallRecord {
                    call_index: 0,
                    server: call.server.clone(),
                    tool: call.tool.clone(),
                    arguments: call.arguments,
                    result: result.clone(),
                    ok: true,
                    duration_ms,
                    iteration,
                };
                ToolExecutionEnvelope {
                    message_json: serde_json::json!({
                        "server": call.server,
                        "tool": call.tool,
                        "ok": true,
                        "result": result
                    }),
                    record,
                }
            }
            Err(err) => {
                let error_json = serde_json::json!({"error": err.to_string()});
                let record = ToolCallRecord {
                    call_index: 0,
                    server: call.server.clone(),
                    tool: call.tool.clone(),
                    arguments: call.arguments,
                    result: error_json.clone(),
                    ok: false,
                    duration_ms,
                    iteration,
                };
                ToolExecutionEnvelope {
                    message_json: serde_json::json!({
                        "server": call.server,
                        "tool": call.tool,
                        "ok": false,
                        "error": err.to_string()
                    }),
                    record,
                }
            }
        }
    }
}
```

Move the existing private helpers from `service.rs` into this module and make only these items public: `ParsedToolCall`, `ToolProposal`, `ToolProtocol`, `ToolExecutionEnvelope`, `verification_feedback_message`, and `annotate_record_with_verification_failure`.

- [ ] **Step 4: Run protocol tests**

Run:

```bash
cargo test --test harness_tool_protocol -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Replace non-stream MCP loop internals**

Modify `complete_with_mcp_loop` in `src/service.rs`:

- Build `let protocol = ToolProtocol::new(tools.clone(), self.agent_mcp.max_tool_result_chars);`
- Replace `parse_mcp_tool_call_attempt(&reply)` with `protocol.parse_reply(&reply)`.
- Replace `validate_mcp_tool_call(&tools, &tool_call)` with `protocol.validate_call(&tool_call)`.
- Replace direct `runtime.call_tool(...)` blocks with `protocol.execute_validated(&runtime, tool_call, iteration).await`.

The non-stream loop must still push this system message after tool execution:

```rust
history.push(StoredMessage {
    role: MessageRole::System,
    content: format!(
        "MCP_TOOL_RESULT_JSON:\n{}",
        serde_json::to_string(&tool_message).unwrap_or_else(|_| "{\"ok\":false}".to_string())
    ),
});
```

- [ ] **Step 6: Replace stream MCP loop internals**

Make the same replacement in `complete_with_mcp_loop_stream`. Keep `BufferedStreamSink` behavior unchanged: intermediate tool-call replies remain buffered, and only final answers are replayed to the channel sink.

- [ ] **Step 7: Run MCP service tests**

Run:

```bash
cargo test --test service_mcp_loop -- --nocapture
cargo test --test service_streaming -- --nocapture
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/harness/mod.rs src/harness/tool_protocol.rs src/service.rs tests/harness_tool_protocol.rs tests/service_mcp_loop.rs tests/service_streaming.rs
git commit -m "refactor: extract mcp tool protocol"
```

**Acceptance Criteria:**
- Parse, validation, execution envelope, and feedback message behavior is tested outside `MessageService`.
- Non-streaming and streaming MCP loops continue to pass current service tests.
- Tool result JSON shape remains unchanged.

---

### Task 4: Add Unified OutputExit

**Priority:** P1

**Problem:** Final answer verification is not a single interface. Fast time answers, swarm streaming answers, plain provider answers, Code Mode answers, and MCP answers take different paths.

**Files:**
- Create: `src/harness/output_exit.rs`
- Modify: `src/harness/mod.rs`
- Modify: `src/service.rs`
- Test: `tests/harness_output_exit.rs`
- Test: update `tests/service_pipeline.rs`
- Test: update `tests/service_streaming.rs`

**Interfaces:**
- Consumes: `DeterministicOutputVerifier`, `OutputVerificationMode`, `ChatProvider`, `StreamSink`, `MemoryBackend`.
- Produces:
  - `pub struct OutputExit`
  - `pub struct OutputExitRequest`
  - `pub struct OutputExitResult`
  - `pub async fn finalize(&self, req: OutputExitRequest<'_>) -> anyhow::Result<OutputExitResult>`

- [ ] **Step 1: Write OutputExit tests**

Add `tests/harness_output_exit.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use xiaomaolv::config::OutputVerificationMode;
use xiaomaolv::domain::{MessageRole, StoredMessage};
use xiaomaolv::harness::output_exit::{OutputExit, OutputExitRequest};
use xiaomaolv::harness::trajectory::ToolCallRecord;
use xiaomaolv::provider::{ChatProvider, CompletionRequest, StreamSink};

#[derive(Clone)]
struct EchoProvider;

#[async_trait]
impl ChatProvider for EchoProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        Ok(req
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default())
    }

    async fn complete_stream(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        let text = self.complete(req).await?;
        sink.on_delta(&text).await?;
        Ok(text)
    }
}

#[tokio::test]
async fn output_exit_blocks_hidden_tool_error() {
    let exit = OutputExit::new(
        Arc::new(EchoProvider),
        Some(xiaomaolv::harness::verifier::DeterministicOutputVerifier::new()),
        OutputVerificationMode::Block,
        false,
        6000,
        2000,
    );

    let tool_calls = vec![ToolCallRecord {
        call_index: 0,
        server: "demo".to_string(),
        tool: "search".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"error": "network failed"}),
        ok: false,
        duration_ms: 1,
        iteration: 0,
    }];

    let result = exit
        .finalize(OutputExitRequest {
            history: &[StoredMessage {
                role: MessageRole::User,
                content: "answer with search result".to_string(),
            }],
            channel: "http",
            final_answer: "The answer is 42.".to_string(),
            tool_calls: &tool_calls,
            required_format: None,
        })
        .await
        .expect("finalize");

    assert_ne!(result.text, "The answer is 42.");
    assert!(result.verified);
    assert!(result.blocked_or_revised);
}
```

- [ ] **Step 2: Run the focused test and confirm failure**

Run:

```bash
cargo test --test harness_output_exit output_exit_blocks_hidden_tool_error -- --nocapture
```

Expected: FAIL because `OutputExit` does not exist.

- [ ] **Step 3: Implement OutputExit using existing verification logic**

Create `src/harness/output_exit.rs`:

```rust
use std::sync::Arc;

use anyhow::Context;

use crate::config::OutputVerificationMode;
use crate::domain::{MessageRole, StoredMessage};
use crate::harness::trajectory::ToolCallRecord;
use crate::harness::verifier::{
    DeterministicOutputVerifier, OutputVerificationRequest, VerificationIssue,
};
use crate::provider::{ChatProvider, CompletionRequest};

pub struct OutputExit {
    provider: Arc<dyn ChatProvider>,
    verifier: Option<DeterministicOutputVerifier>,
    mode: OutputVerificationMode,
    llm_enabled: bool,
    max_prompt_chars: usize,
    max_result_chars: usize,
}

pub struct OutputExitRequest<'a> {
    pub history: &'a [StoredMessage],
    pub channel: &'a str,
    pub final_answer: String,
    pub tool_calls: &'a [ToolCallRecord],
    pub required_format: Option<String>,
}

pub struct OutputExitResult {
    pub text: String,
    pub verified: bool,
    pub blocked_or_revised: bool,
    pub issue_codes: Vec<String>,
}

impl OutputExit {
    pub fn new(
        provider: Arc<dyn ChatProvider>,
        verifier: Option<DeterministicOutputVerifier>,
        mode: OutputVerificationMode,
        llm_enabled: bool,
        max_prompt_chars: usize,
        max_result_chars: usize,
    ) -> Self {
        Self {
            provider,
            verifier,
            mode,
            llm_enabled,
            max_prompt_chars,
            max_result_chars,
        }
    }

    pub async fn finalize(&self, req: OutputExitRequest<'_>) -> anyhow::Result<OutputExitResult> {
        let Some(verifier) = &self.verifier else {
            return Ok(OutputExitResult {
                text: req.final_answer,
                verified: false,
                blocked_or_revised: false,
                issue_codes: vec![],
            });
        };

        let verification = verifier.verify(&OutputVerificationRequest {
            final_answer: req.final_answer.clone(),
            recent_history: req.history.to_vec(),
            tool_calls: req.tool_calls.to_vec(),
            channel: req.channel.to_string(),
            required_format: req.required_format.clone(),
        });
        if verification.passed {
            return Ok(OutputExitResult {
                text: req.final_answer,
                verified: true,
                blocked_or_revised: false,
                issue_codes: vec![],
            });
        }

        let issue_codes = verification
            .issues
            .iter()
            .map(|issue| issue.code.clone())
            .collect::<Vec<_>>();

        match self.mode {
            OutputVerificationMode::Off | OutputVerificationMode::Observe => Ok(OutputExitResult {
                text: req.final_answer,
                verified: true,
                blocked_or_revised: false,
                issue_codes,
            }),
            OutputVerificationMode::Block => Ok(OutputExitResult {
                text: verification.suggested_revision.unwrap_or_else(|| {
                    "I could not produce a reliable final answer.".to_string()
                }),
                verified: true,
                blocked_or_revised: true,
                issue_codes,
            }),
            OutputVerificationMode::ReviseOnce => {
                if !self.llm_enabled {
                    return Ok(OutputExitResult {
                        text: verification.suggested_revision.unwrap_or_else(|| {
                            "I could not produce a reliable final answer.".to_string()
                        }),
                        verified: true,
                        blocked_or_revised: true,
                        issue_codes,
                    });
                }
                let revision_prompt = output_revision_prompt(&verification.issues, self.max_prompt_chars);
                let mut revision_history = req.history.to_vec();
                revision_history.push(StoredMessage {
                    role: MessageRole::Assistant,
                    content: truncate_text(&req.final_answer, self.max_result_chars),
                });
                revision_history.push(StoredMessage {
                    role: MessageRole::System,
                    content: revision_prompt,
                });
                let revised = self
                    .provider
                    .complete(CompletionRequest {
                        messages: revision_history,
                        ..Default::default()
                    })
                    .await
                    .context("provider completion failed during output exit revision")?;
                Ok(OutputExitResult {
                    text: truncate_text(revised.trim(), self.max_result_chars),
                    verified: true,
                    blocked_or_revised: true,
                    issue_codes,
                })
            }
        }
    }
}

fn output_revision_prompt(issues: &[VerificationIssue], max_chars: usize) -> String {
    let mut body = String::from("Revise the previous answer. Address these verification issues:\n");
    for issue in issues {
        body.push_str("- ");
        body.push_str(&issue.code);
        body.push_str(": ");
        body.push_str(&issue.message);
        body.push('\n');
    }
    truncate_text(&body, max_chars)
}

fn truncate_text(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out = input.chars().take(max_chars).collect::<String>();
    out.push_str("...(truncated)");
    out
}
```

Modify `src/harness/mod.rs`:

```rust
pub mod compactor;
pub mod observability;
pub mod output_exit;
pub mod run;
pub mod store;
pub mod tool_protocol;
pub mod trajectory;
pub mod verifier;
```

- [ ] **Step 4: Run OutputExit tests**

Run:

```bash
cargo test --test harness_output_exit -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Use OutputExit in `MessageService`**

Modify `MessageService::verify_final_answer` so it constructs and calls `OutputExit` instead of owning verification logic directly. Keep the `verify_final_answer` method as a private compatibility method for this task:

```rust
let exit = OutputExit::new(
    self.provider.clone(),
    self.output_verifier.clone(),
    self.output_verification_mode,
    self.output_verification_llm_enabled,
    self.output_verification_max_prompt_chars,
    self.output_verification_max_result_chars,
);
let result = exit
    .finalize(OutputExitRequest {
        history,
        channel,
        final_answer,
        tool_calls,
        required_format: None,
    })
    .await?;
Ok(result.text)
```

- [ ] **Step 6: Route stream swarm and fast path through the same private compatibility method**

In `handle_stream`, change the swarm branch to verify before streaming:

```rust
if let Some(swarm_text) = self.try_swarm_reply(&incoming, &history).await? {
    let swarm_text = self
        .verify_final_answer(&history, &incoming.channel, swarm_text, &[])
        .await?;
    if !swarm_text.is_empty() {
        sink.on_delta(&swarm_text).await?;
    }
    self.persist_assistant_reply(&incoming, &swarm_text).await?;
    return Ok(OutgoingMessage {
        channel: incoming.channel,
        session_id: incoming.session_id,
        text: swarm_text,
        reply_target: incoming.reply_target,
    });
}
```

Do not change the fast time path in this task; add it to Task 7 documentation because it is deterministic and intentionally bypasses the model.

- [ ] **Step 7: Run service output tests**

Run:

```bash
cargo test --test harness_output_verifier -- --nocapture
cargo test --test service_pipeline -- --nocapture
cargo test --test service_streaming -- --nocapture
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/harness/mod.rs src/harness/output_exit.rs src/service.rs tests/harness_output_exit.rs tests/service_pipeline.rs tests/service_streaming.rs
git commit -m "refactor: unify output verification exit"
```

**Acceptance Criteria:**
- Output verification logic is testable without constructing `MessageService`.
- Streaming swarm answers are verified before delivery.
- Existing MCP and Code Mode output behavior remains covered by existing tests.

---

### Task 5: Introduce ExecutionEnvironment For Code Mode

**Priority:** P2

**Problem:** Code Mode has policy and subprocess execution, but its interface does not clearly state whether an execution is local, subprocess-isolated, or a real sandbox.

**Files:**
- Create: `src/harness/execution_environment.rs`
- Modify: `src/harness/mod.rs`
- Modify: `src/code_mode.rs`
- Modify: `docs/code-mode-observability.md`
- Test: `tests/code_mode_execution_environment.rs`

**Interfaces:**
- Consumes: `CodeModePlan`, `CodeModeExecutionReport`, `McpRuntime`, `McpToolInfo`, `AgentCodeModeSettings`.
- Produces:
  - `pub enum ExecutionIsolation`
  - `pub trait ExecutionEnvironment`
  - `pub struct LocalExecutionEnvironment`
  - `pub struct SubprocessExecutionEnvironment`

- [ ] **Step 1: Write isolation metadata test**

Add `tests/code_mode_execution_environment.rs`:

```rust
use xiaomaolv::code_mode::AgentCodeModeSettings;
use xiaomaolv::harness::execution_environment::{
    ExecutionEnvironment, ExecutionIsolation, LocalExecutionEnvironment,
    SubprocessExecutionEnvironment,
};

#[test]
fn execution_environments_report_isolation_level() {
    let settings = AgentCodeModeSettings::default();
    let local = LocalExecutionEnvironment::new(settings.clone());
    let subprocess = SubprocessExecutionEnvironment::new(settings);

    assert_eq!(local.isolation(), ExecutionIsolation::InProcess);
    assert_eq!(subprocess.isolation(), ExecutionIsolation::SubprocessNoSandbox);
    assert!(!subprocess.isolation().is_security_sandbox());
}
```

- [ ] **Step 2: Run the focused test and confirm failure**

Run:

```bash
cargo test --test code_mode_execution_environment execution_environments_report_isolation_level -- --nocapture
```

Expected: FAIL because `execution_environment` does not exist.

- [ ] **Step 3: Add environment interface and adapters**

Create `src/harness/execution_environment.rs`:

```rust
use async_trait::async_trait;

use crate::code_mode::{
    execute_plan_via_subprocess, AgentCodeModeSettings, CodeModeExecutionReport,
    CodeModeExecutor, CodeModePlan,
};
use crate::mcp::{McpRuntime, McpToolInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionIsolation {
    InProcess,
    SubprocessNoSandbox,
}

impl ExecutionIsolation {
    pub fn is_security_sandbox(self) -> bool {
        false
    }
}

#[async_trait]
pub trait ExecutionEnvironment: Send + Sync {
    fn isolation(&self) -> ExecutionIsolation;

    async fn execute(
        &self,
        runtime: &McpRuntime,
        plan: &CodeModePlan,
        tools: &[McpToolInfo],
    ) -> anyhow::Result<CodeModeExecutionReport>;
}

pub struct LocalExecutionEnvironment {
    settings: AgentCodeModeSettings,
}

impl LocalExecutionEnvironment {
    pub fn new(settings: AgentCodeModeSettings) -> Self {
        Self { settings }
    }
}

#[async_trait]
impl ExecutionEnvironment for LocalExecutionEnvironment {
    fn isolation(&self) -> ExecutionIsolation {
        ExecutionIsolation::InProcess
    }

    async fn execute(
        &self,
        runtime: &McpRuntime,
        plan: &CodeModePlan,
        tools: &[McpToolInfo],
    ) -> anyhow::Result<CodeModeExecutionReport> {
        CodeModeExecutor::new(self.settings.clone())
            .execute(runtime, plan, tools)
            .await
    }
}

pub struct SubprocessExecutionEnvironment {
    settings: AgentCodeModeSettings,
}

impl SubprocessExecutionEnvironment {
    pub fn new(settings: AgentCodeModeSettings) -> Self {
        Self { settings }
    }
}

#[async_trait]
impl ExecutionEnvironment for SubprocessExecutionEnvironment {
    fn isolation(&self) -> ExecutionIsolation {
        ExecutionIsolation::SubprocessNoSandbox
    }

    async fn execute(
        &self,
        _runtime: &McpRuntime,
        plan: &CodeModePlan,
        tools: &[McpToolInfo],
    ) -> anyhow::Result<CodeModeExecutionReport> {
        execute_plan_via_subprocess(plan, tools, &self.settings).await
    }
}
```

Modify `src/harness/mod.rs` to export `execution_environment`.

- [ ] **Step 4: Route `complete_with_code_mode` through the environment interface**

In `src/service.rs`, replace the direct `match self.agent_code_mode.execution_mode` block with:

```rust
let environment: Box<dyn crate::harness::execution_environment::ExecutionEnvironment> =
    match self.agent_code_mode.execution_mode {
        CodeModeExecutionMode::Local => Box::new(
            crate::harness::execution_environment::LocalExecutionEnvironment::new(
                self.agent_code_mode.clone(),
            ),
        ),
        CodeModeExecutionMode::Subprocess => Box::new(
            crate::harness::execution_environment::SubprocessExecutionEnvironment::new(
                self.agent_code_mode.clone(),
            ),
        ),
    };
let execution = environment.execute(runtime, &plan, &code_mode_tools).await;
```

Add the isolation value to `CodeModeAuditRecord.reason` only when execution falls back because of an error:

```rust
reason: Some(format!("{}; isolation={:?}", err, environment.isolation())),
```

- [ ] **Step 5: Update docs**

In `docs/code-mode-observability.md`, add this exact paragraph under the Code Mode safety section:

```markdown
Code Mode execution environments are explicit about isolation. `local` runs in process. `subprocess` runs a child xiaomaolv process and rebuilds the MCP runtime with the filtered tool manifest, but it is not an OS security sandbox. Capability flags (`allow_network`, `allow_filesystem`, `allow_env`) filter tool access before execution; they do not confine arbitrary child process behavior outside the selected MCP tools.
```

- [ ] **Step 6: Run tests**

Run:

```bash
cargo test --test code_mode_execution_environment -- --nocapture
cargo test --all-targets code_mode -- --nocapture
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/harness/mod.rs src/harness/execution_environment.rs src/service.rs docs/code-mode-observability.md tests/code_mode_execution_environment.rs
git commit -m "refactor: make code mode execution environment explicit"
```

**Acceptance Criteria:**
- Code Mode execution isolation is visible in code and docs.
- Local and subprocess execution behavior remains unchanged.
- Future sandbox adapters can satisfy the same interface.

---

### Task 6: Split Skills Selection From Prompt Rendering

**Priority:** P2

**Problem:** `SkillRuntime::build_system_prompt` combines skill matching, ordering, truncation, and rendering. The harness cannot observe which skills were selected except by inspecting prompt text.

**Files:**
- Modify: `src/skills.rs`
- Modify: `src/service.rs`
- Test: `tests/skills_registry.rs`
- Test: `tests/service_skills_runtime.rs`

**Interfaces:**
- Produces:
  - `pub struct SelectedSkill`
  - `pub struct SkillSelection`
  - `pub fn select(&self, query: &str, settings: &SkillRuntimeSelectionSettings) -> SkillSelection`
  - `pub fn render_system_prompt(selection: &SkillSelection, max_prompt_chars: usize) -> Option<String>`

- [ ] **Step 1: Write selection test**

Add to `tests/skills_registry.rs`:

```rust
#[tokio::test]
async fn skill_runtime_exposes_selection_before_rendering() {
    let tmp = tempfile::tempdir().expect("tmp");
    let registry = SkillRegistry::new(test_paths(&tmp)).expect("registry");
    let skill_dir = tmp.path().join("source-skill");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: Rust Test Skill\ndescription: helps with cargo tests\ntags: [rust,test]\n---\nUse cargo test.",
    )
    .expect("write skill");
    registry
        .install_local_skill(
            SkillScope::User,
            &skill_dir,
            Some("rust-test-skill"),
            SkillActivationMode::Semantic,
        )
        .await
        .expect("install");

    let runtime = SkillRuntime::from_registry(&registry).await.expect("runtime");
    let selection = runtime.select(
        "please fix cargo test failures",
        &SkillRuntimeSelectionSettings {
            max_selected: 3,
            max_prompt_chars: 8000,
            match_min_score: 0.1,
        },
    );

    assert_eq!(selection.skills.len(), 1);
    assert_eq!(selection.skills[0].id, "rust-test-skill");
    assert!(selection.skills[0].score > 0.0);

    let prompt = SkillRuntime::render_system_prompt(&selection, 8000).expect("prompt");
    assert!(prompt.contains("SKILLS_CONTEXT"));
    assert!(prompt.contains("Use cargo test."));
}
```

- [ ] **Step 2: Run the focused test and confirm failure**

Run:

```bash
cargo test --test skills_registry skill_runtime_exposes_selection_before_rendering -- --nocapture
```

Expected: FAIL because `select` and `render_system_prompt` do not exist.

- [ ] **Step 3: Add selection structs and methods**

In `src/skills.rs`, add:

```rust
#[derive(Debug, Clone)]
pub struct SelectedSkill {
    pub id: String,
    pub mode: SkillActivationMode,
    pub score: f32,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillSelection {
    pub skills: Vec<SelectedSkill>,
}
```

Move the candidate scoring and ordering logic from `build_system_prompt` into:

```rust
pub fn select(
    &self,
    query: &str,
    settings: &SkillRuntimeSelectionSettings,
) -> SkillSelection
```

Add:

```rust
pub fn render_system_prompt(
    selection: &SkillSelection,
    max_prompt_chars: usize,
) -> Option<String>
```

Keep `build_system_prompt` as a compatibility wrapper:

```rust
pub fn build_system_prompt(
    &self,
    query: &str,
    settings: &SkillRuntimeSelectionSettings,
) -> Option<String> {
    let selection = self.select(query, settings);
    Self::render_system_prompt(&selection, settings.max_prompt_chars)
}
```

- [ ] **Step 4: Record selected skill IDs in service logs**

In `MessageService::apply_skills_prompt`, call `select` first and log IDs before rendering:

```rust
let selection = runtime.select(query_text, &settings);
if !selection.skills.is_empty() {
    info!(
        skills = ?selection.skills.iter().map(|skill| skill.id.as_str()).collect::<Vec<_>>(),
        "selected skills for agent run"
    );
}
if let Some(prompt) = SkillRuntime::render_system_prompt(&selection, settings.max_prompt_chars) {
    history.push(StoredMessage {
        role: MessageRole::System,
        content: prompt,
    });
}
```

- [ ] **Step 5: Run skill tests**

Run:

```bash
cargo test --test skills_registry -- --nocapture
cargo test --test service_skills_runtime -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/skills.rs src/service.rs tests/skills_registry.rs tests/service_skills_runtime.rs
git commit -m "refactor: expose skill selection"
```

**Acceptance Criteria:**
- Skill selection can be tested and observed without parsing prompt text.
- Existing prompt output remains compatible.
- Service logs selected skill IDs for future trajectory integration.

---

### Task 7: Integrate AgentRun Gradually Into MessageService

**Priority:** P3

**Problem:** `AgentRun` exists but `MessageService` still directly controls trajectory start/log/finish in Code Mode and MCP loops.

**Files:**
- Modify: `src/service.rs`
- Test: `tests/service_harness_trajectory.rs`
- Test: `tests/service_mcp_loop.rs`
- Test: `tests/service_streaming.rs`

**Interfaces:**
- Consumes: `AgentRun`, `AgentRunStart`, `AgentRunExit`.
- Produces: no new public interface; this task reduces direct `TrajectoryRun` usage in `MessageService`.

- [ ] **Step 1: Add regression test for one finish per MCP trajectory**

Add to `tests/service_harness_trajectory.rs`:

```rust
#[tokio::test]
async fn mcp_loop_agent_run_finishes_once_on_parse_error_recovery() {
    let store = SqliteMemoryStore::new("sqlite::memory:").await.expect("store");
    let backend = Arc::new(SqliteMemoryBackend::new(store.clone()));
    let provider = Arc::new(FakeProvider::new(vec![
        "MCP_TOOL_CALL_JSON: {bad".to_string(),
        "I cannot use that malformed tool call safely.".to_string(),
    ]));
    let runtime = Arc::new(tokio::sync::RwLock::new(McpRuntime::default()));
    let service = MessageService::new_with_backend(
        provider,
        backend,
        Some(runtime),
        AgentMcpSettings {
            enabled: true,
            max_iterations: 2,
            max_tool_result_chars: 4000,
        },
        8,
        0,
        0,
    )
    .with_harness_config(&AgentHarnessConfig {
        enable_trajectory: true,
        ..AgentHarnessConfig::default()
    });

    let _ = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "session-parse".to_string(),
            user_id: "user-parse".to_string(),
            text: "use a tool".to_string(),
            reply_target: None,
        })
        .await;

    let records = store
        .query_trajectories(TrajectoryFilter {
            session_id: Some("session-parse".to_string()),
            channel: Some("http".to_string()),
            user_id: None,
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");
    assert!(records.len() <= 1);
    if let Some(record) = records.first() {
        assert!(record.finished_at.is_some());
    }
}
```

If existing fake provider helpers are private to another test file, copy the minimal fake provider into this test file with a `Mutex<VecDeque<String>>` reply queue.

- [ ] **Step 2: Run the regression test**

Run:

```bash
cargo test --test service_harness_trajectory mcp_loop_agent_run_finishes_once_on_parse_error_recovery -- --nocapture
```

Expected: PASS before refactor or FAIL because `McpRuntime::default()` has only built-in tools and the test setup needs one fake tool. If it fails for missing tools, add the built-in time tool request to the fake provider reply instead of `MCP_TOOL_CALL_JSON: {bad`.

- [ ] **Step 3: Replace Code Mode trajectory usage with AgentRun**

In `complete_with_code_mode`, replace `TrajectoryRun::start(...)` with:

```rust
let mut run = AgentRun::start(AgentRunStart {
    logger: self.trajectory_logger.clone(),
    metrics: self.trajectory_metrics.clone(),
    session_id: incoming.session_id.clone(),
    channel: incoming.channel.clone(),
    user_id: incoming.user_id.clone(),
    model,
})
.await;
```

Replace `trajectory.log_tool_call(...)` with `run.record_tool_call(...)`.

Replace internal error finish calls with:

```rust
run.finish(AgentRunExit::InternalError).await;
```

Replace final answer finish with:

```rust
run.observe_iteration(0);
run.finish(AgentRunExit::FinalAnswer(reply.clone())).await;
```

- [ ] **Step 4: Replace MCP loop trajectory usage with AgentRun**

In `complete_with_mcp_loop` and `complete_with_mcp_loop_stream`:

- Replace `TrajectoryRun::start(...)` with `AgentRun::start(...)`.
- Replace `trajectory.observe_iteration(iteration)` with `run.observe_iteration(iteration)`.
- Replace `trajectory.log_tool_call(record)` with `run.record_tool_call(record)`.
- Replace `TrajectoryExitReason::FinalAnswer` finish with `AgentRunExit::FinalAnswer(reply.clone())`.
- Replace `TrajectoryExitReason::ToolError` finish with `AgentRunExit::ToolError(final_reply.clone())`.
- Replace `TrajectoryExitReason::MaxIterations` finish with `AgentRunExit::MaxIterations(final_reply.clone())`.
- Replace internal error finish with `AgentRunExit::InternalError`.

- [ ] **Step 5: Remove direct `TrajectoryRun` import from service**

`src/service.rs` should import:

```rust
use crate::harness::run::{AgentRun, AgentRunExit, AgentRunStart};
use crate::harness::trajectory::{ToolCallRecord, TrajectoryLogger};
```

It should not import `TrajectoryRun` after this task.

- [ ] **Step 6: Run service harness tests**

Run:

```bash
cargo test --test service_harness_trajectory -- --nocapture
cargo test --test service_mcp_loop -- --nocapture
cargo test --test service_streaming -- --nocapture
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/service.rs tests/service_harness_trajectory.rs
git commit -m "refactor: route service trajectories through agent run"
```

**Acceptance Criteria:**
- `MessageService` no longer starts or finishes `TrajectoryRun` directly.
- Code Mode, MCP non-stream, and MCP stream paths use `AgentRun`.
- Existing trajectory records remain query-compatible.

---

### Task 8: Align Eval, Docs, And Operator Defaults

**Priority:** P3

**Problem:** The harness shape will be deeper after Tasks 1-7, but docs and evals need to teach the new module interfaces and preserve regression coverage.

**Files:**
- Modify: `docs/agent-harness-eval.md`
- Modify: `docs/mcp-integration.md`
- Modify: `docs/code-mode-observability.md`
- Modify: `docs/plans/2026-06-03-agent-harness-prioritized-roadmap.md`
- Modify: `tests/harness_eval.rs`

**Interfaces:**
- Consumes: all public harness interfaces introduced in previous tasks.
- Produces: updated operator docs and deterministic eval cases.

- [ ] **Step 1: Add eval case labels**

In `tests/harness_eval.rs`, add or update cases so the deterministic subset includes these named cases:

```rust
const CASE_AGENT_RUN_FINAL_ANSWER: &str = "agent_run_final_answer";
const CASE_TOOL_PROTOCOL_SCHEMA_RETRY: &str = "tool_protocol_schema_retry";
const CASE_OUTPUT_EXIT_BLOCK_HIDDEN_TOOL_ERROR: &str = "output_exit_block_hidden_tool_error";
const CASE_SKILL_SELECTION_VISIBLE: &str = "skill_selection_visible";
```

Each case should assert:

- final answer text
- trajectory exit reason
- tool call count
- verification issue marker when present

- [ ] **Step 2: Run harness eval**

Run:

```bash
cargo test --test harness_eval -- --nocapture
```

Expected: PASS.

- [ ] **Step 3: Update `docs/agent-harness-eval.md`**

Replace the scenario list with:

```markdown
Covered scenarios:

- AgentRun lifecycle: final answer, tool error, max iterations, internal error.
- ToolProtocol: valid tool call, malformed JSON recovery, unknown tool rejection, schema-invalid arguments.
- Context compaction: no compaction, head-tail compaction, budget-based compaction, persisted summary reuse.
- OutputExit: observe, revise once, block hidden tool errors, block unresolved tool-call JSON.
- Skills runtime: selected skill IDs are observable before prompt rendering.
```

- [ ] **Step 4: Update `docs/mcp-integration.md`**

In the Agent Auto Tool Loop section, add:

```markdown
Implementation note: MCP orchestration is owned by the harness ToolProtocol module. The model still emits the same JSON shape, but parsing, tool existence checks, JSON Schema argument checks, result envelopes, retry/block feedback, and trajectory tool-call records are handled by ToolProtocol instead of ad hoc service code.
```

- [ ] **Step 5: Update roadmap status**

Append to `docs/plans/2026-06-03-agent-harness-prioritized-roadmap.md`:

```markdown
## 2026-06-17 Follow-up

The next optimization phase deepens the harness interfaces:

- `HarnessStore` separates harness persistence from conversation memory.
- `AgentRun` owns start/log/finish lifecycle semantics.
- `ToolProtocol` owns MCP proposal parsing, validation, execution envelopes, and feedback.
- `OutputExit` owns final answer verification and bounded revision/block behavior.
- `ExecutionEnvironment` makes Code Mode isolation explicit.
- `SkillRuntime::select` exposes selected skill metadata before prompt rendering.
```

- [ ] **Step 6: Run full verification**

Run:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Expected: all commands exit 0.

- [ ] **Step 7: Commit**

```bash
git add docs/agent-harness-eval.md docs/mcp-integration.md docs/code-mode-observability.md docs/plans/2026-06-03-agent-harness-prioritized-roadmap.md tests/harness_eval.rs
git commit -m "docs: align agent harness optimization plan"
```

**Acceptance Criteria:**
- Docs name the new harness modules and their responsibilities.
- Harness eval covers lifecycle, protocol, output exit, and skill selection.
- Full format, lint, and test suite pass.

---

## Execution Order

1. Task 1 first because persistence semantics and `call_index` correctness affect every later trajectory test.
2. Task 2 next because `AgentRun` gives later work a stable lifecycle interface.
3. Task 3 before Task 7 because MCP loops should delegate protocol details before they delegate lifecycle.
4. Task 4 before Task 7 because service integration should finish through one output path.
5. Task 5 and Task 6 can run in parallel after Task 2 if separate workers touch disjoint files.
6. Task 7 integrates the earlier modules into `MessageService`.
7. Task 8 is the final documentation and regression pass.

## Verification Checklist

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --all-targets`
- [ ] `cargo test --test harness_store -- --nocapture`
- [ ] `cargo test --test harness_agent_run -- --nocapture`
- [ ] `cargo test --test harness_tool_protocol -- --nocapture`
- [ ] `cargo test --test harness_output_exit -- --nocapture`
- [ ] `cargo test --test code_mode_execution_environment -- --nocapture`
- [ ] `cargo test --test harness_eval -- --nocapture`

## Self-Review

- Spec coverage: each architectural gap from the 2026-06-17 review maps to at least one task.
- Placeholder scan: the plan avoids placeholder markers and names concrete files, commands, interfaces, and expected results.
- Type consistency: `AgentRun`, `ToolProtocol`, `OutputExit`, `ExecutionEnvironment`, and `SkillRuntime::select` are introduced before any task consumes them.
