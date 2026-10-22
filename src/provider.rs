use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::config::ProviderConfig;
use crate::domain::{MessageRole, StoredMessage};

const RETRY_BACKOFF_BASE_MS: u64 = 100;
const ERROR_BODY_MAX_CHARS: usize = 512;
const COMPAT_MESSAGE_LIMIT: usize = 16;

#[derive(Debug, Clone, Default)]
pub struct CompletionRequest {
    pub messages: Vec<StoredMessage>,
    pub response_format: Option<ResponseFormat>,
}

impl CompletionRequest {
    pub fn from_messages(messages: Vec<StoredMessage>) -> Self {
        Self {
            messages,
            response_format: None,
        }
    }

    pub fn json(messages: Vec<StoredMessage>) -> Self {
        Self {
            messages,
            response_format: Some(ResponseFormat::JsonObject),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ResponseFormat {
    JsonObject,
}

#[async_trait]
pub trait StreamSink: Send {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()>;
}

#[async_trait]
pub trait ChatProvider: Send + Sync {
    fn model_name(&self) -> Option<&str> {
        None
    }

    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String>;

    async fn complete_stream(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        let text = self.complete(req).await?;
        if !text.is_empty() {
            sink.on_delta(&text).await?;
        }
        Ok(text)
    }
}

/// Typed HTTP failure from a provider call — lets the failover chain
/// distinguish request-shape errors (which every provider would also
/// reject) from service-side failures worth trying elsewhere.
#[derive(Debug)]
pub struct ProviderHttpError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider returned status {}{}",
            self.status,
            format_error_body_suffix(&self.body)
        )
    }
}

impl std::error::Error for ProviderHttpError {}

/// Whether an error is worth failing over to the next provider.
/// 4xx request-shape errors (400/404/422) are not — every provider
/// would reject the same request; everything else (5xx, 401/403 auth,
/// 429 rate limit, timeouts, transport) is.
pub fn failover_worthy(err: &anyhow::Error, failover_on_4xx: bool) -> bool {
    if let Some(http) = err.downcast_ref::<ProviderHttpError>()
        && !failover_on_4xx
        && matches!(http.status, 400 | 404 | 422)
    {
        return false;
    }
    true
}

pub trait ProviderFactory: Send + Sync {
    fn kind(&self) -> &'static str;
    fn create(
        &self,
        provider_name: &str,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ChatProvider>>;
}

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    factories: HashMap<String, Arc<dyn ProviderFactory>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry
            .register(Arc::new(OpenAiCompatibleProviderFactory))
            .expect("failed to register default provider factory");
        registry
    }

    pub fn register(&mut self, factory: Arc<dyn ProviderFactory>) -> anyhow::Result<()> {
        let kind = factory.kind().to_string();
        if self.factories.contains_key(&kind) {
            bail!("provider factory '{kind}' already registered");
        }
        self.factories.insert(kind, factory);
        Ok(())
    }

    pub fn build(
        &self,
        provider_name: &str,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ChatProvider>> {
        let factory = self.factories.get(&config.kind).with_context(|| {
            format!(
                "provider kind '{}' is not registered for provider '{}'.",
                config.kind, provider_name
            )
        })?;
        factory.create(provider_name, config)
    }
}

pub struct OpenAiCompatibleProviderFactory;

impl ProviderFactory for OpenAiCompatibleProviderFactory {
    fn kind(&self) -> &'static str {
        "openai-compatible"
    }

    fn create(
        &self,
        _provider_name: &str,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ChatProvider>> {
        Ok(Arc::new(OpenAiCompatibleProvider::from_config(config)?))
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    timeout_secs: u64,
    max_retries: usize,
}

impl OpenAiCompatibleProvider {
    pub fn from_config(cfg: &ProviderConfig) -> anyhow::Result<Self> {
        let client = Client::builder()
            .pool_max_idle_per_host(20)
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .http1_title_case_headers()
            .build()
            .context("failed to build reqwest client")?;

        let base_url = required_setting(cfg, "base_url", cfg.base_url.as_ref())?;
        let api_key = required_setting(cfg, "api_key", cfg.api_key.as_ref())?;
        let model = required_setting(cfg, "model", cfg.model.as_ref())?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            timeout_secs: cfg.timeout_secs.max(1),
            max_retries: cfg.max_retries,
        })
    }
}

fn required_setting(
    config: &ProviderConfig,
    key: &str,
    top_level_value: Option<&String>,
) -> anyhow::Result<String> {
    if let Some(value) = top_level_value
        && !value.is_empty()
    {
        return Ok(value.clone());
    }

    if let Some(value) = config.options.get(key)
        && !value.is_empty()
    {
        return Ok(value.clone());
    }

    bail!(
        "provider kind '{}' is missing required setting '{}'.",
        config.kind,
        key
    )
}

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<StoredMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormatPayload>,
}

#[derive(Serialize)]
struct ResponseFormatPayload {
    r#type: String,
}

impl From<&ResponseFormat> for ResponseFormatPayload {
    fn from(fmt: &ResponseFormat) -> Self {
        match fmt {
            ResponseFormat::JsonObject => Self {
                r#type: "json_object".to_string(),
            },
        }
    }
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

#[async_trait]
impl ChatProvider for OpenAiCompatibleProvider {
    fn model_name(&self) -> Option<&str> {
        Some(&self.model)
    }

    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        self.complete_with_retry(req).await
    }

    async fn complete_stream(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        self.complete_stream_with_retry(req, sink).await
    }
}

impl OpenAiCompatibleProvider {
    async fn complete_with_retry(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let normalized_messages = normalize_messages(req.messages);
        let response_format = req
            .response_format
            .as_ref()
            .map(ResponseFormatPayload::from);
        let payload = OpenAiRequest {
            model: self.model.clone(),
            messages: normalized_messages.clone(),
            response_format,
        };

        let attempts = retry_attempts(self.max_retries);
        let mut last_error = None;

        for attempt in 0..attempts {
            let response = self
                .client
                .post(&url)
                .timeout(Duration::from_secs(self.timeout_secs))
                .bearer_auth(&self.api_key)
                .json(&payload)
                .send()
                .await;

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        let body = resp.text().await.unwrap_or_default();
                        if let Some(text) = self
                            .try_compat_nonstream_recovery(
                                &url,
                                &normalized_messages,
                                status,
                                &body,
                                req.response_format.as_ref(),
                            )
                            .await
                            .unwrap_or_else(|err| {
                                warn!(error = %err, "compat non-stream recovery failed");
                                None
                            })
                        {
                            return Ok(text);
                        }
                        last_error = Some(
                            ProviderHttpError {
                                status: status.as_u16(),
                                body,
                            }
                            .into(),
                        );
                    } else {
                        let body: OpenAiResponse = resp
                            .json()
                            .await
                            .context("failed to deserialize provider response")?;

                        if let Some(choice) = body.choices.into_iter().next() {
                            return Ok(choice.message.content);
                        }
                        bail!("provider response has no choices");
                    }
                }
                Err(err) => {
                    last_error = Some(err.into());
                }
            }

            if attempt + 1 < attempts {
                tokio::time::sleep(retry_backoff_delay(attempt)).await;
            }
        }

        Err(last_error
            .unwrap_or_else(|| anyhow::anyhow!("provider request failed without details")))
    }

    async fn complete_stream_with_retry(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let messages = normalize_messages(req.messages);
        let response_format_payload = req
            .response_format
            .as_ref()
            .map(ResponseFormatPayload::from);
        let attempts = retry_attempts(self.max_retries);
        let mut last_error = None;

        for attempt in 0..attempts {
            // Streamed long-form replies can exceed regular request timeout.
            let stream_timeout_secs = self.timeout_secs.saturating_mul(10).max(300);
            let mut stream_body = serde_json::json!({
                "model": self.model.clone(),
                "messages": messages.clone(),
                "stream": true
            });
            if let Some(ref fmt) = response_format_payload {
                stream_body["response_format"] = serde_json::json!({ "type": fmt.r#type });
            }
            let response = self
                .client
                .post(&url)
                .timeout(Duration::from_secs(stream_timeout_secs))
                .bearer_auth(&self.api_key)
                .json(&stream_body)
                .send()
                .await;

            match response {
                Ok(mut resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        let body = resp.text().await.unwrap_or_default();
                        last_error = Some(
                            ProviderHttpError {
                                status: status.as_u16(),
                                body,
                            }
                            .into(),
                        );
                    } else {
                        let mut assembled = String::new();
                        let mut buffer = String::new();

                        while let Some(chunk) = resp
                            .chunk()
                            .await
                            .context("failed to read provider stream chunk")?
                        {
                            buffer.push_str(&String::from_utf8_lossy(&chunk));
                            while let Some(index) = buffer.find('\n') {
                                let mut line = buffer.drain(..=index).collect::<String>();
                                if line.ends_with('\n') {
                                    line.pop();
                                }
                                if line.ends_with('\r') {
                                    line.pop();
                                }
                                if let Some(delta) = extract_stream_delta(&line)? {
                                    assembled.push_str(&delta);
                                    sink.on_delta(&delta).await?;
                                }
                            }
                        }

                        if !buffer.trim().is_empty()
                            && let Some(delta) = extract_stream_delta(buffer.trim())?
                        {
                            assembled.push_str(&delta);
                            sink.on_delta(&delta).await?;
                        }

                        if !assembled.is_empty() {
                            return Ok(assembled);
                        }

                        last_error = Some(anyhow::anyhow!(
                            "provider stream completed without text delta"
                        ));
                    }
                }
                Err(err) => {
                    last_error = Some(err.into());
                }
            }

            if attempt + 1 < attempts {
                tokio::time::sleep(retry_backoff_delay(attempt)).await;
            }
        }

        let fallback = self
            .complete_with_retry(CompletionRequest {
                messages: messages.clone(),
                response_format: req.response_format.clone(),
            })
            .await;

        match fallback {
            Ok(text) => {
                if !text.is_empty() {
                    sink.on_delta(&text).await?;
                }
                Ok(text)
            }
            Err(fallback_err) => Err(last_error.unwrap_or(fallback_err)),
        }
    }

    async fn try_compat_nonstream_recovery(
        &self,
        url: &str,
        messages: &[StoredMessage],
        status: StatusCode,
        body: &str,
        response_format: Option<&ResponseFormat>,
    ) -> anyhow::Result<Option<String>> {
        if !is_message_shape_error(status, body) {
            return Ok(None);
        }

        let compat_messages = build_compat_messages(messages);
        if compat_messages.is_empty() || compat_messages == messages {
            return Ok(None);
        }

        warn!(
            status = %status,
            original_messages = messages.len(),
            compat_messages = compat_messages.len(),
            "provider request rejected by message shape, retry with compatibility message set"
        );

        let payload = OpenAiRequest {
            model: self.model.clone(),
            messages: compat_messages,
            response_format: response_format.map(ResponseFormatPayload::from),
        };
        let resp = self
            .client
            .post(url)
            .timeout(Duration::from_secs(self.timeout_secs))
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .context("compat recovery request failed")?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let parsed: OpenAiResponse = resp
            .json()
            .await
            .context("failed to deserialize compat recovery response")?;
        Ok(parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content))
    }
}

type ChainEntry = (String, Arc<dyn ChatProvider>, Arc<Mutex<Circuit>>);

/// Per-provider circuit state shared across the failover chain. A
/// provider opens after `failure_threshold` consecutive failures and
/// stays out of rotation until `cooldown` elapses; the first probe
/// through a half-open circuit either closes it (success) or re-arms
/// the cooldown (failure).
#[derive(Debug)]
struct Circuit {
    consecutive_failures: u32,
    opened_until: Option<Instant>,
}

impl Circuit {
    fn new() -> Self {
        Self {
            consecutive_failures: 0,
            opened_until: None,
        }
    }

    /// A closed or cooldown-elapsed circuit accepts calls.
    fn admits(&self) -> bool {
        match self.opened_until {
            Some(until) => Instant::now() >= until,
            None => true,
        }
    }

    fn on_success(&mut self) {
        self.consecutive_failures = 0;
        self.opened_until = None;
    }

    fn on_failure(&mut self, threshold: u32, cooldown: Duration) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= threshold || self.opened_until.is_some() {
            self.opened_until = Some(Instant::now() + cooldown);
        }
    }
}

/// Ordered cross-provider failover: tries the primary first, then each
/// fallback in order, skipping providers whose circuit is open.
/// Streaming requests only fail over while no delta has been emitted —
/// once bytes reach the channel the result is already partially
/// committed and a retry would duplicate output.
pub struct FailoverProvider {
    chain: Vec<ChainEntry>,
    failure_threshold: u32,
    cooldown: Duration,
    failover_on_4xx: bool,
}

impl FailoverProvider {
    pub fn new(
        chain: Vec<(String, Arc<dyn ChatProvider>)>,
        failure_threshold: u32,
        cooldown: Duration,
        failover_on_4xx: bool,
    ) -> Self {
        Self {
            chain: chain
                .into_iter()
                .map(|(name, provider)| (name, provider, Arc::new(Mutex::new(Circuit::new()))))
                .collect::<Vec<ChainEntry>>(),
            failure_threshold: failure_threshold.max(1),
            cooldown,
            failover_on_4xx,
        }
    }

    fn record_success(&self, idx: usize) {
        if let Ok(mut circuit) = self.chain[idx].2.lock() {
            circuit.on_success();
        }
    }

    fn record_failure(&self, idx: usize) {
        if let Ok(mut circuit) = self.chain[idx].2.lock() {
            circuit.on_failure(self.failure_threshold, self.cooldown);
        }
    }

    fn admitted_indices(&self) -> Vec<usize> {
        self.chain
            .iter()
            .enumerate()
            .filter(|(_, (_, _, circuit))| circuit.lock().map(|c| c.admits()).unwrap_or(true))
            .map(|(i, _)| i)
            .collect()
    }
}

#[async_trait]
impl ChatProvider for FailoverProvider {
    fn model_name(&self) -> Option<&str> {
        self.chain.first().and_then(|(_, p, _)| p.model_name())
    }

    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let mut last_error: Option<anyhow::Error> = None;
        let mut tried = false;
        for idx in self.admitted_indices() {
            if let Some(err) = &last_error
                && !failover_worthy(err, self.failover_on_4xx)
            {
                break;
            }
            tried = true;
            let (name, provider, _) = &self.chain[idx];
            match provider.complete(req.clone()).await {
                Ok(text) => {
                    self.record_success(idx);
                    return Ok(text);
                }
                Err(err) => {
                    warn!(provider = %name, error = %err, "provider failed, trying next in failover chain");
                    self.record_failure(idx);
                    last_error = Some(err);
                }
            }
        }
        let _ = tried;
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("all failover providers circuit-open or unavailable")
        }))
    }

    async fn complete_stream(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        let mut last_error: Option<anyhow::Error> = None;
        for idx in self.admitted_indices() {
            if let Some(err) = &last_error
                && !failover_worthy(err, self.failover_on_4xx)
            {
                break;
            }
            let (name, provider, _) = &self.chain[idx];
            // Gate counts deltas actually emitted — only fail over while
            // the stream produced nothing, or the user gets duplicated text.
            let mut gate = DeltaGateSink {
                inner: sink,
                emitted: 0,
            };
            match provider.complete_stream(req.clone(), &mut gate).await {
                Ok(text) => {
                    self.record_success(idx);
                    return Ok(text);
                }
                Err(err) => {
                    self.record_failure(idx);
                    if gate.emitted > 0 {
                        // Partial output already delivered — do not retry
                        // elsewhere, the answer is committed.
                        warn!(provider = %name, error = %err, "stream failed after partial output; not failing over");
                        return Err(err);
                    }
                    warn!(provider = %name, error = %err, "stream failed before first delta, trying next");
                    last_error = Some(err);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("all failover providers circuit-open or unavailable")
        }))
    }
}

/// Counting sink: whether any delta reached the channel decides if the
/// stream may fail over to the next provider.
struct DeltaGateSink<'a> {
    inner: &'a mut dyn StreamSink,
    emitted: usize,
}

#[async_trait]
impl StreamSink for DeltaGateSink<'_> {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        self.inner.on_delta(delta).await?;
        self.emitted += 1;
        Ok(())
    }
}

fn retry_attempts(max_retries: usize) -> usize {
    max_retries.saturating_add(1).max(1)
}

fn retry_backoff_delay(attempt: usize) -> Duration {
    Duration::from_millis(RETRY_BACKOFF_BASE_MS * (attempt as u64 + 1))
}

fn normalize_messages(messages: Vec<StoredMessage>) -> Vec<StoredMessage> {
    let mut normalized: Vec<StoredMessage> = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.content.trim().is_empty() {
            continue;
        }
        if let Some(last) = normalized.last_mut()
            && last.role == msg.role
        {
            last.content.push_str("\n\n");
            last.content.push_str(&msg.content);
            continue;
        }
        normalized.push(msg);
    }
    normalized
}

fn build_compat_messages(messages: &[StoredMessage]) -> Vec<StoredMessage> {
    let system_guidance = messages
        .iter()
        .filter(|msg| msg.role == MessageRole::System)
        .map(|msg| msg.content.trim())
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut filtered = normalize_messages(
        messages
            .iter()
            .filter(|msg| msg.role != MessageRole::System)
            .cloned()
            .collect(),
    );

    if filtered.len() > COMPAT_MESSAGE_LIMIT {
        filtered = filtered.split_off(filtered.len().saturating_sub(COMPAT_MESSAGE_LIMIT));
    }

    if filtered.is_empty()
        && let Some(last_user) = messages
            .iter()
            .rev()
            .find(|msg| msg.role == MessageRole::User && !msg.content.trim().is_empty())
    {
        filtered.push(last_user.clone());
    }

    if !system_guidance.is_empty() {
        let folded = format!("系统提示（兼容模式）:\n{system_guidance}");
        if let Some(first_user) = filtered
            .iter_mut()
            .find(|msg| msg.role == MessageRole::User)
        {
            first_user.content = format!("{folded}\n\n{}", first_user.content);
        } else {
            filtered.insert(
                0,
                StoredMessage {
                    role: MessageRole::User,
                    content: folded,
                },
            );
        }
    }

    filtered
}

fn is_message_shape_error(status: StatusCode, body: &str) -> bool {
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    // Only check first 256 chars to avoid allocating large strings for huge error bodies
    let check_len = body.len().min(256);
    let lower = &body[..check_len];
    lower.contains("invalid chat setting")
        || lower.contains("expr_path=messages")
        || lower.contains("missing required parameter")
        || lower.contains("invalid params")
}

fn format_error_body_suffix(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {}", truncate_chars(trimmed, ERROR_BODY_MAX_CHARS))
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return value.to_string();
    }

    let mut out = value.chars().take(max_chars).collect::<String>();
    out.push_str("...(truncated)");
    out
}

fn extract_stream_delta(line: &str) -> anyhow::Result<Option<String>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with("data:") {
        return Ok(None);
    }

    let payload = trimmed.trim_start_matches("data:").trim();
    if payload.is_empty() || payload == "[DONE]" {
        return Ok(None);
    }

    let event: Value = serde_json::from_str(payload)
        .with_context(|| format!("invalid stream event: {payload}"))?;

    if let Some(delta) = extract_content_from_event(&event) {
        return Ok(Some(delta));
    }

    Ok(None)
}

fn extract_content_from_event(event: &Value) -> Option<String> {
    let choice = event
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|choices| choices.first())?;

    if let Some(content) = choice
        .get("delta")
        .and_then(|delta| delta.get("content"))
        .and_then(|content| content.as_str())
    {
        return Some(content.to_string());
    }

    if let Some(content_items) = choice
        .get("delta")
        .and_then(|delta| delta.get("content"))
        .and_then(|content| content.as_array())
    {
        let text = content_items
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            return Some(text);
        }
    }

    if let Some(content) = choice
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
    {
        return Some(content.to_string());
    }

    choice
        .get("text")
        .and_then(|text| text.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::{
        ChatProvider, CompletionRequest, FailoverProvider, ProviderHttpError, StreamSink,
        build_compat_messages, extract_stream_delta, failover_worthy, is_message_shape_error,
        normalize_messages, retry_attempts, truncate_chars,
    };
    use crate::domain::{MessageRole, StoredMessage};
    use reqwest::StatusCode;

    #[test]
    fn retry_attempts_include_initial_request() {
        assert_eq!(retry_attempts(0), 1);
        assert_eq!(retry_attempts(2), 3);
    }

    #[test]
    fn truncate_chars_appends_marker_when_limited() {
        assert_eq!(truncate_chars("abcdef", 3), "abc...(truncated)");
        assert_eq!(truncate_chars("abc", 3), "abc");
    }

    #[test]
    fn extract_stream_delta_reads_openai_sse_payload() {
        let line = r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#;
        let delta = extract_stream_delta(line).expect("extract delta");
        assert_eq!(delta.as_deref(), Some("hello"));
    }

    #[test]
    fn normalize_messages_merges_adjacent_roles_and_drops_empty() {
        let messages = vec![
            StoredMessage {
                role: MessageRole::User,
                content: "hello".to_string(),
            },
            StoredMessage {
                role: MessageRole::User,
                content: "  world  ".to_string(),
            },
            StoredMessage {
                role: MessageRole::Assistant,
                content: "ok".to_string(),
            },
            StoredMessage {
                role: MessageRole::Assistant,
                content: "   ".to_string(),
            },
        ];

        let out = normalize_messages(messages);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, MessageRole::User);
        assert!(out[0].content.contains("hello"));
        assert!(out[0].content.contains("world"));
        assert_eq!(out[1].role, MessageRole::Assistant);
    }

    #[test]
    fn build_compat_messages_drops_system_and_limits_tail() {
        let mut messages = Vec::new();
        messages.push(StoredMessage {
            role: MessageRole::System,
            content: "system memo".to_string(),
        });
        for idx in 0..24 {
            messages.push(StoredMessage {
                role: if idx % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: format!("m{idx}"),
            });
        }

        let out = build_compat_messages(&messages);
        assert!(out.len() <= 16);
        assert!(out.iter().all(|m| m.role != MessageRole::System));
        assert!(out.iter().any(|m| m.content == "m23"));
    }

    #[test]
    fn build_compat_messages_preserves_system_guidance_via_user_message() {
        let messages = vec![
            StoredMessage {
                role: MessageRole::System,
                content: "运行时当前时间: 2026-02-26".to_string(),
            },
            StoredMessage {
                role: MessageRole::User,
                content: "现在是哪一年？".to_string(),
            },
        ];

        let out = build_compat_messages(&messages);
        assert!(out.iter().all(|m| m.role != MessageRole::System));
        assert!(out.iter().any(|m| {
            m.role == MessageRole::User && m.content.contains("运行时当前时间: 2026-02-26")
        }));
    }

    #[test]
    fn is_message_shape_error_detects_minimax_style_400() {
        assert!(is_message_shape_error(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"invalid params, invalid chat setting (2013)"}}"#,
        ));
        assert!(!is_message_shape_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid chat setting",
        ));
    }

    enum ScriptOutcome {
        Ok(String),
        Http(u16),
    }

    struct ScriptProvider {
        calls: Arc<AtomicUsize>,
        outcome: ScriptOutcome,
        stream_emits_before_error: bool,
    }

    impl ScriptProvider {
        fn ok(text: &str) -> (Arc<AtomicUsize>, Self) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                calls.clone(),
                Self {
                    calls,
                    outcome: ScriptOutcome::Ok(text.to_string()),
                    stream_emits_before_error: false,
                },
            )
        }

        fn fail(err: anyhow::Error) -> (Arc<AtomicUsize>, Self) {
            let status = err
                .downcast_ref::<ProviderHttpError>()
                .map(|e| e.status)
                .unwrap_or(500);
            let calls = Arc::new(AtomicUsize::new(0));
            (
                calls.clone(),
                Self {
                    calls,
                    outcome: ScriptOutcome::Http(status),
                    stream_emits_before_error: false,
                },
            )
        }

        fn run(&self) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.outcome {
                ScriptOutcome::Ok(text) => Ok(text.clone()),
                ScriptOutcome::Http(status) => Err(http_err(*status)),
            }
        }
    }

    #[async_trait::async_trait]
    impl ChatProvider for ScriptProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            self.run()
        }

        async fn complete_stream(
            &self,
            _req: CompletionRequest,
            sink: &mut dyn StreamSink,
        ) -> anyhow::Result<String> {
            if self.stream_emits_before_error {
                sink.on_delta("partial-").await?;
            }
            self.run()
        }
    }

    struct CollectSink {
        deltas: Vec<String>,
    }

    #[async_trait::async_trait]
    impl StreamSink for CollectSink {
        async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
            self.deltas.push(delta.to_string());
            Ok(())
        }
    }

    fn http_err(status: u16) -> anyhow::Error {
        ProviderHttpError {
            status,
            body: "err".to_string(),
        }
        .into()
    }

    fn req() -> CompletionRequest {
        CompletionRequest {
            messages: vec![StoredMessage {
                role: MessageRole::User,
                content: "hi".to_string(),
            }],
            response_format: None,
        }
    }

    #[test]
    fn failover_worthy_rejects_request_shape_errors() {
        assert!(!failover_worthy(&http_err(400), false));
        assert!(!failover_worthy(&http_err(422), false));
        assert!(failover_worthy(&http_err(429), false));
        assert!(failover_worthy(&http_err(500), false));
        assert!(failover_worthy(&http_err(401), false));
        assert!(failover_worthy(&http_err(400), true));
    }

    #[tokio::test]
    async fn failover_uses_backup_when_primary_fails() {
        let (primary_calls, primary) = ScriptProvider::fail(http_err(500));
        let (backup_calls, backup) = ScriptProvider::ok("backup-answer");
        let chain = vec![
            (
                "primary".to_string(),
                Arc::new(primary) as Arc<dyn ChatProvider>,
            ),
            (
                "backup".to_string(),
                Arc::new(backup) as Arc<dyn ChatProvider>,
            ),
        ];
        let failover = FailoverProvider::new(chain, 3, Duration::from_secs(60), false);
        let out = failover.complete(req()).await.expect("failover result");
        assert_eq!(out, "backup-answer");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backup_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failover_does_not_retry_request_shape_errors() {
        let (_primary_calls, primary) = ScriptProvider::fail(http_err(400));
        let (backup_calls, backup) = ScriptProvider::ok("backup-answer");
        let chain = vec![
            (
                "primary".to_string(),
                Arc::new(primary) as Arc<dyn ChatProvider>,
            ),
            (
                "backup".to_string(),
                Arc::new(backup) as Arc<dyn ChatProvider>,
            ),
        ];
        let failover = FailoverProvider::new(chain, 3, Duration::from_secs(60), false);
        assert!(failover.complete(req()).await.is_err());
        assert_eq!(backup_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn circuit_opens_after_threshold_and_recovers() {
        let (primary_calls, primary) = ScriptProvider::fail(http_err(500));
        let (backup_calls, backup) = ScriptProvider::ok("b");
        let chain = vec![
            (
                "primary".to_string(),
                Arc::new(primary) as Arc<dyn ChatProvider>,
            ),
            (
                "backup".to_string(),
                Arc::new(backup) as Arc<dyn ChatProvider>,
            ),
        ];
        let failover = FailoverProvider::new(chain, 2, Duration::from_millis(50), false);

        for _ in 0..2 {
            assert!(failover.complete(req()).await.is_ok());
        }
        // After 2 failures the primary circuit is open — next call goes
        // straight to backup.
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        assert!(failover.complete(req()).await.is_ok());
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        assert_eq!(backup_calls.load(Ordering::SeqCst), 3);

        // Cooldown elapsed → primary admitted again (half-open probe).
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(failover.complete(req()).await.is_ok());
        assert_eq!(primary_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn stream_fails_over_only_before_first_delta() {
        // Primary streams partial text then dies — output is committed,
        // no failover.
        let (_primary_calls, mut primary) = ScriptProvider::fail(http_err(500));
        primary.stream_emits_before_error = true;
        let (backup_calls, backup) = ScriptProvider::ok("backup");
        let chain = vec![
            (
                "primary".to_string(),
                Arc::new(primary) as Arc<dyn ChatProvider>,
            ),
            (
                "backup".to_string(),
                Arc::new(backup) as Arc<dyn ChatProvider>,
            ),
        ];
        let failover = FailoverProvider::new(chain, 3, Duration::from_secs(60), false);
        let mut sink = CollectSink { deltas: vec![] };
        assert!(failover.complete_stream(req(), &mut sink).await.is_err());
        assert_eq!(sink.deltas, vec!["partial-"]);
        assert_eq!(backup_calls.load(Ordering::SeqCst), 0);

        // Primary fails before any delta → clean failover.
        let (_pc, primary) = ScriptProvider::fail(http_err(500));
        let (bc, backup) = ScriptProvider::ok("backup");
        let chain = vec![
            (
                "primary".to_string(),
                Arc::new(primary) as Arc<dyn ChatProvider>,
            ),
            (
                "backup".to_string(),
                Arc::new(backup) as Arc<dyn ChatProvider>,
            ),
        ];
        let failover = FailoverProvider::new(chain, 3, Duration::from_secs(60), false);
        let mut sink = CollectSink { deltas: vec![] };
        let out = failover
            .complete_stream(req(), &mut sink)
            .await
            .expect("stream failover");
        assert_eq!(out, "backup");
        assert_eq!(bc.load(Ordering::SeqCst), 1);
    }
}
