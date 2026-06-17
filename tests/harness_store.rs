use xiaomaolv::harness::trajectory::{ToolCallRecord, TrajectoryExitReason, TrajectoryFilter};
use xiaomaolv::memory::SqliteMemoryStore;

#[tokio::test]
async fn trajectory_store_preserves_explicit_call_index() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    store
        .start_trajectory("traj-index", "session-a", "http", "user-a", "model-a")
        .await
        .expect("start trajectory");

    let first = ToolCallRecord {
        call_index: 7,
        server: "server-a".to_string(),
        tool: "tool-a".to_string(),
        arguments: serde_json::json!({ "q": "first" }),
        result: serde_json::json!({ "ok": true }),
        ok: true,
        duration_ms: 12,
        iteration: 0,
    };
    let second = ToolCallRecord {
        call_index: 3,
        server: "server-a".to_string(),
        tool: "tool-a".to_string(),
        arguments: serde_json::json!({ "q": "second" }),
        result: serde_json::json!({ "ok": true }),
        ok: true,
        duration_ms: 15,
        iteration: 0,
    };

    store
        .insert_trajectory_tool_call("traj-index", first)
        .await
        .expect("insert first");
    store
        .insert_trajectory_tool_call("traj-index", second)
        .await
        .expect("insert second");
    store
        .finish_trajectory(
            "traj-index",
            Some("done".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish trajectory");

    let records = store
        .query_trajectories(TrajectoryFilter {
            session_id: Some("session-a".to_string()),
            channel: Some("http".to_string()),
            user_id: None,
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");

    let calls = &records[0].tool_calls;
    assert_eq!(
        calls.iter().map(|c| c.call_index).collect::<Vec<_>>(),
        vec![3, 7]
    );
    assert_eq!(calls[0].arguments["q"], "second");
    assert_eq!(calls[1].arguments["q"], "first");
}
