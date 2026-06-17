use prometheus::Registry;
use xiaomaolv::harness::observability::TrajectoryMetrics;
use xiaomaolv::harness::run::{AgentRun, AgentRunExit, AgentRunStart};
use xiaomaolv::harness::trajectory::ToolCallRecord;
use xiaomaolv::memory::{SqliteMemoryBackend, SqliteMemoryStore};

#[tokio::test]
async fn agent_run_finishes_once_and_records_tool_call() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let backend = std::sync::Arc::new(SqliteMemoryBackend::new(store.clone()));
    let logger = xiaomaolv::harness::trajectory::TrajectoryLogger::new(backend, true);
    let metrics = TrajectoryMetrics::new(&Registry::new());

    let mut run = AgentRun::start(AgentRunStart {
        logger: Some(logger),
        metrics: Some(metrics),
        session_id: "session-run".to_string(),
        channel: "http".to_string(),
        user_id: "user-run".to_string(),
        model: "model-run".to_string(),
    })
    .await;

    run.record_tool_call(ToolCallRecord {
        call_index: 0,
        server: "s".to_string(),
        tool: "t".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"ok": true}),
        ok: true,
        duration_ms: 8,
        iteration: 0,
    })
    .await;

    run.finish(AgentRunExit::FinalAnswer("first".to_string()))
        .await;
    run.finish(AgentRunExit::InternalError).await;

    let record = store
        .get_trajectory(run.id())
        .await
        .expect("get trajectory")
        .expect("trajectory exists");

    assert_eq!(record.final_answer.as_deref(), Some("first"));
    assert_eq!(record.tool_calls.len(), 1);
    assert!(record.finished_at.is_some());
}
