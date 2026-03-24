use std::sync::Arc;
use xiaomaolv::domain::{MessageRole, StoredMessage};
use xiaomaolv::harness::compactor::{
    CompactionRequest, CompactionResult, CompactionStrategy, Compactor,
};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};

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

/// A mock chat provider that returns a fixed response for testing
struct MockChatProvider {
    response: String,
}

impl MockChatProvider {
    fn new(response: String) -> Self {
        Self { response }
    }
}

#[async_trait::async_trait]
impl ChatProvider for MockChatProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok(self.response.clone())
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
            Arc::new(MockChatProvider::new("summary".to_string())),
        )
        .await
        .unwrap();

    assert_eq!(result.compacted_messages.len(), 10);
    assert_eq!(result.tokens_saved, 0);
}

#[tokio::test]
async fn test_head_tail_compaction_short_messages_no_op() {
    let compactor = Compactor::new(true);
    // Head: 2, Tail: 2, total: 4 - exactly at threshold, should return unchanged
    let messages = create_test_messages(4);

    let result = compactor
        .compact(
            CompactionRequest {
                messages,
                strategy: CompactionStrategy::HeadTail {
                    head_count: 2,
                    tail_count: 2,
                },
            },
            Arc::new(MockChatProvider::new("summary".to_string())),
        )
        .await
        .unwrap();

    // Should return unchanged since 4 <= 2 + 2
    assert_eq!(result.compacted_messages.len(), 4);
    assert_eq!(result.tokens_saved, 0);
}

#[tokio::test]
async fn test_head_tail_compaction_calls_llm() {
    let compactor = Compactor::new(true);
    let messages = create_test_messages(20);

    let mock_response = "This is a summarized conversation.".to_string();
    let result = compactor
        .compact(
            CompactionRequest {
                messages,
                strategy: CompactionStrategy::HeadTail {
                    head_count: 2,
                    tail_count: 2,
                },
            },
            Arc::new(MockChatProvider::new(mock_response.clone())),
        )
        .await
        .unwrap();

    // Expected: 2 head + 1 summary + 2 tail = 5 messages
    assert_eq!(result.compacted_messages.len(), 5);

    // The summary message should contain the LLM response
    let summary_msg = &result.compacted_messages[2];
    assert!(summary_msg.content.contains(&mock_response));
    assert_eq!(summary_msg.role, MessageRole::System);
}

#[tokio::test]
async fn test_head_tail_compaction_preserves_head_and_tail() {
    let compactor = Compactor::new(true);
    let messages = create_test_messages(20);

    let result = compactor
        .compact(
            CompactionRequest {
                messages,
                strategy: CompactionStrategy::HeadTail {
                    head_count: 2,
                    tail_count: 2,
                },
            },
            Arc::new(MockChatProvider::new("summary".to_string())),
        )
        .await
        .unwrap();

    // Check head is preserved
    assert_eq!(result.compacted_messages[0].content, "Message 0");
    assert_eq!(result.compacted_messages[1].content, "Message 1");

    // Check tail is preserved (last 2 before summary)
    let tail_start = result.compacted_messages.len() - 2;
    assert_eq!(result.compacted_messages[tail_start].content, "Message 18");
    assert_eq!(
        result.compacted_messages[tail_start + 1].content,
        "Message 19"
    );
}

#[tokio::test]
async fn test_age_based_strategy_returns_unchanged() {
    let compactor = Compactor::new(true);
    let messages = create_test_messages(20);

    // AgeBased requires timestamp info not available in StoredMessage,
    // so it should return messages unchanged
    let result = compactor
        .compact(
            CompactionRequest {
                messages,
                strategy: CompactionStrategy::AgeBased { max_age_days: 7 },
            },
            Arc::new(MockChatProvider::new("summary".to_string())),
        )
        .await
        .unwrap();

    // Should return unchanged
    assert_eq!(result.compacted_messages.len(), 20);
    assert_eq!(result.tokens_saved, 0);
}

#[tokio::test]
async fn test_budget_based_strategy_returns_unchanged() {
    let compactor = Compactor::new(true);
    let messages = create_test_messages(20);

    // BudgetBased is handled by apply_context_budget, compactor should pass through
    let result = compactor
        .compact(
            CompactionRequest {
                messages,
                strategy: CompactionStrategy::BudgetBased { max_tokens: 1000 },
            },
            Arc::new(MockChatProvider::new("summary".to_string())),
        )
        .await
        .unwrap();

    // Should return unchanged since BudgetBased is not implemented in compactor
    assert_eq!(result.compacted_messages.len(), 20);
    assert_eq!(result.tokens_saved, 0);
}

#[test]
fn test_compaction_strategy_variants() {
    let head_tail = CompactionStrategy::HeadTail {
        head_count: 3,
        tail_count: 3,
    };
    let age = CompactionStrategy::AgeBased { max_age_days: 7 };
    let budget = CompactionStrategy::BudgetBased { max_tokens: 5000 };

    // Verify debug formatting works
    assert!(format!("{:?}", head_tail).contains("HeadTail"));
    assert!(format!("{:?}", age).contains("AgeBased"));
    assert!(format!("{:?}", budget).contains("BudgetBased"));
}

#[test]
fn test_compaction_result_structure() {
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

#[test]
fn test_compactor_is_enabled() {
    let enabled_compactor = Compactor::new(true);
    let disabled_compactor = Compactor::new(false);

    assert!(enabled_compactor.is_enabled());
    assert!(!disabled_compactor.is_enabled());
}

#[tokio::test]
async fn test_compactor_with_empty_messages() {
    let compactor = Compactor::new(true);
    let messages = vec![];

    let result = compactor
        .compact(
            CompactionRequest {
                messages,
                strategy: CompactionStrategy::HeadTail {
                    head_count: 2,
                    tail_count: 2,
                },
            },
            Arc::new(MockChatProvider::new("summary".to_string())),
        )
        .await
        .unwrap();

    // Empty messages should return empty
    assert!(result.compacted_messages.is_empty());
    assert_eq!(result.tokens_saved, 0);
}

#[tokio::test]
async fn test_compactor_at_threshold() {
    let compactor = Compactor::new(true);
    // Exactly at threshold: 2 head + 2 tail = 4 messages
    let messages = create_test_messages(5);

    let result = compactor
        .compact(
            CompactionRequest {
                messages,
                strategy: CompactionStrategy::HeadTail {
                    head_count: 2,
                    tail_count: 2,
                },
            },
            Arc::new(MockChatProvider::new("summary".to_string())),
        )
        .await
        .unwrap();

    // 5 > 2 + 2, so compaction happens: 2 head + 1 summary + 2 tail = 5 messages
    // (middle is 1 message, which gets summarized)
    assert_eq!(result.compacted_messages.len(), 5);
}
