use tempfile::tempdir;
use xiaomaolv::domain::{MessageRole, StoredMessage};
use xiaomaolv::memory::SqliteMemoryStore;

#[tokio::test]
async fn memory_store_persists_and_loads_ordered_messages() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");

    store
        .append(
            "session-1",
            StoredMessage {
                role: MessageRole::User,
                content: "hello".to_string(),
            },
        )
        .await
        .expect("append user");

    store
        .append(
            "session-1",
            StoredMessage {
                role: MessageRole::Assistant,
                content: "hi".to_string(),
            },
        )
        .await
        .expect("append assistant");

    let history = store
        .load_recent("session-1", 10)
        .await
        .expect("load history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content, "hello");
    assert_eq!(history[1].content, "hi");
}

#[tokio::test]
async fn memory_store_creates_missing_sqlite_file() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("new-store.db");
    let url = format!("sqlite://{}", db_path.display());

    let store = SqliteMemoryStore::new(&url).await.expect("init store");

    store
        .append(
            "session-create",
            StoredMessage {
                role: MessageRole::User,
                content: "hello".to_string(),
            },
        )
        .await
        .expect("append");

    let history = store
        .load_recent("session-create", 10)
        .await
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].content, "hello");
}
