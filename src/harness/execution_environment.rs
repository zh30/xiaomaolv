use async_trait::async_trait;

use crate::code_mode::{
    AgentCodeModeSettings, CodeModeExecutionReport, CodeModeExecutor, CodeModePlan,
    execute_plan_via_subprocess,
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
