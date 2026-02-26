use std::collections::HashSet;
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::domain::{MessageRole, StoredMessage};
use crate::mcp::{McpConfigPaths, McpRegistry, McpRuntime, McpToolInfo};
use crate::provider::{ChatProvider, CompletionRequest};

const DEFAULT_PLANNER_MAX_TOOLS: usize = 48;
const DEFAULT_PLANNER_MAX_HISTORY: usize = 8;
const CODE_MODE_PROTOCOL_VERSION: u32 = 1;
const CODE_MODE_SUBPROCESS_TOKEN_ENV: &str = "XIAOMAOLV_CODE_MODE_EXEC_TOKEN";
const CODE_MODE_SUBPROCESS_MAX_PAYLOAD_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCodeModeSettings {
    #[serde(default = "default_code_mode_enabled")]
    pub enabled: bool,
    #[serde(default = "default_code_mode_shadow")]
    pub shadow_mode: bool,
    #[serde(default = "default_code_mode_max_calls")]
    pub max_calls: usize,
    #[serde(default = "default_code_mode_max_parallel")]
    pub max_parallel: usize,
    #[serde(default = "default_code_mode_max_runtime_ms")]
    pub max_runtime_ms: u64,
    #[serde(default = "default_code_mode_max_call_timeout_ms")]
    pub max_call_timeout_ms: u64,
    #[serde(default = "default_code_mode_timeout_warn_ratio")]
    pub timeout_warn_ratio: f64,
    #[serde(default = "default_code_mode_timeout_auto_shadow_enabled")]
    pub timeout_auto_shadow_enabled: bool,
    #[serde(default = "default_code_mode_timeout_auto_shadow_probe_every")]
    pub timeout_auto_shadow_probe_every: usize,
    #[serde(default = "default_code_mode_timeout_auto_shadow_streak")]
    pub timeout_auto_shadow_streak: usize,
    #[serde(default = "default_code_mode_max_result_chars")]
    pub max_result_chars: usize,
    #[serde(default = "default_code_mode_execution_mode")]
    pub execution_mode: CodeModeExecutionMode,
    #[serde(default = "default_code_mode_subprocess_timeout_secs")]
    pub subprocess_timeout_secs: u64,
    #[serde(default)]
    pub allow_network: bool,
    #[serde(default)]
    pub allow_filesystem: bool,
    #[serde(default)]
    pub allow_env: bool,
}

impl Default for AgentCodeModeSettings {
    fn default() -> Self {
        Self {
            enabled: default_code_mode_enabled(),
            shadow_mode: default_code_mode_shadow(),
            max_calls: default_code_mode_max_calls(),
            max_parallel: default_code_mode_max_parallel(),
            max_runtime_ms: default_code_mode_max_runtime_ms(),
            max_call_timeout_ms: default_code_mode_max_call_timeout_ms(),
            timeout_warn_ratio: default_code_mode_timeout_warn_ratio(),
            timeout_auto_shadow_enabled: default_code_mode_timeout_auto_shadow_enabled(),
            timeout_auto_shadow_probe_every: default_code_mode_timeout_auto_shadow_probe_every(),
            timeout_auto_shadow_streak: default_code_mode_timeout_auto_shadow_streak(),
            max_result_chars: default_code_mode_max_result_chars(),
            execution_mode: default_code_mode_execution_mode(),
            subprocess_timeout_secs: default_code_mode_subprocess_timeout_secs(),
            allow_network: false,
            allow_filesystem: false,
            allow_env: false,
        }
    }
}

impl AgentCodeModeSettings {
    pub fn normalized_timeout_warn_ratio(&self) -> f64 {
        if !self.timeout_warn_ratio.is_finite() {
            return default_code_mode_timeout_warn_ratio();
        }
        self.timeout_warn_ratio.clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodeModeExecutionMode {
    Local,
    Subprocess,
}

fn default_code_mode_enabled() -> bool {
    false
}

fn default_code_mode_shadow() -> bool {
    true
}

fn default_code_mode_max_calls() -> usize {
    6
}

fn default_code_mode_max_parallel() -> usize {
    2
}

fn default_code_mode_max_runtime_ms() -> u64 {
    2500
}

fn default_code_mode_max_call_timeout_ms() -> u64 {
    1200
}

fn default_code_mode_timeout_warn_ratio() -> f64 {
    0.4
}

fn default_code_mode_timeout_auto_shadow_enabled() -> bool {
    false
}

fn default_code_mode_timeout_auto_shadow_probe_every() -> usize {
    5
}

fn default_code_mode_timeout_auto_shadow_streak() -> usize {
    3
}

fn default_code_mode_max_result_chars() -> usize {
    12000
}

fn default_code_mode_execution_mode() -> CodeModeExecutionMode {
    CodeModeExecutionMode::Local
}

fn default_code_mode_subprocess_timeout_secs() -> u64 {
    8
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeModeToolCall {
    pub server: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeModePlan {
    #[serde(default)]
    pub calls: Vec<CodeModeToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeModeCallResult {
    pub server: String,
    pub tool: String,
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeModeExecutionReport {
    pub calls: Vec<CodeModeCallResult>,
    pub failed_calls: usize,
    pub timed_out_calls: usize,
    pub truncated_outputs: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeModeSandboxRequest {
    pub protocol_version: u32,
    pub auth_token: String,
    pub plan: CodeModePlan,
    pub allowed_tools: Vec<CodeModeAllowedTool>,
    pub settings: AgentCodeModeSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeModeAllowedTool {
    pub server: String,
    pub tool: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeModeSandboxResponse {
    pub ok: bool,
    pub report: Option<CodeModeExecutionReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeModeAuditRecord {
    pub planner: String,
    pub used: bool,
    pub fallback: bool,
    pub reason: Option<String>,
    pub planned_calls: usize,
    pub executed_calls: usize,
    pub failed_calls: usize,
    pub timed_out_calls: usize,
    pub elapsed_ms: u128,
}

impl CodeModeAuditRecord {
    pub fn fallback(planner: &str, reason: impl Into<String>) -> Self {
        Self {
            planner: planner.to_string(),
            used: false,
            fallback: true,
            reason: Some(reason.into()),
            planned_calls: 0,
            executed_calls: 0,
            failed_calls: 0,
            timed_out_calls: 0,
            elapsed_ms: 0,
        }
    }
}

#[async_trait]
pub trait CodeModePlanner: Send + Sync {
    fn name(&self) -> &'static str {
        "custom"
    }

    async fn build_plan(
        &self,
        _history: &[StoredMessage],
        _tools: &[McpToolInfo],
    ) -> anyhow::Result<Option<CodeModePlan>>;
}

pub struct DisabledCodeModePlanner;

#[async_trait]
impl CodeModePlanner for DisabledCodeModePlanner {
    fn name(&self) -> &'static str {
        "disabled"
    }

    async fn build_plan(
        &self,
        _history: &[StoredMessage],
        _tools: &[McpToolInfo],
    ) -> anyhow::Result<Option<CodeModePlan>> {
        Ok(None)
    }
}

#[derive(Clone)]
pub struct LlmCodeModePlanner {
    provider: Arc<dyn ChatProvider>,
    max_tools_in_prompt: usize,
    max_history_messages: usize,
}

impl LlmCodeModePlanner {
    pub fn new(provider: Arc<dyn ChatProvider>) -> Self {
        Self {
            provider,
            max_tools_in_prompt: DEFAULT_PLANNER_MAX_TOOLS,
            max_history_messages: DEFAULT_PLANNER_MAX_HISTORY,
        }
    }

    pub fn with_limits(mut self, max_tools_in_prompt: usize, max_history_messages: usize) -> Self {
        self.max_tools_in_prompt = max_tools_in_prompt.max(1);
        self.max_history_messages = max_history_messages.max(1);
        self
    }

    fn build_tools_prompt_json(&self, tools: &[McpToolInfo]) -> anyhow::Result<String> {
        let tool_defs = tools
            .iter()
            .take(self.max_tools_in_prompt)
            .map(|t| {
                serde_json::json!({
                    "server": t.server,
                    "tool": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_string(&tool_defs).context("failed to serialize code mode planner tool defs")
    }
}

#[async_trait]
impl CodeModePlanner for LlmCodeModePlanner {
    fn name(&self) -> &'static str {
        "llm-plan-v1"
    }

    async fn build_plan(
        &self,
        history: &[StoredMessage],
        tools: &[McpToolInfo],
    ) -> anyhow::Result<Option<CodeModePlan>> {
        if tools.is_empty() {
            return Ok(None);
        }

        let mut history_view = history
            .iter()
            .rev()
            .take(self.max_history_messages)
            .cloned()
            .collect::<Vec<_>>();
        history_view.reverse();

        let serialized_tools = self.build_tools_prompt_json(tools)?;
        let serialized_history = serde_json::to_string(
            &history_view
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "role": m.role.as_str(),
                        "content": truncate_prompt_text(&m.content, 1200),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .context("failed to serialize code mode planner history")?;

        let planner_prompt = [
            "You are a secure MCP tool planner.",
            "Return ONLY a JSON object, no markdown, no explanations.",
            "Output schema:",
            r#"{"calls":[{"server":"<server>","tool":"<tool>","arguments":{...}}]}"#,
            "Rules:",
            "1) Use only tools listed in AVAILABLE_TOOLS_JSON.",
            "2) arguments must be a JSON object.",
            "3) Keep plan short and bounded.",
            "4) If no tool is required, return {\"calls\":[]}.",
        ]
        .join("\n");

        let user_prompt = format!(
            "AVAILABLE_TOOLS_JSON: {serialized_tools}\nCONVERSATION_JSON: {serialized_history}"
        );

        let reply = self
            .provider
            .complete(CompletionRequest {
                messages: vec![
                    StoredMessage {
                        role: MessageRole::System,
                        content: planner_prompt,
                    },
                    StoredMessage {
                        role: MessageRole::User,
                        content: user_prompt,
                    },
                ],
            })
            .await
            .context("code mode planner completion failed")?;

        let plan = parse_plan_from_reply(&reply).context("failed to parse code mode plan")?;
        if plan.calls.is_empty() {
            return Ok(None);
        }
        Ok(Some(plan))
    }
}

#[derive(Debug, Clone)]
pub struct CodeModePolicy {
    settings: AgentCodeModeSettings,
}

impl CodeModePolicy {
    pub fn new(settings: AgentCodeModeSettings) -> Self {
        Self { settings }
    }

    pub fn validate_plan(&self, plan: &CodeModePlan, tools: &[McpToolInfo]) -> anyhow::Result<()> {
        if self.settings.max_parallel == 0 {
            bail!("code mode max_parallel must be >= 1");
        }

        if plan.calls.is_empty() {
            bail!("code mode plan must include at least one tool call");
        }

        let max_calls = self.settings.max_calls.max(1);
        if plan.calls.len() > max_calls {
            bail!(
                "code mode plan exceeds max_calls: {} > {}",
                plan.calls.len(),
                max_calls
            );
        }

        let allowed = allowed_tool_set(tools);
        for call in &plan.calls {
            if call.server.trim().is_empty() || call.tool.trim().is_empty() {
                bail!("code mode plan contains empty server/tool name");
            }
            if !call.arguments.is_object() {
                bail!(
                    "code mode arguments must be object for {}::{}",
                    call.server,
                    call.tool
                );
            }
            let key = format!("{}::{}", call.server, call.tool);
            if !allowed.contains(&key) {
                bail!("code mode plan references unavailable tool: {key}");
            }
        }

        Ok(())
    }

    pub fn ensure_runtime_within_budget(&self, started_at: Instant) -> anyhow::Result<()> {
        let deadline = Duration::from_millis(self.settings.max_runtime_ms.max(1));
        if started_at.elapsed() > deadline {
            bail!("code mode execution exceeded runtime budget");
        }
        Ok(())
    }

    fn max_parallel(&self) -> usize {
        self.settings.max_parallel.max(1)
    }

    fn per_call_timeout(&self, started_at: Instant) -> Duration {
        let configured = Duration::from_millis(self.settings.max_call_timeout_ms.max(1));
        let total_budget = Duration::from_millis(self.settings.max_runtime_ms.max(1));
        let remaining = total_budget.saturating_sub(started_at.elapsed());
        let remaining = if remaining.is_zero() {
            Duration::from_millis(1)
        } else {
            remaining
        };
        configured.min(remaining)
    }

    fn truncate_result(&self, value: &Value, used_chars: &mut usize) -> (Value, bool) {
        let encoded = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
        let total_chars = encoded.chars().count();
        let max_chars = self.settings.max_result_chars;

        if *used_chars >= max_chars {
            return (
                serde_json::json!({
                    "truncated": "(code mode output budget exhausted)"
                }),
                true,
            );
        }

        let remain = max_chars.saturating_sub(*used_chars);
        if total_chars <= remain {
            *used_chars += total_chars;
            return (value.clone(), false);
        }

        let mut shortened = encoded.chars().take(remain).collect::<String>();
        shortened.push_str("...(truncated)");
        *used_chars = max_chars;
        (serde_json::json!({ "truncated": shortened }), true)
    }
}

#[derive(Debug, Clone)]
pub struct CodeModeExecutor {
    policy: CodeModePolicy,
}

impl CodeModeExecutor {
    pub fn new(settings: AgentCodeModeSettings) -> Self {
        Self {
            policy: CodeModePolicy::new(settings),
        }
    }

    pub async fn execute(
        &self,
        runtime: &McpRuntime,
        plan: &CodeModePlan,
        tools: &[McpToolInfo],
    ) -> anyhow::Result<CodeModeExecutionReport> {
        self.policy.validate_plan(plan, tools)?;

        let started_at = Instant::now();
        let max_parallel = self.policy.max_parallel();
        let mut collected: Vec<Option<(CodeModeToolCall, anyhow::Result<Value>, bool)>> =
            (0..plan.calls.len()).map(|_| None).collect();

        for chunk_start in (0..plan.calls.len()).step_by(max_parallel) {
            self.policy.ensure_runtime_within_budget(started_at)?;
            let chunk_end = (chunk_start + max_parallel).min(plan.calls.len());
            let chunk = &plan.calls[chunk_start..chunk_end];
            let per_call_timeout = self.policy.per_call_timeout(started_at);

            let mut jobs = JoinSet::new();
            for (offset, call) in chunk.iter().cloned().enumerate() {
                let idx = chunk_start + offset;
                let runtime = runtime.clone();
                jobs.spawn(async move {
                    let outcome = timeout(
                        per_call_timeout,
                        runtime.call_tool(&call.server, &call.tool, call.arguments.clone()),
                    )
                    .await;
                    match outcome {
                        Ok(result) => (idx, call, result, false),
                        Err(_) => (
                            idx,
                            call.clone(),
                            Err(anyhow!(
                                "code mode tool call timeout after {}ms: {}::{}",
                                per_call_timeout.as_millis(),
                                call.server,
                                call.tool
                            )),
                            true,
                        ),
                    }
                });
            }

            while let Some(joined) = jobs.join_next().await {
                let (idx, call, result, timed_out) =
                    joined.context("code mode execution task join failed")?;
                collected[idx] = Some((call, result, timed_out));
            }
        }

        let mut calls = Vec::with_capacity(plan.calls.len());
        let mut failed_calls = 0usize;
        let mut timed_out_calls = 0usize;
        let mut truncated_outputs = 0usize;
        let mut used_chars = 0usize;

        for item in collected {
            let (call, result, timed_out) =
                item.context("code mode internal execution ordering error")?;
            match result {
                Ok(value) => {
                    let (result_value, truncated) =
                        self.policy.truncate_result(&value, &mut used_chars);
                    if truncated {
                        truncated_outputs += 1;
                    }
                    calls.push(CodeModeCallResult {
                        server: call.server,
                        tool: call.tool,
                        ok: true,
                        result: Some(result_value),
                        error: None,
                        truncated,
                    });
                }
                Err(err) => {
                    failed_calls += 1;
                    if timed_out {
                        timed_out_calls += 1;
                    }
                    calls.push(CodeModeCallResult {
                        server: call.server,
                        tool: call.tool,
                        ok: false,
                        result: None,
                        error: Some(err.to_string()),
                        truncated: false,
                    });
                }
            }
        }

        Ok(CodeModeExecutionReport {
            calls,
            failed_calls,
            timed_out_calls,
            truncated_outputs,
            elapsed_ms: started_at.elapsed().as_millis(),
        })
    }
}

pub async fn execute_plan_via_subprocess(
    plan: &CodeModePlan,
    tools: &[McpToolInfo],
    settings: &AgentCodeModeSettings,
) -> anyhow::Result<CodeModeExecutionReport> {
    let executable =
        std::env::current_exe().context("failed to resolve current executable for code mode")?;
    let auth_token = generate_subprocess_auth_token();
    let allowed_tools = tools
        .iter()
        .map(|tool| CodeModeAllowedTool {
            server: tool.server.clone(),
            tool: tool.name.clone(),
        })
        .collect::<Vec<_>>();
    let payload = serde_json::to_vec(&CodeModeSandboxRequest {
        protocol_version: CODE_MODE_PROTOCOL_VERSION,
        auth_token: auth_token.clone(),
        plan: plan.clone(),
        allowed_tools,
        settings: settings.clone(),
    })
    .context("failed to encode code mode subprocess request")?;
    if payload.len() > CODE_MODE_SUBPROCESS_MAX_PAYLOAD_BYTES {
        bail!(
            "code mode subprocess request too large: {} bytes > {} bytes",
            payload.len(),
            CODE_MODE_SUBPROCESS_MAX_PAYLOAD_BYTES
        );
    }

    let mut child = Command::new(executable)
        .arg("__code-mode-exec")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env(CODE_MODE_SUBPROCESS_TOKEN_ENV, &auth_token)
        .spawn()
        .context("failed to spawn code mode subprocess executor")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("code mode subprocess missing stdin")?;
        stdin
            .write_all(&payload)
            .await
            .context("failed writing request to code mode subprocess")?;
        stdin
            .flush()
            .await
            .context("failed flushing request to code mode subprocess")?;
    }
    child.stdin.take();

    let output = timeout(
        Duration::from_secs(settings.subprocess_timeout_secs.max(1)),
        child.wait_with_output(),
    )
    .await
    .context("code mode subprocess timeout")?
    .context("failed waiting for code mode subprocess")?;

    let stdout =
        String::from_utf8(output.stdout).context("code mode subprocess stdout not utf-8")?;
    let stderr = String::from_utf8(output.stderr).unwrap_or_default();
    if !output.status.success() && stdout.trim().is_empty() {
        bail!(
            "code mode subprocess exited with status {}{}",
            output.status,
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(", stderr: {}", truncate_prompt_text(stderr.trim(), 800))
            }
        );
    }

    let response: CodeModeSandboxResponse = serde_json::from_str(stdout.trim())
        .with_context(|| format!("failed to decode code mode subprocess response: {stdout}"))?;
    if response.ok {
        return response
            .report
            .with_context(|| "code mode subprocess response missing report".to_string());
    }

    bail!(
        "code mode subprocess execution failed: {}",
        response
            .error
            .unwrap_or_else(|| "unknown subprocess error".to_string())
    )
}

pub async fn run_subprocess_exec_from_stdin() -> anyhow::Result<()> {
    let mut raw = Vec::new();
    std::io::stdin()
        .take((CODE_MODE_SUBPROCESS_MAX_PAYLOAD_BYTES + 1) as u64)
        .read_to_end(&mut raw)
        .context("failed to read code mode subprocess stdin")?;
    if raw.len() > CODE_MODE_SUBPROCESS_MAX_PAYLOAD_BYTES {
        bail!(
            "code mode subprocess payload too large: {} bytes > {} bytes",
            raw.len(),
            CODE_MODE_SUBPROCESS_MAX_PAYLOAD_BYTES
        );
    }
    let raw =
        String::from_utf8(raw).context("code mode subprocess stdin payload is not valid utf-8")?;

    let request: CodeModeSandboxRequest =
        serde_json::from_str(raw.trim()).context("failed to parse code mode subprocess request")?;
    let result = execute_subprocess_request(request).await;

    let response = match result {
        Ok(report) => CodeModeSandboxResponse {
            ok: true,
            report: Some(report),
            error: None,
        },
        Err(err) => CodeModeSandboxResponse {
            ok: false,
            report: None,
            error: Some(err.to_string()),
        },
    };

    let encoded =
        serde_json::to_vec(&response).context("failed to encode code mode subprocess response")?;
    let mut out = std::io::stdout();
    out.write_all(&encoded)
        .context("failed to write code mode subprocess response")?;
    out.flush()
        .context("failed to flush code mode subprocess response")?;
    Ok(())
}

async fn execute_subprocess_request(
    req: CodeModeSandboxRequest,
) -> anyhow::Result<CodeModeExecutionReport> {
    let expected_token = expected_subprocess_auth_token()?;
    validate_subprocess_request(&req, &expected_token)?;
    let policy_tools = materialize_policy_tools(&req.allowed_tools);

    let cwd = std::env::current_dir().context("failed to resolve cwd in code mode subprocess")?;
    let paths = McpConfigPaths::discover(&cwd)?;
    let registry = McpRegistry::new(paths);
    let runtime = McpRuntime::from_registry(&registry)
        .await
        .context("failed to build mcp runtime in code mode subprocess")?;
    let local_settings = AgentCodeModeSettings {
        execution_mode: CodeModeExecutionMode::Local,
        ..req.settings
    };
    let executor = CodeModeExecutor::new(local_settings);
    executor.execute(&runtime, &req.plan, &policy_tools).await
}

fn generate_subprocess_auth_token() -> String {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_nanos())
        .unwrap_or_default();
    let pid = std::process::id();
    let thread_id = format!("{:?}", std::thread::current().id());

    let mut hasher = Sha256::new();
    hasher.update(now_ns.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(thread_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn expected_subprocess_auth_token() -> anyhow::Result<String> {
    let token = std::env::var(CODE_MODE_SUBPROCESS_TOKEN_ENV).with_context(|| {
        format!("code mode subprocess auth token env missing: {CODE_MODE_SUBPROCESS_TOKEN_ENV}")
    })?;
    if token.trim().is_empty() {
        bail!("code mode subprocess auth token env is empty");
    }
    Ok(token)
}

fn validate_subprocess_request(
    req: &CodeModeSandboxRequest,
    expected_token: &str,
) -> anyhow::Result<()> {
    if req.protocol_version != CODE_MODE_PROTOCOL_VERSION {
        bail!(
            "unsupported code mode subprocess protocol version: {} (expected {})",
            req.protocol_version,
            CODE_MODE_PROTOCOL_VERSION
        );
    }
    if req.auth_token != expected_token {
        bail!("code mode subprocess auth token mismatch");
    }
    Ok(())
}

fn materialize_policy_tools(allowed_tools: &[CodeModeAllowedTool]) -> Vec<McpToolInfo> {
    allowed_tools
        .iter()
        .map(|tool| McpToolInfo {
            server: tool.server.clone(),
            name: tool.tool.clone(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        })
        .collect::<Vec<_>>()
}

fn allowed_tool_set(tools: &[McpToolInfo]) -> HashSet<String> {
    tools
        .iter()
        .map(|t| format!("{}::{}", t.server, t.name))
        .collect::<HashSet<_>>()
}

fn truncate_prompt_text(input: &str, max_chars: usize) -> String {
    let count = input.chars().count();
    if count <= max_chars {
        return input.to_string();
    }
    let mut out = input.chars().take(max_chars).collect::<String>();
    out.push_str("...(truncated)");
    out
}

fn extract_json_payload(text: &str) -> String {
    if text.starts_with("```") {
        let stripped = text.trim_start_matches("```");
        let stripped = stripped.strip_prefix("json").unwrap_or(stripped).trim();
        let stripped = stripped.strip_suffix("```").unwrap_or(stripped).trim();
        return stripped.to_string();
    }
    text.to_string()
}

fn parse_plan_from_reply(reply: &str) -> anyhow::Result<CodeModePlan> {
    let payload = extract_json_payload(reply.trim());
    let value: Value = serde_json::from_str(&payload)
        .with_context(|| format!("invalid code mode planner JSON: {payload}"))?;
    parse_plan_from_value(&value)
}

fn parse_plan_from_value(value: &Value) -> anyhow::Result<CodeModePlan> {
    if let Some(inner) = value.get("code_mode_plan") {
        return parse_plan_from_value(inner);
    }
    if let Some(inner) = value.get("plan") {
        return parse_plan_from_value(inner);
    }

    if let Some(items) = value.as_array() {
        let calls = parse_calls(items)?;
        return Ok(CodeModePlan { calls });
    }

    let obj = value
        .as_object()
        .with_context(|| "code mode planner JSON must be object or array")?;

    if let Some(calls) = obj.get("calls").and_then(|v| v.as_array()) {
        let calls = parse_calls(calls)?;
        return Ok(CodeModePlan { calls });
    }

    if let (Some(server), Some(tool)) = (
        obj.get("server").and_then(|v| v.as_str()),
        obj.get("tool").and_then(|v| v.as_str()),
    ) {
        let arguments = obj
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        return Ok(CodeModePlan {
            calls: vec![CodeModeToolCall {
                server: server.to_string(),
                tool: tool.to_string(),
                arguments,
            }],
        });
    }

    bail!("code mode planner JSON missing 'calls' field")
}

fn parse_calls(items: &[Value]) -> anyhow::Result<Vec<CodeModeToolCall>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let obj = item
            .as_object()
            .with_context(|| "code mode call item must be object")?;
        let server = obj
            .get("server")
            .and_then(|v| v.as_str())
            .with_context(|| "code mode call missing server")?;
        let tool = obj
            .get("tool")
            .and_then(|v| v.as_str())
            .with_context(|| "code mode call missing tool")?;
        let arguments = obj
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        out.push(CodeModeToolCall {
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::{
        AgentCodeModeSettings, CodeModeExecutionMode, CodeModeExecutionReport, CodeModeExecutor,
        CodeModePlan, CodeModePolicy, CodeModeSandboxRequest, CodeModeToolCall,
        DisabledCodeModePlanner, LlmCodeModePlanner, generate_subprocess_auth_token,
        validate_subprocess_request,
    };
    use crate::code_mode::CodeModePlanner;
    use crate::domain::{MessageRole, StoredMessage};
    use crate::mcp::{McpRuntime, McpServerConfig, McpToolInfo, McpTransport};
    use crate::provider::{ChatProvider, CompletionRequest};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::Value;

    #[test]
    fn default_code_mode_is_locked_down() {
        let settings = AgentCodeModeSettings::default();
        assert!(!settings.enabled);
        assert!(settings.shadow_mode);
        assert!(!settings.allow_network);
        assert!(!settings.allow_filesystem);
        assert!(!settings.allow_env);
        assert_eq!(settings.max_calls, 6);
        assert_eq!(settings.max_parallel, 2);
        assert_eq!(settings.max_call_timeout_ms, 1200);
        assert_eq!(settings.timeout_warn_ratio, 0.4);
        assert!(!settings.timeout_auto_shadow_enabled);
        assert_eq!(settings.timeout_auto_shadow_probe_every, 5);
        assert_eq!(settings.timeout_auto_shadow_streak, 3);
        assert_eq!(settings.execution_mode, CodeModeExecutionMode::Local);
        assert_eq!(settings.subprocess_timeout_secs, 8);
    }

    #[test]
    fn timeout_warn_ratio_is_normalized() {
        let settings = AgentCodeModeSettings {
            timeout_warn_ratio: 1.8,
            ..AgentCodeModeSettings::default()
        };
        assert_eq!(settings.normalized_timeout_warn_ratio(), 1.0);

        let settings = AgentCodeModeSettings {
            timeout_warn_ratio: -0.2,
            ..AgentCodeModeSettings::default()
        };
        assert_eq!(settings.normalized_timeout_warn_ratio(), 0.0);

        let settings = AgentCodeModeSettings {
            timeout_warn_ratio: f64::NAN,
            ..AgentCodeModeSettings::default()
        };
        assert_eq!(settings.normalized_timeout_warn_ratio(), 0.4);
    }

    #[test]
    fn policy_rejects_plan_above_call_budget() {
        let policy = CodeModePolicy::new(AgentCodeModeSettings {
            enabled: true,
            max_calls: 1,
            ..AgentCodeModeSettings::default()
        });
        let plan = CodeModePlan {
            calls: vec![
                CodeModeToolCall {
                    server: "s1".to_string(),
                    tool: "t1".to_string(),
                    arguments: serde_json::json!({}),
                },
                CodeModeToolCall {
                    server: "s2".to_string(),
                    tool: "t2".to_string(),
                    arguments: serde_json::json!({}),
                },
            ],
        };

        let tools = vec![tool("s1", "t1"), tool("s2", "t2")];
        let err = policy
            .validate_plan(&plan, &tools)
            .expect_err("budget should fail");
        assert!(err.to_string().contains("max_calls"));
    }

    #[test]
    fn policy_rejects_empty_plan() {
        let policy = CodeModePolicy::new(AgentCodeModeSettings::default());
        let err = policy
            .validate_plan(&CodeModePlan { calls: vec![] }, &[tool("s1", "t1")])
            .expect_err("empty plan should fail");
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn policy_rejects_unknown_tool() {
        let policy = CodeModePolicy::new(AgentCodeModeSettings::default());
        let plan = CodeModePlan {
            calls: vec![CodeModeToolCall {
                server: "s2".to_string(),
                tool: "unknown".to_string(),
                arguments: serde_json::json!({}),
            }],
        };
        let err = policy
            .validate_plan(&plan, &[tool("s1", "t1")])
            .expect_err("unknown tool should fail");
        assert!(err.to_string().contains("unavailable tool"));
    }

    #[test]
    fn per_call_timeout_is_capped_by_remaining_runtime() {
        let policy = CodeModePolicy::new(AgentCodeModeSettings {
            max_runtime_ms: 2500,
            max_call_timeout_ms: 1200,
            ..AgentCodeModeSettings::default()
        });

        let timeout_at_start = policy.per_call_timeout(Instant::now());
        assert_eq!(timeout_at_start, Duration::from_millis(1200));

        let near_deadline = Instant::now()
            .checked_sub(Duration::from_millis(2480))
            .expect("checked_sub should succeed");
        let timeout_near_deadline = policy.per_call_timeout(near_deadline);
        assert!(timeout_near_deadline <= Duration::from_millis(20));
        assert!(timeout_near_deadline >= Duration::from_millis(1));

        let past_deadline = Instant::now()
            .checked_sub(Duration::from_millis(3000))
            .expect("checked_sub should succeed");
        let timeout_past_deadline = policy.per_call_timeout(past_deadline);
        assert_eq!(timeout_past_deadline, Duration::from_millis(1));
    }

    #[tokio::test]
    async fn disabled_planner_returns_none() {
        let planner = DisabledCodeModePlanner;
        let out = planner
            .build_plan(&[], &[])
            .await
            .expect("planner should succeed");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn llm_planner_parses_fenced_json_plan() {
        let provider = Arc::new(FakePlannerProvider {
            reply: "```json\n{\"calls\":[{\"server\":\"search\",\"tool\":\"web\",\"arguments\":{\"q\":\"rust\"}}]}\n```".to_string(),
        });
        let planner = LlmCodeModePlanner::new(provider).with_limits(4, 4);

        let history = vec![StoredMessage {
            role: MessageRole::User,
            content: "搜一下 rust mcp".to_string(),
        }];
        let tools = vec![tool("search", "web")];

        let plan = planner
            .build_plan(&history, &tools)
            .await
            .expect("planner ok")
            .expect("plan exists");

        assert_eq!(plan.calls.len(), 1);
        assert_eq!(plan.calls[0].server, "search");
        assert_eq!(plan.calls[0].tool, "web");
        assert_eq!(plan.calls[0].arguments["q"], "rust");
    }

    #[test]
    fn subprocess_token_is_sha256_hex() {
        let token = generate_subprocess_auth_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn subprocess_request_validation_checks_protocol_and_token() {
        let req = CodeModeSandboxRequest {
            protocol_version: 999,
            auth_token: "bad".to_string(),
            plan: CodeModePlan { calls: vec![] },
            allowed_tools: vec![],
            settings: AgentCodeModeSettings::default(),
        };
        let err = validate_subprocess_request(&req, "expected")
            .expect_err("protocol mismatch should fail");
        assert!(err.to_string().contains("protocol version"));

        let req = CodeModeSandboxRequest {
            protocol_version: 1,
            auth_token: "bad".to_string(),
            plan: CodeModePlan { calls: vec![] },
            allowed_tools: vec![],
            settings: AgentCodeModeSettings::default(),
        };
        let err =
            validate_subprocess_request(&req, "expected").expect_err("token mismatch should fail");
        assert!(err.to_string().contains("token mismatch"));
    }

    #[derive(Default)]
    struct SlowHttpMcpState {
        initialize_calls: AtomicUsize,
        tools_call_calls: AtomicUsize,
    }

    async fn slow_http_mcp_handler(
        State(state): State<Arc<SlowHttpMcpState>>,
        headers: HeaderMap,
        Json(payload): Json<Value>,
    ) -> impl IntoResponse {
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        match method {
            "initialize" => {
                state.initialize_calls.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [("mcp-session-id", "slow-session")],
                    Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": payload.get("id").cloned().unwrap_or_else(|| serde_json::json!(1)),
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "serverInfo": { "name": "slow-mock", "version": "1.0.0" }
                        }
                    })),
                )
                    .into_response()
            }
            "notifications/initialized" => (
                StatusCode::OK,
                Json(serde_json::json!({ "jsonrpc": "2.0", "result": {} })),
            )
                .into_response(),
            "tools/call" => {
                if headers.get("mcp-session-id").is_none() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": "missing mcp-session-id" })),
                    )
                        .into_response();
                }
                state.tools_call_calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(120)).await;
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": payload.get("id").cloned().unwrap_or_else(|| serde_json::json!(2)),
                        "result": { "ok": true }
                    })),
                )
                    .into_response()
            }
            _ => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("unsupported method: {method}") })),
            )
                .into_response(),
        }
    }

    #[tokio::test]
    async fn executor_counts_timed_out_calls_for_slow_mcp_tool() {
        let state = Arc::new(SlowHttpMcpState::default());
        let app = Router::new()
            .route("/mcp", post(slow_http_mcp_handler))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let mut servers = HashMap::new();
        servers.insert(
            "slow".to_string(),
            McpServerConfig {
                enabled: true,
                transport: McpTransport::Http,
                command: None,
                args: vec![],
                cwd: None,
                env: HashMap::new(),
                url: Some(format!("http://{addr}/mcp")),
                headers: HashMap::new(),
                timeout_secs: 5,
            },
        );
        let runtime = McpRuntime::new(servers);
        let executor = CodeModeExecutor::new(AgentCodeModeSettings {
            enabled: true,
            shadow_mode: false,
            max_calls: 1,
            max_parallel: 1,
            max_runtime_ms: 1500,
            max_call_timeout_ms: 30,
            timeout_warn_ratio: 0.4,
            timeout_auto_shadow_enabled: false,
            timeout_auto_shadow_probe_every: 5,
            timeout_auto_shadow_streak: 3,
            max_result_chars: 12000,
            execution_mode: CodeModeExecutionMode::Local,
            subprocess_timeout_secs: 8,
            allow_network: false,
            allow_filesystem: false,
            allow_env: false,
        });

        let plan = CodeModePlan {
            calls: vec![CodeModeToolCall {
                server: "slow".to_string(),
                tool: "sleepy".to_string(),
                arguments: serde_json::json!({}),
            }],
        };
        let tools = vec![tool("slow", "sleepy")];

        let report: CodeModeExecutionReport = executor
            .execute(&runtime, &plan, &tools)
            .await
            .expect("executor should return report");
        assert_eq!(report.calls.len(), 1);
        assert_eq!(report.failed_calls, 1);
        assert_eq!(report.timed_out_calls, 1);
        assert!(!report.calls[0].ok);
        assert!(
            report.calls[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("timeout")
        );
        assert_eq!(state.initialize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.tools_call_calls.load(Ordering::SeqCst), 1);

        server.abort();
        let _ = server.await;
    }

    fn tool(server: &str, name: &str) -> McpToolInfo {
        McpToolInfo {
            server: server.to_string(),
            name: name.to_string(),
            description: Some("demo".to_string()),
            input_schema: serde_json::json!({ "type": "object" }),
        }
    }

    struct FakePlannerProvider {
        reply: String,
    }

    #[async_trait::async_trait]
    impl ChatProvider for FakePlannerProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            Ok(self.reply.clone())
        }
    }
}
