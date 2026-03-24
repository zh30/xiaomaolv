use std::sync::Arc;

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

/// Request for compaction operation
pub struct CompactionRequest {
    pub messages: Vec<StoredMessage>,
    pub strategy: CompactionStrategy,
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
                self.compact_head_tail(request.messages, head_count, tail_count, model)
                    .await
            }
            CompactionStrategy::AgeBased { max_age_days: _ } => {
                // Age-based compaction would require timestamp information in StoredMessage
                // For now, fall back to returning messages as-is
                Ok(CompactionResult {
                    compacted_messages: request.messages,
                    tokens_saved: 0,
                    summary: String::new(),
                })
            }
            CompactionStrategy::BudgetBased { max_tokens: _ } => {
                // Budget-based is handled by apply_context_budget in service.rs
                // This strategy would need integration with the existing budget logic
                Ok(CompactionResult {
                    compacted_messages: request.messages,
                    tokens_saved: 0,
                    summary: String::new(),
                })
            }
        }
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

        let head = messages[..head_count].to_vec();
        let middle = messages[head_count..messages.len() - tail_count].to_vec();
        let tail = messages[messages.len() - tail_count..].to_vec();

        let middle_summary = self.summarize(&middle, model).await?;
        let compacted: Vec<StoredMessage> = vec![
            head,
            vec![StoredMessage {
                role: MessageRole::System,
                content: format!(
                    "[Earlier {} messages summarized: {}]",
                    middle.len(),
                    middle_summary
                ),
            }],
            tail,
        ]
        .into_iter()
        .flatten()
        .collect();

        // Estimate tokens saved (rough approximation)
        let original_tokens: usize = messages
            .iter()
            .map(|m| m.content.chars().count().saturating_add(4))
            .sum();
        let compacted_tokens: usize = compacted
            .iter()
            .map(|m| m.content.chars().count().saturating_add(4))
            .sum();
        let tokens_saved = original_tokens.saturating_sub(compacted_tokens);

        Ok(CompactionResult {
            compacted_messages: compacted,
            tokens_saved,
            summary: middle_summary,
        })
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
            })
            .await?;

        Ok(response)
    }
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
                CompactionRequest {
                    messages,
                    strategy: CompactionStrategy::HeadTail {
                        head_count: 2,
                        tail_count: 2,
                    },
                },
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
