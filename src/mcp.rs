use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, timeout};

const ENV_USER_CONFIG_OVERRIDE: &str = "XIAOMAOLV_MCP_USER_CONFIG";
const ENV_PROJECT_CONFIG_OVERRIDE: &str = "XIAOMAOLV_MCP_PROJECT_CONFIG";
const DEFAULT_TIMEOUT_SECS: u64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(default)]
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl McpServerConfig {
    pub fn validate(&self, name: &str) -> anyhow::Result<()> {
        if self.enabled {
            match self.transport {
                McpTransport::Stdio => {
                    if self.command.as_ref().is_none_or(|v| v.trim().is_empty()) {
                        bail!("mcp server '{name}' requires command for stdio transport");
                    }
                }
                McpTransport::Http => {
                    if self.url.as_ref().is_none_or(|v| v.trim().is_empty()) {
                        bail!("mcp server '{name}' requires url for http transport");
                    }
                }
            }
        }
        Ok(())
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            transport: McpTransport::Stdio,
            command: None,
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfigFile {
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpListScope {
    User,
    Project,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpRemoveScope {
    User,
    Project,
    All,
}

#[derive(Debug, Clone)]
pub struct McpConfigPaths {
    pub user: PathBuf,
    pub project: PathBuf,
}

impl McpConfigPaths {
    pub fn discover(cwd: &Path) -> anyhow::Result<Self> {
        let user = if let Ok(override_path) = std::env::var(ENV_USER_CONFIG_OVERRIDE) {
            PathBuf::from(override_path)
        } else {
            let base = dirs::config_dir().context("cannot resolve user config directory")?;
            base.join("xiaomaolv").join("mcp.toml")
        };

        let project = if let Ok(override_path) = std::env::var(ENV_PROJECT_CONFIG_OVERRIDE) {
            PathBuf::from(override_path)
        } else {
            cwd.join(".xiaomaolv").join("mcp.toml")
        };

        Ok(Self { user, project })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerView {
    pub name: String,
    pub source: String,
    pub enabled: bool,
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAddSpec {
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    pub headers: HashMap<String, String>,
    pub timeout_secs: u64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpProbeResult {
    pub server: String,
    pub transport: String,
    pub enabled: bool,
    pub elapsed_ms: u128,
    pub tools: Vec<McpToolInfo>,
}

pub struct McpRegistry {
    paths: McpConfigPaths,
}

impl McpRegistry {
    pub fn new(paths: McpConfigPaths) -> Self {
        Self { paths }
    }

    pub async fn add_server(
        &self,
        scope: McpScope,
        name: &str,
        spec: McpAddSpec,
    ) -> anyhow::Result<()> {
        let mut file = self.load_scope(scope).await?;
        let server = McpServerConfig {
            enabled: spec.enabled,
            transport: spec.transport,
            command: spec.command,
            args: spec.args,
            cwd: spec.cwd,
            env: spec.env,
            url: spec.url,
            headers: spec.headers,
            timeout_secs: spec.timeout_secs.max(1),
        };
        server.validate(name)?;
        file.servers.insert(name.to_string(), server);
        self.save_scope(scope, &file).await
    }

    pub async fn remove_server(&self, scope: McpRemoveScope, name: &str) -> anyhow::Result<bool> {
        match scope {
            McpRemoveScope::User => self.remove_from_scope(McpScope::User, name).await,
            McpRemoveScope::Project => self.remove_from_scope(McpScope::Project, name).await,
            McpRemoveScope::All => {
                let a = self.remove_from_scope(McpScope::User, name).await?;
                let b = self.remove_from_scope(McpScope::Project, name).await?;
                Ok(a || b)
            }
        }
    }

    pub async fn list_servers(&self, scope: McpListScope) -> anyhow::Result<Vec<McpServerView>> {
        match scope {
            McpListScope::User => {
                let file = self.load_scope(McpScope::User).await?;
                Ok(file
                    .servers
                    .into_iter()
                    .map(|(name, cfg)| server_view(name, "user", cfg))
                    .collect())
            }
            McpListScope::Project => {
                let file = self.load_scope(McpScope::Project).await?;
                Ok(file
                    .servers
                    .into_iter()
                    .map(|(name, cfg)| server_view(name, "project", cfg))
                    .collect())
            }
            McpListScope::Merged => {
                let user = self.load_scope(McpScope::User).await?;
                let project = self.load_scope(McpScope::Project).await?;

                let mut merged: HashMap<String, (String, McpServerConfig)> = user
                    .servers
                    .into_iter()
                    .map(|(k, v)| (k, ("user".to_string(), v)))
                    .collect();

                for (k, v) in project.servers {
                    merged.insert(k, ("project".to_string(), v));
                }

                Ok(merged
                    .into_iter()
                    .map(|(name, (source, cfg))| server_view(name, &source, cfg))
                    .collect())
            }
        }
    }

    pub async fn load_merged(&self) -> anyhow::Result<HashMap<String, McpServerConfig>> {
        let mut merged = self.load_scope(McpScope::User).await?.servers;
        for (name, cfg) in self.load_scope(McpScope::Project).await?.servers {
            merged.insert(name, cfg);
        }
        Ok(merged)
    }

    pub async fn test_server(&self, name: &str) -> anyhow::Result<McpProbeResult> {
        let merged = self.load_merged().await?;
        let cfg = merged
            .get(name)
            .cloned()
            .with_context(|| format!("mcp server '{name}' is not configured"))?;

        if !cfg.enabled {
            bail!("mcp server '{name}' is disabled");
        }

        let runtime = McpRuntime::new(merged);
        runtime.test_server(name, &cfg).await
    }

    async fn remove_from_scope(&self, scope: McpScope, name: &str) -> anyhow::Result<bool> {
        let mut file = self.load_scope(scope).await?;
        let existed = file.servers.remove(name).is_some();
        if existed {
            self.save_scope(scope, &file).await?;
        }
        Ok(existed)
    }

    async fn load_scope(&self, scope: McpScope) -> anyhow::Result<McpConfigFile> {
        let path = self.path_for(scope);
        if !path.exists() {
            return Ok(McpConfigFile::default());
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read mcp config file: {}", path.display()))?;
        let cfg: McpConfigFile = toml::from_str(&content)
            .with_context(|| format!("failed to parse mcp config file: {}", path.display()))?;
        for (name, server) in &cfg.servers {
            server.validate(name)?;
        }
        Ok(cfg)
    }

    async fn save_scope(&self, scope: McpScope, file: &McpConfigFile) -> anyhow::Result<()> {
        let path = self.path_for(scope);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create mcp config dir: {}", parent.display())
            })?;
        }
        let content = toml::to_string_pretty(file).context("failed to serialize mcp config")?;
        tokio::fs::write(&path, content)
            .await
            .with_context(|| format!("failed to write mcp config file: {}", path.display()))?;
        Ok(())
    }

    fn path_for(&self, scope: McpScope) -> PathBuf {
        match scope {
            McpScope::User => self.paths.user.clone(),
            McpScope::Project => self.paths.project.clone(),
        }
    }
}

fn server_view(name: String, source: &str, cfg: McpServerConfig) -> McpServerView {
    McpServerView {
        name,
        source: source.to_string(),
        enabled: cfg.enabled,
        transport: match cfg.transport {
            McpTransport::Stdio => "stdio".to_string(),
            McpTransport::Http => "http".to_string(),
        },
        command: cfg.command,
        args: cfg.args,
        cwd: cfg.cwd,
        url: cfg.url,
        timeout_secs: cfg.timeout_secs,
    }
}

#[derive(Clone)]
pub struct McpRuntime {
    servers: HashMap<String, McpServerConfig>,
    http_client: reqwest::Client,
    session_pool: Arc<McpSessionPool>,
    tool_cache: Arc<RwLock<HashMap<String, Vec<McpToolInfo>>>>,
}

#[derive(Default)]
struct McpSessionPool {
    stdio: Mutex<HashMap<String, Arc<Mutex<StdioSession>>>>,
    http: Mutex<HashMap<String, String>>,
}

struct StdioSession {
    child: tokio::process::Child,
    writer: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl Drop for StdioSession {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl Default for McpRuntime {
    fn default() -> Self {
        Self::new(HashMap::new())
    }
}

impl McpSessionPool {
    async fn get_or_create_stdio(
        &self,
        server_name: &str,
        cfg: &McpServerConfig,
    ) -> anyhow::Result<Arc<Mutex<StdioSession>>> {
        let mut sessions = self.stdio.lock().await;
        if let Some(session) = sessions.get(server_name) {
            return Ok(session.clone());
        }
        let created = Arc::new(Mutex::new(
            StdioSession::connect(server_name, cfg)
                .await
                .with_context(|| {
                    format!("failed to initialize stdio session for '{server_name}'")
                })?,
        ));
        sessions.insert(server_name.to_string(), created.clone());
        Ok(created)
    }

    async fn drop_stdio(&self, server_name: &str) {
        let mut sessions = self.stdio.lock().await;
        sessions.remove(server_name);
    }

    async fn get_http_session_id(&self, server_name: &str) -> Option<String> {
        self.http.lock().await.get(server_name).cloned()
    }

    async fn set_http_session_id(&self, server_name: &str, session_id: Option<String>) {
        let mut sessions = self.http.lock().await;
        if let Some(session) = session_id {
            sessions.insert(server_name.to_string(), session);
        } else {
            sessions.remove(server_name);
        }
    }
}

impl StdioSession {
    async fn connect(server_name: &str, cfg: &McpServerConfig) -> anyhow::Result<Self> {
        let command = cfg
            .command
            .as_deref()
            .with_context(|| format!("mcp server '{server_name}' command missing"))?;

        let mut cmd = Command::new(command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        if let Some(cwd) = cfg.cwd.as_deref() {
            cmd.current_dir(cwd);
        }

        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().with_context(|| {
            format!("failed to spawn stdio mcp server '{server_name}' using '{command}'")
        })?;

        let stdin = child
            .stdin
            .take()
            .with_context(|| format!("mcp server '{server_name}' missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("mcp server '{server_name}' missing stdout"))?;
        let mut session = Self {
            child,
            writer: stdin,
            reader: BufReader::new(stdout),
            next_id: 2,
        };
        session
            .initialize(server_name, cfg.timeout_secs.max(1))
            .await
            .with_context(|| format!("mcp server '{server_name}' initialize failed"))?;
        Ok(session)
    }

    async fn initialize(&mut self, server_name: &str, timeout_secs: u64) -> anyhow::Result<()> {
        write_json_line(
            &mut self.writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "xiaomaolv",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
        )
        .await?;

        let init_response =
            read_jsonrpc_message_with_id(&mut self.reader, Duration::from_secs(timeout_secs), 1)
                .await
                .with_context(|| format!("mcp server '{server_name}' initialize read failed"))?;
        if init_response.get("error").is_some() {
            bail!("mcp server '{server_name}' initialize returned error: {init_response}");
        }

        write_json_line(
            &mut self.writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }),
        )
        .await?;

        Ok(())
    }

    fn allocate_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1).max(2);
        id
    }
}

impl McpRuntime {
    pub fn new(servers: HashMap<String, McpServerConfig>) -> Self {
        Self {
            servers,
            http_client: reqwest::Client::new(),
            session_pool: Arc::new(McpSessionPool::default()),
            tool_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn from_registry(registry: &McpRegistry) -> anyhow::Result<Self> {
        Ok(Self::new(registry.load_merged().await?))
    }

    pub fn servers(&self) -> Vec<McpServerView> {
        self.servers
            .iter()
            .map(|(name, cfg)| server_view(name.clone(), "merged", cfg.clone()))
            .collect()
    }

    pub async fn list_tools(
        &self,
        server_filter: Option<&str>,
    ) -> anyhow::Result<Vec<McpToolInfo>> {
        let mut out = Vec::new();

        for (name, cfg) in &self.servers {
            if !cfg.enabled {
                continue;
            }
            if let Some(filter) = server_filter
                && filter != name
            {
                continue;
            }
            let tools = self.list_server_tools(name, cfg).await?;
            out.extend(tools);
        }

        Ok(out)
    }

    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: Value,
    ) -> anyhow::Result<Value> {
        let cfg = self
            .servers
            .get(server_name)
            .with_context(|| format!("mcp server '{server_name}' not found"))?;

        if !cfg.enabled {
            bail!("mcp server '{server_name}' is disabled");
        }

        let response = self
            .request_server(
                server_name,
                cfg,
                "tools/call",
                serde_json::json!({
                    "name": tool_name,
                    "arguments": args
                }),
            )
            .await?;

        Ok(response
            .get("result")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})))
    }

    pub async fn test_server(
        &self,
        name: &str,
        cfg: &McpServerConfig,
    ) -> anyhow::Result<McpProbeResult> {
        let started = Instant::now();
        let tools = self.list_server_tools(name, cfg).await?;
        Ok(McpProbeResult {
            server: name.to_string(),
            transport: match cfg.transport {
                McpTransport::Stdio => "stdio".to_string(),
                McpTransport::Http => "http".to_string(),
            },
            enabled: cfg.enabled,
            elapsed_ms: started.elapsed().as_millis(),
            tools,
        })
    }

    async fn list_server_tools(
        &self,
        server_name: &str,
        cfg: &McpServerConfig,
    ) -> anyhow::Result<Vec<McpToolInfo>> {
        if let Some(cached) = self.tool_cache.read().await.get(server_name).cloned() {
            return Ok(cached);
        }

        let response = self
            .request_server(server_name, cfg, "tools/list", serde_json::json!({}))
            .await?;

        let tools = response
            .get("result")
            .and_then(|result| result.get("tools"))
            .and_then(|tools| tools.as_array())
            .cloned()
            .unwrap_or_default();

        let parsed = tools
            .into_iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?.to_string();
                let description = tool
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let input_schema = tool.get("inputSchema").cloned().unwrap_or(Value::Null);
                Some(McpToolInfo {
                    server: server_name.to_string(),
                    name,
                    description,
                    input_schema,
                })
            })
            .collect::<Vec<_>>();
        self.tool_cache
            .write()
            .await
            .insert(server_name.to_string(), parsed.clone());
        Ok(parsed)
    }

    async fn request_server(
        &self,
        server_name: &str,
        cfg: &McpServerConfig,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        match cfg.transport {
            McpTransport::Stdio => self.request_stdio(server_name, cfg, method, params).await,
            McpTransport::Http => self.request_http(server_name, cfg, method, params).await,
        }
    }

    async fn request_stdio(
        &self,
        server_name: &str,
        cfg: &McpServerConfig,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        let timeout_secs = cfg.timeout_secs.max(1);
        let send = || async {
            let session = self
                .session_pool
                .get_or_create_stdio(server_name, cfg)
                .await?;
            let mut guard = session.lock().await;
            let request_id = guard.allocate_request_id();

            write_json_line(
                &mut guard.writer,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params.clone()
                }),
            )
            .await?;

            read_jsonrpc_message_with_id(
                &mut guard.reader,
                Duration::from_secs(timeout_secs),
                request_id,
            )
            .await
            .with_context(|| format!("mcp server '{server_name}' read failed"))
        };

        match send().await {
            Ok(response) => Ok(response),
            Err(first_err) => {
                self.session_pool.drop_stdio(server_name).await;
                if method == "tools/list" {
                    return send().await.with_context(|| {
                        format!("mcp server '{server_name}' retry after session reset failed")
                    });
                }
                Err(first_err).with_context(|| {
                    format!(
                        "mcp server '{server_name}' request failed; session reset, no auto-retry for non-idempotent method '{method}'"
                    )
                })
            }
        }
    }

    async fn request_http(
        &self,
        server_name: &str,
        cfg: &McpServerConfig,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        let url = cfg
            .url
            .as_deref()
            .with_context(|| format!("mcp server '{server_name}' url missing"))?;
        let timeout_secs = cfg.timeout_secs.max(1);
        let mut session_id = self.session_pool.get_http_session_id(server_name).await;
        if session_id.is_none() {
            self.initialize_http_session(url, cfg, &mut session_id, timeout_secs)
                .await
                .with_context(|| format!("mcp server '{server_name}' initialize failed"))?;
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": method,
            "params": params
        });
        let first = self
            .http_jsonrpc(url, cfg, &request, &mut session_id, timeout_secs)
            .await;

        match first {
            Ok(value) => {
                self.session_pool
                    .set_http_session_id(server_name, session_id)
                    .await;
                Ok(value)
            }
            Err(first_err) => {
                session_id = None;
                self.session_pool
                    .set_http_session_id(server_name, None)
                    .await;
                self.initialize_http_session(url, cfg, &mut session_id, timeout_secs)
                    .await
                    .with_context(|| format!("mcp server '{server_name}' reinitialize failed"))?;
                let retry = self
                    .http_jsonrpc(url, cfg, &request, &mut session_id, timeout_secs)
                    .await
                    .with_context(|| {
                        format!("mcp server '{server_name}' request failed after reinitialize")
                    });
                match retry {
                    Ok(value) => {
                        self.session_pool
                            .set_http_session_id(server_name, session_id)
                            .await;
                        Ok(value)
                    }
                    Err(retry_err) => Err(anyhow::anyhow!(
                        "mcp server '{server_name}' request failed before and after reinitialize; first error: {first_err}; retry error: {retry_err}"
                    )),
                }
            }
        }
    }

    async fn initialize_http_session(
        &self,
        url: &str,
        cfg: &McpServerConfig,
        session_id: &mut Option<String>,
        timeout_secs: u64,
    ) -> anyhow::Result<()> {
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "xiaomaolv",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });
        let init = self
            .http_jsonrpc(url, cfg, &initialize, session_id, timeout_secs)
            .await?;
        if init.get("error").is_some() {
            bail!("initialize returned error: {init}");
        }

        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let _ = self
            .http_jsonrpc(url, cfg, &initialized, session_id, timeout_secs)
            .await?;
        Ok(())
    }

    async fn http_jsonrpc(
        &self,
        url: &str,
        cfg: &McpServerConfig,
        payload: &Value,
        session_id: &mut Option<String>,
        timeout_secs: u64,
    ) -> anyhow::Result<Value> {
        let mut request = self
            .http_client
            .post(url)
            .timeout(Duration::from_secs(timeout_secs))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(payload);

        if let Some(session) = session_id {
            request = request.header("Mcp-Session-Id", session.as_str());
        }

        let headers = build_headers(&cfg.headers)?;
        for (name, value) in &headers {
            request = request.header(name, value);
        }

        let response = request.send().await.context("mcp http request failed")?;
        let status = response.status();
        let headers = response.headers().clone();
        let content_type = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response.text().await.context("mcp http read body failed")?;

        if !status.is_success() {
            bail!("mcp http status {status}: {body}");
        }

        if let Some(v) = headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .or_else(|| headers.get("Mcp-Session-Id").and_then(|v| v.to_str().ok()))
        {
            *session_id = Some(v.to_string());
        }

        if content_type.contains("application/json") {
            let value: Value =
                serde_json::from_str(&body).context("mcp http json decode failed")?;
            return Ok(value);
        }

        if content_type.contains("text/event-stream") {
            return parse_sse_jsonrpc(&body);
        }

        bail!("unsupported mcp http content type: {content_type}");
    }
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

async fn write_json_line(
    writer: &mut tokio::process::ChildStdin,
    value: &Value,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(value).context("failed to encode jsonrpc message")?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .context("failed to write jsonrpc message")?;
    writer
        .flush()
        .await
        .context("failed to flush jsonrpc message")?;
    Ok(())
}

async fn read_jsonrpc_message(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    dur: Duration,
) -> anyhow::Result<Value> {
    timeout(dur, async {
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .await
                .context("failed reading stdio line")?;
            if read == 0 {
                bail!("mcp stdio closed");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(len) = parse_content_length(trimmed) {
                let mut blank = String::new();
                let _ = reader.read_line(&mut blank).await?;
                let mut buf = vec![0u8; len];
                reader
                    .read_exact(&mut buf)
                    .await
                    .context("failed reading content-length body")?;
                let value: Value =
                    serde_json::from_slice(&buf).context("invalid content-length jsonrpc body")?;
                return Ok(value);
            }

            let value: Value =
                serde_json::from_str(trimmed).context("invalid newline jsonrpc body")?;
            return Ok(value);
        }
    })
    .await
    .context("mcp stdio read timeout")?
}

async fn read_jsonrpc_message_with_id(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    dur: Duration,
    request_id: u64,
) -> anyhow::Result<Value> {
    loop {
        let msg = read_jsonrpc_message(reader, dur).await?;
        let id = msg.get("id");
        if id.is_none() {
            continue;
        }
        if id == Some(&serde_json::json!(request_id)) {
            return Ok(msg);
        }
    }
}

fn parse_content_length(line: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    if !lower.starts_with("content-length:") {
        return None;
    }
    line.split(':')
        .nth(1)
        .and_then(|v| v.trim().parse::<usize>().ok())
}

fn build_headers(headers: &HashMap<String, String>) -> anyhow::Result<HeaderMap> {
    let mut out = HeaderMap::new();
    for (k, v) in headers {
        let key = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid header name '{k}'"))?;
        let val =
            HeaderValue::from_str(v).with_context(|| format!("invalid header value '{k}'"))?;
        out.insert(key, val);
    }
    Ok(out)
}

fn parse_sse_jsonrpc(body: &str) -> anyhow::Result<Value> {
    let mut data_lines = Vec::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_string());
        }
    }
    if data_lines.is_empty() {
        bail!("empty sse payload");
    }

    for raw in data_lines {
        if raw.is_empty() || raw == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            return Ok(value);
        }
    }

    bail!("unable to parse sse jsonrpc payload");
}

pub fn parse_kv(input: &str) -> anyhow::Result<(String, String)> {
    let (k, v) = input
        .split_once('=')
        .with_context(|| format!("invalid key-value '{input}', expected KEY=VALUE"))?;
    if k.trim().is_empty() {
        bail!("invalid key-value '{input}', key is empty");
    }
    Ok((k.trim().to_string(), v.to_string()))
}

pub fn parse_header_kv(input: &str) -> anyhow::Result<(String, String)> {
    if let Some((k, v)) = input.split_once(':') {
        if k.trim().is_empty() {
            bail!("invalid header '{input}', key is empty");
        }
        return Ok((k.trim().to_string(), v.trim().to_string()));
    }
    parse_kv(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use tempfile::tempdir;

    #[derive(Default)]
    struct MockHttpMcpState {
        initialize_calls: AtomicUsize,
        tools_list_calls: AtomicUsize,
        tools_call_calls: AtomicUsize,
    }

    async fn mock_http_mcp_handler(
        State(state): State<Arc<MockHttpMcpState>>,
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
                    [("mcp-session-id", "session-1")],
                    Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": payload.get("id").cloned().unwrap_or_else(|| serde_json::json!(1)),
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "serverInfo": { "name": "mock", "version": "1.0.0" }
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
            "tools/list" => {
                if headers.get("mcp-session-id").is_none() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": "missing mcp-session-id" })),
                    )
                        .into_response();
                }
                state.tools_list_calls.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": payload.get("id").cloned().unwrap_or_else(|| serde_json::json!(2)),
                        "result": {
                            "tools": [
                                {
                                    "name": "echo",
                                    "description": "echo tool",
                                    "inputSchema": { "type": "object" }
                                }
                            ]
                        }
                    })),
                )
                    .into_response()
            }
            "tools/call" => {
                if headers.get("mcp-session-id").is_none() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": "missing mcp-session-id" })),
                    )
                        .into_response();
                }
                state.tools_call_calls.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": payload.get("id").cloned().unwrap_or_else(|| serde_json::json!(2)),
                        "result": {
                            "echoed": payload.get("params").cloned().unwrap_or_else(|| serde_json::json!({}))
                        }
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
    async fn registry_merge_uses_project_precedence() {
        let tmp = tempdir().expect("tempdir");
        let paths = McpConfigPaths {
            user: tmp.path().join("user.toml"),
            project: tmp.path().join("project.toml"),
        };
        let registry = McpRegistry::new(paths);

        registry
            .add_server(
                McpScope::User,
                "search",
                McpAddSpec {
                    transport: McpTransport::Http,
                    command: None,
                    args: vec![],
                    cwd: None,
                    env: HashMap::new(),
                    url: Some("https://user.example/mcp".to_string()),
                    headers: HashMap::new(),
                    timeout_secs: 10,
                    enabled: true,
                },
            )
            .await
            .expect("add user");

        registry
            .add_server(
                McpScope::Project,
                "search",
                McpAddSpec {
                    transport: McpTransport::Http,
                    command: None,
                    args: vec![],
                    cwd: None,
                    env: HashMap::new(),
                    url: Some("https://project.example/mcp".to_string()),
                    headers: HashMap::new(),
                    timeout_secs: 20,
                    enabled: true,
                },
            )
            .await
            .expect("add project");

        let merged = registry.load_merged().await.expect("merged");
        let cfg = merged.get("search").expect("search");
        assert_eq!(
            cfg.url.as_deref(),
            Some("https://project.example/mcp"),
            "project config should override user config on same name"
        );
        assert_eq!(cfg.timeout_secs, 20);
    }

    #[tokio::test]
    async fn registry_remove_all_scope() {
        let tmp = tempdir().expect("tempdir");
        let paths = McpConfigPaths {
            user: tmp.path().join("user.toml"),
            project: tmp.path().join("project.toml"),
        };
        let registry = McpRegistry::new(paths);

        let spec = McpAddSpec {
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            cwd: None,
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            timeout_secs: 10,
            enabled: true,
        };

        registry
            .add_server(McpScope::User, "demo", spec.clone())
            .await
            .expect("add user");
        registry
            .add_server(McpScope::Project, "demo", spec)
            .await
            .expect("add project");

        let removed = registry
            .remove_server(McpRemoveScope::All, "demo")
            .await
            .expect("remove all");
        assert!(removed);
        let merged = registry.load_merged().await.expect("merged");
        assert!(!merged.contains_key("demo"));
    }

    #[test]
    fn parse_kv_helpers() {
        let (a, b) = parse_kv("KEY=VALUE").expect("parse kv");
        assert_eq!(a, "KEY");
        assert_eq!(b, "VALUE");

        let (h, v) = parse_header_kv("Authorization: Bearer token").expect("parse header");
        assert_eq!(h, "Authorization");
        assert_eq!(v, "Bearer token");
    }

    #[tokio::test]
    async fn http_runtime_reuses_session_and_caches_tools() {
        let state = Arc::new(MockHttpMcpState::default());
        let app = Router::new()
            .route("/mcp", post(mock_http_mcp_handler))
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
            "demo".to_string(),
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

        let tools_first = runtime.list_tools(Some("demo")).await.expect("tools first");
        let tools_second = runtime
            .list_tools(Some("demo"))
            .await
            .expect("tools second");
        assert_eq!(tools_first.len(), 1);
        assert_eq!(tools_second.len(), 1);

        let _ = runtime
            .call_tool("demo", "echo", serde_json::json!({ "q": 1 }))
            .await
            .expect("call first");
        let _ = runtime
            .call_tool("demo", "echo", serde_json::json!({ "q": 2 }))
            .await
            .expect("call second");

        assert_eq!(state.initialize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.tools_list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.tools_call_calls.load(Ordering::SeqCst), 2);

        server.abort();
        let _ = server.await;
    }
}
