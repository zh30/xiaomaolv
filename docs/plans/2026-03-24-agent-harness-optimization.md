# Agent Harness Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enhance xiaomaolv's Agent Harness capabilities: trajectory logging, context compaction, tool call verification, and observability for continuous improvement.

**Architecture:** Add new harness layers around the existing MCP loop without disrupting current functionality. New components are opt-in via config. Trajectory data is stored in SQLite alongside existing memory schema.

**Tech Stack:** Rust (Tokio, SQLx/SQLite, Tracing), existing MCP runtime

---

## Current State Analysis

| Harness Component | Status | Gap |
|-------------------|--------|-----|
| Tool Integration | ✅ MCP, Skills, Code Mode | None |
| Memory/State | ✅ Hybrid SQLite+zvec | No trajectory persistence |
| Context Management | ✅ Token budget, memory injection | No dynamic compaction |
| Planning/Decomposition | ✅ Agent Swarm | None |
| Verification/Guardrails | ⚠️ Partial | No output verification |
| Observability | ⚠️ Basic metrics | No trajectory capture |

---

### Task 1: Trajectory Logging Infrastructure

**Files:**
- Create: `src/harness/trajectory.rs` (new module)
- Modify: `src/service.rs` (integrate trajectory capture)
- Modify: `src/memory.rs` (add trajectory tables)
- Create: `tests/harness_trajectory.rs`

**Step 1: Define trajectory schema and Rust types**

```rust
// src/harness/trajectory.rs
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub server: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    pub result: serde_json::Value,
    pub ok: bool,
    pub duration_ms: u64,
    pub iteration: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryRecord {
    pub id: String,
    pub session_id: String,
    pub channel: String,
    pub user_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub final_answer: Option<String>,
    pub exit_reason: TrajectoryExitReason,
    pub model: String,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TrajectoryExitReason {
    FinalAnswer,
    MaxIterations,
    ToolError,
    Timeout,
    InternalError,
}
```

**Step 2: Add trajectory database tables to memory.rs**

```sql
-- Add to memory schema
CREATE TABLE IF NOT EXISTS mcp_trajectories (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    channel TEXT NOT NULL,
    user_id TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    final_answer TEXT,
    exit_reason TEXT NOT NULL,
    model TEXT NOT NULL,
    total_tokens INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS mcp_trajectory_tool_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    trajectory_id TEXT NOT NULL REFERENCES mcp_trajectories(id),
    iteration INTEGER NOT NULL,
    server TEXT NOT NULL,
    tool TEXT NOT NULL,
    arguments TEXT NOT NULL,
    result TEXT NOT NULL,
    ok INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    UNIQUE(trajectory_id, iteration, server, tool)
);

CREATE INDEX idx_trajectory_session ON mcp_trajectories(session_id, started_at);
CREATE INDEX idx_trajectory_channel ON mcp_trajectories(channel, started_at);
```

**Step 3: Implement TrajectoryLogger in harness/trajectory.rs**

```rust
pub struct TrajectoryLogger {
    memory: Arc<dyn MemoryStore>,
    enabled: bool,
}

impl TrajectoryLogger {
    pub async fn log_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()> {
        if !self.enabled { return Ok(()); }
        // Store to database
        self.memory.insert_trajectory_tool_call(trajectory_id, record).await
    }

    pub async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()> {
        if !self.enabled { return Ok(()); }
        self.memory.finish_trajectory(trajectory_id, final_answer, exit_reason).await
    }
}
```

**Step 4: Add MemoryStore trait methods**

Modify `src/memory.rs` to add to `MemoryStore` trait:
```rust
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
async fn query_trajectories(
    &self,
    filter: TrajectoryFilter,
) -> anyhow::Result<Vec<TrajectoryRecord>>;
```

**Step 5: Integrate into MCP loop in service.rs**

In `complete_with_mcp_loop` and `complete_with_mcp_loop_stream`:
- Create trajectory at loop start
- Log each tool call with timing
- Log final answer or max_iterations on exit

```rust
// In complete_with_mcp_loop_stream (service.rs ~line 2038)
let trajectory_id = format!("traj-{}-{}", session_id, uuid::Uuid::new_v4());
let trajectory_logger = TrajectoryLogger::new(self.memory.clone(), self.agent_harness.enable_trajectory);
trajectory_logger.start(trajectory_id, /* ... */).await?;
```

**Step 6: Write tests**

```rust
// tests/harness_trajectory.rs
#[tokio::test]
async fn test_trajectory_records_tool_calls() {
    // Test that tool calls are recorded with timing
}

#[tokio::test]
async fn test_trajectory_captures_final_answer() {
    // Test that final answer is stored
}

#[tokio::test]
async fn test_trajectory_filter_by_session() {
    // Test querying trajectories by session
}
```

---

### Task 2: Context Compaction (Long Conversation Optimization)

**Files:**
- Create: `src/harness/compactor.rs`
- Modify: `src/memory.rs` (add compaction methods)
- Create: `tests/harness_compactor.rs`

**Step 1: Define CompactionStrategy enum**

```rust
// src/harness/compactor.rs
pub enum CompactionStrategy {
    /// Keep first and last N messages, summarize middle
    HeadTail { head_count: usize, tail_count: usize },
    /// Summarize messages older than N days
    AgeBased { max_age_days: usize },
    /// Compact when token budget exceeds threshold
    BudgetBased { max_tokens: usize },
}

pub struct CompactionRequest {
    pub messages: Vec<StoredMessage>,
    pub strategy: CompactionStrategy,
    pub model: Arc<dyn ChatProvider>,
}

pub struct CompactionResult {
    pub compacted_messages: Vec<StoredMessage>,
    pub tokens_saved: usize,
    pub summary: String,
}
```

**Step 2: Implement compaction logic**

```rust
impl Compactor {
    pub async fn compact(
        &self,
        request: CompactionRequest,
    ) -> anyhow::Result<CompactionResult> {
        match request.strategy {
            CompactionStrategy::HeadTail { head_count, tail_count } => {
                self.compact_head_tail(request.messages, head_count, tail_count).await
            }
            // ...
        }
    }

    async fn compact_head_tail(
        &self,
        messages: Vec<StoredMessage>,
        head_count: usize,
        tail_count: usize,
    ) -> anyhow::Result<CompactionResult> {
        if messages.len() <= head_count + tail_count {
            return Ok(CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            });
        }

        let head = &messages[..head_count];
        let middle = &messages[head_count..messages.len() - tail_count];
        let tail = &messages[messages.len() - tail_count..];

        let middle_summary = self.summarize(middle).await?;
        let compacted = vec![
            head.to_vec(),
            vec![StoredMessage {
                role: MessageRole::System,
                content: format!("[Earlier {} messages summarized: {}]", middle.len(), middle_summary),
            }],
            tail.to_vec(),
        ].into_iter().flatten().collect();

        Ok(CompactionResult { compacted_messages: compacted, tokens_saved: 0, summary: middle_summary })
    }
}
```

**Step 3: Add configuration to AgentConfig**

In `src/config.rs`:
```rust
pub struct AgentHarnessConfig {
    pub enable_trajectory: bool,
    pub enable_compaction: bool,
    pub compaction_strategy: String,  // "head_tail", "age_based", "budget_based"
    pub compaction_head_count: usize,
    pub compaction_tail_count: usize,
    pub compaction_max_tokens: usize,
}
```

**Step 4: Integrate into message processing**

In `Service::process_message` (service.rs), after fetching memory but before calling provider:
```rust
if self.agent_harness.enable_compaction {
    let history = self.compactor.compact(CompactionRequest {
        messages: history,
        strategy: self.agent_harness.strategy(),
        model: self.provider.clone(),
    }).await?;
}
```

**Step 5: Write tests**

```rust
// tests/harness_compactor.rs
#[tokio::test]
async fn test_head_tail_compaction_preserves_context() {
    let messages = create_test_conversation(20);
    let result = compactor.compact(messages, CompactionStrategy::HeadTail { head_count: 2, tail_count: 2 }).await;
    assert_eq!(result.compacted_messages.len(), 5);  // 2 head + 1 summary + 2 tail
}
```

---

### Task 3: Tool Call Verification

**Files:**
- Create: `src/harness/verifier.rs`
- Modify: `src/service.rs` (integrate verification)
- Create: `tests/harness_verifier.rs`

**Step 1: Define VerificationResult and Verifier**

```rust
// src/harness/verifier.rs
#[derive(Debug, Clone)]
pub struct VerificationResult {
    pub passed: bool,
    pub confidence: f64,
    pub issues: Vec<VerificationIssue>,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VerificationIssue {
    pub severity: IssueSeverity,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IssueSeverity {
    Warning,
    Error,
    Critical,
}

pub trait ToolCallVerifier: Send + Sync {
    fn verify(&self, tool_call: &ToolCallRecord) -> VerificationResult;
}
```

**Step 2: Implement built-in verifiers**

```rust
pub struct SchemaVerifier;
pub struct SemanticVerifier;
pub struct TimingVerifier { max_duration_ms: u64 }

impl ToolCallVerifier for TimingVerifier {
    fn verify(&self, call: &ToolCallRecord) -> VerificationResult {
        if call.duration_ms > self.max_duration_ms {
            VerificationResult {
                passed: true,  // Not a failure, just a warning
                confidence: 0.8,
                issues: vec![VerificationIssue {
                    severity: IssueSeverity::Warning,
                    code: "SLOW_TOOL".to_string(),
                    message: format!("Tool took {}ms (>{}ms threshold)", call.duration_ms, self.max_duration_ms),
                }],
                suggestion: Some("Consider caching this result".to_string()),
            }
        } else {
            VerificationResult { passed: true, confidence: 1.0, issues: vec![], suggestion: None }
        }
    }
}
```

**Step 3: Integrate into MCP loop**

In `complete_with_mcp_loop` (service.rs), after each tool call:
```rust
let tool_result = runtime.call_tool(&tool_call.server, &tool_call.tool, tool_call.arguments.clone()).await;

if let Some(verifier) = &self.tool_verifier {
    let verification = verifier.verify(&ToolCallRecord {
        server: tool_call.server.clone(),
        tool: tool_call.tool.clone(),
        arguments: tool_call.arguments.clone(),
        result: tool_result.clone().unwrap_or_default(),
        ok: tool_result.is_ok(),
        duration_ms: elapsed,
        iteration,
    });
    
    if !verification.passed {
        warn!(?verification.issues, "Tool call verification failed");
        // Could retry, fall back, or continue with warning
    }
}
```

**Step 4: Add output verification (self-check before final answer)**

Before returning final answer in MCP loop:
```rust
let final_answer = /* ... */;

// Self-verification: check answer quality
if let Some(output_verifier) = &self.output_verifier {
    let check = output_verifier.check(&final_answer, &history).await;
    if !check.passed {
        debug!(confidence = check.confidence, "Output verification flagged issues");
        // Log but don't block - could add retry loop here
    }
}
```

---

### Task 4: Observability Dashboard Endpoint

**Files:**
- Modify: `src/http.rs` (add trajectory endpoints)
- Create: `src/harness/observability.rs`
- Create: `tests/harness_observability.rs`

**Step 1: Add trajectory query endpoints**

In `src/http.rs`:
```rust
// GET /v1/harness/trajectories
async fn list_trajectories(
    State(state): State<AppState>,
    Query(params): Query<TrajectoryQuery>,
) -> Result<Json<TrajectoryListResponse>> {
    let trajectories = state.service.query_trajectories(params).await?;
    Ok(Json(TrajectoryListResponse { trajectories }))
}

// GET /v1/harness/trajectories/{id}
async fn get_trajectory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TrajectoryDetailResponse>> {
    let trajectory = state.service.get_trajectory_detail(id).await?;
    Ok(Json(TrajectoryDetailResponse { trajectory }))
}
```

**Step 2: Add Prometheus metrics for trajectory analysis**

```rust
// In observability.rs
pub struct TrajectoryMetrics {
    pub trajectories_total: IntCounter,
    pub tool_calls_total: IntCounterVec,
    pub avg_iterations_per_trajectory: Gauge,
    pub trajectory_duration_seconds: Histogram,
    pub tool_call_duration_seconds: HistogramVec,
}

impl TrajectoryMetrics {
    pub fn describe(&self, registry: &Registry) {
        registry.register(self.trajectories_total);
        registry.register(self.tool_calls_total);
        // ...
    }
}
```

**Step 3: Integration test**

```rust
// tests/harness_observability.rs
#[tokio::test]
async fn test_trajectory_endpoint_returns_results() {
    let app = create_test_app().await;
    let response = app.oneshot(
        Request::builder()
            .uri("/v1/harness/trajectories?limit=10")
            .body(Body::empty())
            .unwrap(),
    ).await;
    assert_eq!(response.status(), StatusCode::OK);
}
```

---

### Task 5: Configuration Schema Updates

**Files:**
- Modify: `src/config.rs`
- Modify: `config/xiaomaolv.example.toml`

**Step 1: Add harness config section**

In `src/config.rs`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHarnessConfig {
    #[serde(default)]
    pub enable_trajectory: bool,
    #[serde(default)]
    pub enable_compaction: bool,
    #[serde(default = "default_compaction_strategy")]
    pub compaction_strategy: String,
    #[serde(default = "default_compaction_head_count")]
    pub compaction_head_count: usize,
    #[serde(default = "default_compaction_tail_count")]
    pub compaction_tail_count: usize,
    #[serde(default = "default_compaction_max_tokens")]
    pub compaction_max_tokens: usize,
    #[serde(default)]
    pub enable_verification: bool,
    #[serde(default)]
    pub verification_max_tool_duration_ms: u64,
}
```

**Step 2: Update config parsing**

In `AppConfig::from_file` or `AgentConfig`, integrate `AgentHarnessConfig`.

**Step 3: Update example config**

In `config/xiaomaolv.example.toml`:
```toml
[agent.harness]
# Enable trajectory logging for feedback loops
enable_trajectory = true

# Enable context compaction for long conversations
enable_compaction = false
compaction_strategy = "head_tail"  # head_tail | age_based | budget_based
compaction_head_count = 10
compaction_tail_count = 10
compaction_max_tokens = 16000

# Enable tool call verification
enable_verification = false
verification_max_tool_duration_ms = 5000
```

---

### Task 6: Verification

**Files:**
- Test: All modified files

**Step 1: Format check**

Run: `cargo fmt --all`
Expected: no formatting diffs afterward.

**Step 2: Strict lint check**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: pass with zero warnings.

**Step 3: Unit tests**

Run: `cargo test --all-targets`
Expected: all tests pass.

**Step 4: Integration test (if exists)**

Run: `cargo test --test <integration_test_name> -- --nocapture`
Expected: pass.

---

## Subsystem Independence

| Task | Dependencies | Can Start After |
|------|--------------|------------------|
| Task 1: Trajectory Logging | None | Foundation |
| Task 2: Compaction | Task 1 | Task 1 |
| Task 3: Verification | Task 1 | Task 1 |
| Task 4: Observability | Task 1 | Task 1 |
| Task 5: Config | None | Can start early |
| Task 6: Verification | All above | All above |

**Recommendation:** Start Task 1 (Trajectory Logging) and Task 5 (Config) in parallel, then Tasks 2-4 can proceed in parallel after Task 1.

---

## Key Files to Modify Summary

| File | Changes |
|------|---------|
| `src/harness/trajectory.rs` | NEW - trajectory logging |
| `src/harness/compactor.rs` | NEW - context compaction |
| `src/harness/verifier.rs` | NEW - tool/output verification |
| `src/harness/observability.rs` | NEW - metrics/endpoint helpers |
| `src/service.rs` | Integrate all harness components |
| `src/memory.rs` | Add trajectory storage |
| `src/config.rs` | Add harness config section |
| `src/http.rs` | Add trajectory API endpoints |
| `config/xiaomaolv.example.toml` | Document harness settings |
| `tests/harness_*.rs` | NEW - unit tests per component |
