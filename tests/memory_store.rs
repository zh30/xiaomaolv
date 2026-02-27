use chrono::{Datelike, Local, TimeZone};
use tempfile::tempdir;
use xiaomaolv::domain::{MessageRole, StoredMessage};
use xiaomaolv::memory::{
    MemoryBackend, MemoryChunkRecord, MemoryContextRequest, SqliteMemoryBackend, SqliteMemoryStore,
};

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

#[tokio::test]
async fn sqlite_memory_context_includes_time_dimension_for_yesterday_queries() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());

    let now = Local::now();
    let today = now.date_naive();
    let yesterday = today.pred_opt().expect("yesterday exists");

    let today_start = Local
        .with_ymd_and_hms(today.year(), today.month(), today.day(), 0, 0, 0)
        .single()
        .expect("today local timestamp");
    let yesterday_noon = Local
        .with_ymd_and_hms(
            yesterday.year(),
            yesterday.month(),
            yesterday.day(),
            12,
            0,
            0,
        )
        .single()
        .expect("yesterday local timestamp");

    store
        .append_chunk(MemoryChunkRecord {
            chunk_id: "mem-yesterday".to_string(),
            session_id: "session-y".to_string(),
            user_id: "u-y".to_string(),
            channel: "telegram".to_string(),
            role: MessageRole::User,
            text: "昨天我整理了项目计划和接口文档".to_string(),
            created_at: yesterday_noon.timestamp(),
        })
        .await
        .expect("append yesterday chunk");

    store
        .append_chunk(MemoryChunkRecord {
            chunk_id: "mem-today".to_string(),
            session_id: "session-y".to_string(),
            user_id: "u-y".to_string(),
            channel: "telegram".to_string(),
            role: MessageRole::User,
            text: "今天我整理了项目计划并修复构建".to_string(),
            created_at: today_start.timestamp(),
        })
        .await
        .expect("append today chunk");

    let context = backend
        .load_context(MemoryContextRequest {
            session_id: "session-y".to_string(),
            user_id: "u-y".to_string(),
            channel: "telegram".to_string(),
            query_text: "我昨天整理了什么".to_string(),
            max_recent_turns: 8,
            max_semantic_memories: 1,
            semantic_lookback_days: 30,
        })
        .await
        .expect("load context");

    assert_eq!(context.len(), 1, "expect one injected memory snippet");
    let content = &context[0].content;
    assert!(
        content.contains("昨天我整理了项目计划和接口文档"),
        "expected yesterday memory in snippet, got: {content}"
    );
    assert!(
        !content.contains("今天我整理了项目计划并修复构建"),
        "today memory should not win for yesterday query, got: {content}"
    );
    assert!(
        content.contains("at="),
        "memory snippet should carry explicit time dimension, got: {content}"
    );
}
