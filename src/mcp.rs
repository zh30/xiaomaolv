use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
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

#[derive(Debug, Clone, Serialize)]
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

pub struct McpRuntime {
    servers: HashMap<String, McpServerConfig>,
    http_client: reqwest::Client,
}

impl Default for McpRuntime {
    fn default() -> Self {
        Self::new(HashMap::new())
    }
}

impl McpRuntime {
    pub fn new(servers: HashMap<String, McpServerConfig>) -> Self {
        Self {
            servers,
            http_client: reqwest::Client::new(),
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
        let response = self
            .request_server(server_name, cfg, "tools/list", serde_json::json!({}))
            .await?;

        let tools = response
            .get("result")
            .and_then(|result| result.get("tools"))
            .and_then(|tools| tools.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(tools
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
            .collect())
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
        let mut writer = stdin;
        let mut reader = BufReader::new(stdout);
        let timeout_secs = cfg.timeout_secs.max(1);

        write_json_line(
            &mut writer,
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

        let init_response = read_jsonrpc_message(&mut reader, Duration::from_secs(timeout_secs))
            .await
            .with_context(|| format!("mcp server '{server_name}' initialize read failed"))?;

        if init_response.get("error").is_some() {
            bail!("mcp server '{server_name}' initialize returned error: {init_response}");
        }

        write_json_line(
            &mut writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }),
        )
        .await?;

        write_json_line(
            &mut writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": method,
                "params": params
            }),
        )
        .await?;

        let response = loop {
            let msg = read_jsonrpc_message(&mut reader, Duration::from_secs(timeout_secs))
                .await
                .with_context(|| format!("mcp server '{server_name}' read failed"))?;
            let id = msg.get("id");
            if id.is_none() {
                continue;
            }
            if id == Some(&serde_json::json!(2)) {
                break msg;
            }
        };

        let _ = child.kill().await;
        Ok(response)
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

        let mut session_id: Option<String> = None;
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
            .http_jsonrpc(url, cfg, &initialize, &mut session_id, timeout_secs)
            .await?;
        if init.get("error").is_some() {
            bail!("mcp server '{server_name}' initialize returned error: {init}");
        }

        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let _ = self
            .http_jsonrpc(url, cfg, &initialized, &mut session_id, timeout_secs)
            .await?;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": method,
            "params": params
        });
        self.http_jsonrpc(url, cfg, &request, &mut session_id, timeout_secs)
            .await
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
    use tempfile::tempdir;

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
}
