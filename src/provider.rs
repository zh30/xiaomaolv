use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

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

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<StoredMessage>,
}

#[async_trait]
pub trait StreamSink: Send {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()>;
}

#[async_trait]
pub trait ChatProvider: Send + Sync {
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
        let payload = OpenAiRequest {
            model: self.model.clone(),
            messages: normalized_messages.clone(),
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
                            )
                            .await
                            .unwrap_or_else(|err| {
                                warn!(error = %err, "compat non-stream recovery failed");
                                None
                            })
                        {
                            return Ok(text);
                        }
                        last_error = Some(anyhow::anyhow!(
                            "provider returned status {}{}",
                            status,
                            format_error_body_suffix(&body)
                        ));
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
        let attempts = retry_attempts(self.max_retries);
        let mut last_error = None;

        for attempt in 0..attempts {
            // Streamed long-form replies can exceed regular request timeout.
            let stream_timeout_secs = self.timeout_secs.saturating_mul(10).max(300);
            let response = self
                .client
                .post(&url)
                .timeout(Duration::from_secs(stream_timeout_secs))
                .bearer_auth(&self.api_key)
                .json(&serde_json::json!({
                    "model": self.model.clone(),
                    "messages": messages.clone(),
                    "stream": true
                }))
                .send()
                .await;

            match response {
                Ok(mut resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        let body = resp.text().await.unwrap_or_default();
                        last_error = Some(anyhow::anyhow!(
                            "provider returned status {}{}",
                            status,
                            format_error_body_suffix(&body)
                        ));
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
    let lower = body.to_lowercase();
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
    use super::{
        build_compat_messages, extract_stream_delta, is_message_shape_error, normalize_messages,
        retry_attempts, truncate_chars,
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
}
