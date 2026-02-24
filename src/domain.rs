use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "assistant" => Self::Assistant,
            "system" => Self::System,
            _ => Self::User,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredMessage {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplyTarget {
    Telegram { chat_id: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncomingMessage {
    pub channel: String,
    pub session_id: String,
    pub user_id: String,
    pub text: String,
    pub reply_target: Option<ReplyTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub channel: String,
    pub session_id: String,
    pub text: String,
    pub reply_target: Option<ReplyTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionPayload {
    pub model: String,
    pub messages: Vec<StoredMessage>,
}
