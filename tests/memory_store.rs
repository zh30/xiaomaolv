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

#[tokio::test]
async fn memory_store_persists_group_aliases_and_deduplicates() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");

    store
        .upsert_group_aliases(
            "telegram",
            -100001,
            &["小绿".to_string(), "xiaolv".to_string(), "小绿".to_string()],
        )
        .await
        .expect("upsert aliases");

    let aliases = store
        .load_group_aliases("telegram", -100001, 10)
        .await
        .expect("load aliases");

    assert_eq!(aliases.len(), 2);
    assert!(aliases.iter().any(|v| v == "小绿"));
    assert!(aliases.iter().any(|v| v == "xiaolv"));
}

#[tokio::test]
async fn memory_store_group_aliases_survive_reopen() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("alias-store.db");
    let url = format!("sqlite://{}", db_path.display());

    let store1 = SqliteMemoryStore::new(&url).await.expect("init store1");
    store1
        .upsert_group_aliases("telegram", -100777, &["阿绿".to_string()])
        .await
        .expect("upsert aliases");
    drop(store1);

    let store2 = SqliteMemoryStore::new(&url).await.expect("init store2");
    let aliases = store2
        .load_group_aliases("telegram", -100777, 10)
        .await
        .expect("load aliases");

    assert_eq!(aliases, vec!["阿绿".to_string()]);
}

#[tokio::test]
async fn memory_store_persists_group_user_profiles_and_survives_reopen() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("profile-store.db");
    let url = format!("sqlite://{}", db_path.display());

    let store1 = SqliteMemoryStore::new(&url).await.expect("init store1");
    store1
        .upsert_group_user_profile("telegram", -100333, 1001, "阿青", Some("aqing_99"))
        .await
        .expect("upsert profile");
    drop(store1);

    let store2 = SqliteMemoryStore::new(&url).await.expect("init store2");
    let profiles = store2
        .load_group_user_profiles("telegram", -100333, 20)
        .await
        .expect("load profiles");

    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].user_id, 1001);
    assert_eq!(profiles[0].preferred_name, "阿青");
    assert_eq!(profiles[0].username.as_deref(), Some("aqing_99"));
}
