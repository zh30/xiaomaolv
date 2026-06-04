use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::{MessageRole, StoredMessage};
use crate::provider::{ChatProvider, CompletionRequest};

/// Strategy for context compaction
#[derive(Debug, Clone)]
pub enum CompactionStrategy {
    /// Keep first and last N messages, summarize middle
    HeadTail {
        head_count: usize,
        tail_count: usize,
    },
    /// Summarize messages older than N days
    AgeBased { max_age_days: usize },
    /// Compact when token budget exceeds threshold
    BudgetBased { max_tokens: usize },
}

#[derive(Debug, Clone, Default)]
pub struct CompactionMessageMetadata {
    pub source_id: Option<i64>,
    pub created_at: Option<i64>,
}

/// Request for compaction operation
pub struct CompactionRequest {
    pub messages: Vec<StoredMessage>,
    pub strategy: CompactionStrategy,
    pub metadata: Vec<CompactionMessageMetadata>,
    pub now_unix: Option<i64>,
    pub min_recent_messages: usize,
}

impl CompactionRequest {
    pub fn new(messages: Vec<StoredMessage>, strategy: CompactionStrategy) -> Self {
        Self {
            messages,
            strategy,
            metadata: Vec::new(),
            now_unix: None,
            min_recent_messages: 0,
        }
    }
}

/// Result of compaction operation
pub struct CompactionResult {
    pub compacted_messages: Vec<StoredMessage>,
    pub tokens_saved: usize,
    pub summary: String,
}

/// Compactor for context-aware message history compaction
#[derive(Clone)]
pub struct Compactor {
    enabled: bool,
}

impl Compactor {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Compact messages according to the specified strategy
    pub async fn compact(
        &self,
        request: CompactionRequest,
        model: Arc<dyn ChatProvider>,
    ) -> anyhow::Result<CompactionResult> {
        if !self.enabled {
            return Ok(CompactionResult {
                compacted_messages: request.messages,
                tokens_saved: 0,
                summary: String::new(),
            });
        }

        match request.strategy {
            CompactionStrategy::HeadTail {
                head_count,
                tail_count,
            } => {
                self.compact_head_tail(
                    request.messages,
                    head_count,
                    tail_count.max(request.min_recent_messages),
                    model,
                )
                .await
            }
            CompactionStrategy::AgeBased { max_age_days } => {
                self.compact_age_based(
                    request.messages,
                    request.metadata,
                    max_age_days,
                    request.now_unix.unwrap_or_else(unix_ts),
                    request.min_recent_messages,
                    model,
                )
                .await
            }
            CompactionStrategy::BudgetBased { max_tokens } => {
                self.compact_budget_based(
                    request.messages,
                    max_tokens,
                    request.min_recent_messages,
                    model,
                )
                .await
            }
        }
    }

    pub fn compact_with_summary(
        &self,
        request: CompactionRequest,
        summary: String,
    ) -> anyhow::Result<CompactionResult> {
        if !self.enabled {
            return Ok(CompactionResult {
                compacted_messages: request.messages,
                tokens_saved: 0,
                summary: String::new(),
            });
        }

        let summary = sanitize_summary(summary);
        Ok(match request.strategy {
            CompactionStrategy::HeadTail {
                head_count,
                tail_count,
            } => self.compact_head_tail_with_summary(
                request.messages,
                head_count,
                tail_count.max(request.min_recent_messages),
                summary,
            ),
            CompactionStrategy::AgeBased { max_age_days } => self.compact_age_based_with_summary(
                request.messages,
                request.metadata,
                max_age_days,
                request.now_unix.unwrap_or_else(unix_ts),
                request.min_recent_messages,
                summary,
            ),
            CompactionStrategy::BudgetBased { max_tokens } => self
                .compact_budget_based_with_summary(
                    request.messages,
                    max_tokens,
                    request.min_recent_messages,
                    summary,
                ),
        })
    }

    /// Head-tail compaction: keep first N and last N messages, summarize the middle
    async fn compact_head_tail(
        &self,
        messages: Vec<StoredMessage>,
        head_count: usize,
        tail_count: usize,
        model: Arc<dyn ChatProvider>,
    ) -> anyhow::Result<CompactionResult> {
        if messages.len() <= head_count + tail_count {
            return Ok(CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            });
        }

        let middle = messages[head_count..messages.len() - tail_count].to_vec();

        let middle_summary = self.summarize(&middle, model).await?;
        Ok(self.compact_head_tail_with_summary(messages, head_count, tail_count, middle_summary))
    }

    fn compact_head_tail_with_summary(
        &self,
        messages: Vec<StoredMessage>,
        head_count: usize,
        tail_count: usize,
        summary: String,
    ) -> CompactionResult {
        if messages.len() <= head_count + tail_count {
            return CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            };
        }

        let head = messages[..head_count].to_vec();
        let middle_len = messages.len() - head_count - tail_count;
        let tail = messages[messages.len() - tail_count..].to_vec();
        let middle_summary = sanitize_summary(summary);
        let compacted: Vec<StoredMessage> = vec![
            head,
            vec![StoredMessage {
                role: MessageRole::System,
                content: format!(
                    "[Earlier {} messages summarized: {}]",
                    middle_len, middle_summary
                ),
            }],
            tail,
        ]
        .into_iter()
        .flatten()
        .collect();

        let original_tokens = estimate_messages_tokens(&messages);
        let compacted_tokens = estimate_messages_tokens(&compacted);
        let tokens_saved = original_tokens.saturating_sub(compacted_tokens);

        CompactionResult {
            compacted_messages: compacted,
            tokens_saved,
            summary: middle_summary,
        }
    }

    async fn compact_budget_based(
        &self,
        messages: Vec<StoredMessage>,
        max_tokens: usize,
        min_recent_messages: usize,
        model: Arc<dyn ChatProvider>,
    ) -> anyhow::Result<CompactionResult> {
        let original_tokens = estimate_messages_tokens(&messages);
        if original_tokens <= max_tokens || messages.len() <= 3 {
            return Ok(CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            });
        }

        let tail_count = messages
            .len()
            .saturating_sub(1)
            .min(6usize.max(min_recent_messages));
        let head_count = 1usize;
        if messages.len() <= head_count + tail_count {
            return Ok(CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            });
        }

        let middle = messages[head_count..messages.len() - tail_count].to_vec();
        let summary = sanitize_summary(self.summarize(&middle, model).await?);
        Ok(self.compact_budget_based_with_summary(
            messages,
            max_tokens,
            min_recent_messages,
            summary,
        ))
    }

    fn compact_budget_based_with_summary(
        &self,
        messages: Vec<StoredMessage>,
        max_tokens: usize,
        min_recent_messages: usize,
        summary: String,
    ) -> CompactionResult {
        let original_tokens = estimate_messages_tokens(&messages);
        if original_tokens <= max_tokens || messages.len() <= 3 {
            return CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            };
        }

        let tail_count = messages
            .len()
            .saturating_sub(1)
            .min(6usize.max(min_recent_messages));
        let head_count = 1usize;
        if messages.len() <= head_count + tail_count {
            return CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            };
        }

        let head = messages[..head_count].to_vec();
        let middle_len = messages.len() - head_count - tail_count;
        let tail = messages[messages.len() - tail_count..].to_vec();
        let summary = sanitize_summary(summary);
        let compacted = vec![
            head,
            vec![StoredMessage {
                role: MessageRole::System,
                content: format!(
                    "[Budget compacted {} earlier messages: {}]",
                    middle_len, summary
                ),
            }],
            tail,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let compacted_tokens = estimate_messages_tokens(&compacted);

        CompactionResult {
            compacted_messages: compacted,
            tokens_saved: original_tokens.saturating_sub(compacted_tokens),
            summary,
        }
    }

    async fn compact_age_based(
        &self,
        messages: Vec<StoredMessage>,
        metadata: Vec<CompactionMessageMetadata>,
        max_age_days: usize,
        now_unix: i64,
        min_recent_messages: usize,
        model: Arc<dyn ChatProvider>,
    ) -> anyhow::Result<CompactionResult> {
        let compact_count = age_based_compact_count(
            messages.len(),
            &metadata,
            max_age_days,
            now_unix,
            min_recent_messages,
        );
        if compact_count == 0 {
            return Ok(CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            });
        }

        let stale = messages[..compact_count].to_vec();
        let summary = sanitize_summary(self.summarize(&stale, model).await?);
        Ok(self.compact_age_based_with_summary(
            messages,
            metadata,
            max_age_days,
            now_unix,
            min_recent_messages,
            summary,
        ))
    }

    fn compact_age_based_with_summary(
        &self,
        messages: Vec<StoredMessage>,
        metadata: Vec<CompactionMessageMetadata>,
        max_age_days: usize,
        now_unix: i64,
        min_recent_messages: usize,
        summary: String,
    ) -> CompactionResult {
        let compact_count = age_based_compact_count(
            messages.len(),
            &metadata,
            max_age_days,
            now_unix,
            min_recent_messages,
        );
        if compact_count == 0 {
            return CompactionResult {
                compacted_messages: messages,
                tokens_saved: 0,
                summary: String::new(),
            };
        }

        let original_tokens = estimate_messages_tokens(&messages);
        let tail = messages[compact_count..].to_vec();
        let summary = sanitize_summary(summary);
        let compacted = vec![
            vec![StoredMessage {
                role: MessageRole::System,
                content: format!(
                    "[Age compacted {} messages older than {} days: {}]",
                    compact_count, max_age_days, summary
                ),
            }],
            tail,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let compacted_tokens = estimate_messages_tokens(&compacted);

        CompactionResult {
            compacted_messages: compacted,
            tokens_saved: original_tokens.saturating_sub(compacted_tokens),
            summary,
        }
    }

    /// Summarize a group of messages using the LLM
    async fn summarize(
        &self,
        messages: &[StoredMessage],
        model: Arc<dyn ChatProvider>,
    ) -> anyhow::Result<String> {
        if messages.is_empty() {
            return Ok(String::new());
        }

        let conversation = messages
            .iter()
            .map(|m| format!("{}: {}", m.role.as_str(), m.content))
            .collect::<Vec<_>>()
            .join("\n");

        let summary_prompt = format!(
            "Summarize the following conversation in 2-3 sentences:\n{}",
            conversation
        );

        let response = model
            .complete(CompletionRequest {
                messages: vec![StoredMessage {
                    role: MessageRole::User,
                    content: summary_prompt,
                }],
                ..Default::default()
            })
            .await?;

        Ok(response)
    }
}

fn estimate_message_tokens(msg: &StoredMessage) -> usize {
    ((msg.content.chars().count().saturating_add(3)) / 4)
        .max(1)
        .saturating_add(4)
}

fn estimate_messages_tokens(messages: &[StoredMessage]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

fn age_based_compact_count(
    message_count: usize,
    metadata: &[CompactionMessageMetadata],
    max_age_days: usize,
    now_unix: i64,
    min_recent_messages: usize,
) -> usize {
    let retained_tail = 6usize.max(min_recent_messages);
    if message_count <= retained_tail || metadata.len() != message_count {
        return 0;
    }

    let max_age_secs = (max_age_days as i64).saturating_mul(86_400);
    let cutoff = now_unix.saturating_sub(max_age_secs);
    let stale_prefix = metadata
        .iter()
        .take_while(|meta| {
            meta.created_at
                .is_some_and(|created_at| created_at < cutoff)
        })
        .count();
    stale_prefix.min(message_count.saturating_sub(retained_tail))
}

fn sanitize_summary(summary: String) -> String {
    let mut summary = summary.trim().to_string();
    if summary.is_empty() {
        summary = "Summary unavailable.".to_string();
    }
    for marker in [
        "MCP_TOOL_RESULT_JSON",
        "MCP_TOOL_VERIFICATION_FAILED_JSON",
        "CODE_MODE_TOOL_RESULT_JSON",
    ] {
        summary = summary.replace(marker, "[tool-json]");
    }
    let max_chars = 4000usize;
    if summary.chars().count() > max_chars {
        let mut truncated = summary.chars().take(max_chars).collect::<String>();
        truncated.push_str("...(truncated)");
        truncated
    } else {
        summary
    }
}

fn unix_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MessageRole;

    fn create_test_messages(count: usize) -> Vec<StoredMessage> {
        (0..count)
            .map(|i| StoredMessage {
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: format!("Message {}", i),
            })
            .collect()
    }

    // Simple mock provider for testing that doesn't require configuration
    struct MockProvider;

    #[async_trait::async_trait]
    impl crate::provider::ChatProvider for MockProvider {
        async fn complete(
            &self,
            _req: crate::provider::CompletionRequest,
        ) -> anyhow::Result<String> {
            Ok("mock response".to_string())
        }
    }

    #[tokio::test]
    async fn test_compactor_disabled_returns_original() {
        let compactor = Compactor::new(false);
        let messages = create_test_messages(10);

        let result = compactor
            .compact(
                CompactionRequest::new(
                    messages,
                    CompactionStrategy::HeadTail {
                        head_count: 2,
                        tail_count: 2,
                    },
                ),
                Arc::new(MockProvider),
            )
            .await
            .unwrap();

        assert_eq!(result.compacted_messages.len(), 10);
        assert_eq!(result.tokens_saved, 0);
    }

    #[test]
    fn test_head_tail_compaction_preserves_context() {
        // This test verifies the structure without calling LLM
        let messages = create_test_messages(20);

        // Simulate head-tail logic manually
        let head_count = 2;
        let tail_count = 2;

        assert!(messages.len() > head_count + tail_count);

        let head = &messages[..head_count];
        let middle = &messages[head_count..messages.len() - tail_count];
        let tail = &messages[messages.len() - tail_count..];

        assert_eq!(head.len(), 2);
        assert_eq!(middle.len(), 16);
        assert_eq!(tail.len(), 2);

        // Verify head and tail are preserved
        assert_eq!(head[0].content, "Message 0");
        assert_eq!(head[1].content, "Message 1");
        assert_eq!(tail[0].content, "Message 18");
        assert_eq!(tail[1].content, "Message 19");
    }

    #[test]
    fn test_compaction_strategy_debug() {
        let strategy = CompactionStrategy::HeadTail {
            head_count: 3,
            tail_count: 3,
        };
        let debug_str = format!("{:?}", strategy);
        assert!(debug_str.contains("HeadTail"));
        assert!(debug_str.contains("3"));

        let age_strategy = CompactionStrategy::AgeBased { max_age_days: 7 };
        let age_debug = format!("{:?}", age_strategy);
        assert!(age_debug.contains("AgeBased"));

        let budget_strategy = CompactionStrategy::BudgetBased { max_tokens: 1000 };
        let budget_debug = format!("{:?}", budget_strategy);
        assert!(budget_debug.contains("BudgetBased"));
    }

    #[test]
    fn test_compaction_result_fields() {
        let result = CompactionResult {
            compacted_messages: vec![StoredMessage {
                role: MessageRole::User,
                content: "test".to_string(),
            }],
            tokens_saved: 100,
            summary: "Test summary".to_string(),
        };

        assert_eq!(result.compacted_messages.len(), 1);
        assert_eq!(result.tokens_saved, 100);
        assert_eq!(result.summary, "Test summary");
    }
}
