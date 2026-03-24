use prometheus::Registry;
use xiaomaolv::harness::observability::TrajectoryMetrics;
use xiaomaolv::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryFilter, TrajectoryLogger, new_trajectory_id,
};
use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};

#[tokio::test]
async fn test_trajectory_endpoint_returns_results() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());
    let logger = TrajectoryLogger::new(std::sync::Arc::new(backend), true);

    let trajectory_id = new_trajectory_id();
    let session_id = "test-session-observe".to_string();
    let channel = "test-channel".to_string();
    let user_id = "test-user".to_string();

    // Start trajectory
    logger
        .start_trajectory(
            &trajectory_id,
            &session_id,
            &channel,
            &user_id,
            "test-model",
        )
        .await
        .expect("start trajectory");

    // Insert a tool call
    let record = ToolCallRecord {
        server: "test-server".to_string(),
        tool: "test-tool".to_string(),
        arguments: serde_json::json!({"query": "test"}),
        result: serde_json::json!({"result": "ok"}),
        ok: true,
        duration_ms: 100,
        iteration: 0,
    };

    logger
        .log_tool_call(&trajectory_id, record)
        .await
        .expect("log tool call");

    // Finish the trajectory
    logger
        .finish_trajectory(
            &trajectory_id,
            Some("final answer".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish trajectory");

    // Query trajectories - should find our trajectory
    let trajectories = logger
        .query_trajectories(TrajectoryFilter {
            session_id: Some(session_id.clone()),
            channel: Some(channel.clone()),
            user_id: Some(user_id.clone()),
            limit: 10,
        })
        .await
        .expect("query trajectories");

    // Find our trajectory
    let our_trajectory = trajectories.iter().find(|t| t.id == trajectory_id);
    assert!(
        our_trajectory.is_some(),
        "Should find the trajectory we created"
    );

    let trajectory = our_trajectory.unwrap();
    assert_eq!(trajectory.channel, channel);
    assert_eq!(trajectory.session_id, session_id);
    assert_eq!(trajectory.user_id, user_id);
    assert!(trajectory.final_answer.is_some());
    assert!(matches!(
        trajectory.exit_reason,
        TrajectoryExitReason::FinalAnswer
    ));
}

#[tokio::test]
async fn test_trajectory_get_by_id() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());

    let trajectory_id = new_trajectory_id();
    let session_id = "test-session-get".to_string();
    let channel = "test-channel".to_string();
    let user_id = "test-user".to_string();

    // Insert trajectory directly via store
    store
        .start_trajectory(
            &trajectory_id,
            &session_id,
            &channel,
            &user_id,
            "test-model",
        )
        .await
        .expect("start trajectory");

    // Get trajectory by id
    let trajectory = backend
        .get_trajectory(&trajectory_id)
        .await
        .expect("get trajectory")
        .expect("trajectory should exist");

    assert_eq!(trajectory.id, trajectory_id);
    assert_eq!(trajectory.session_id, session_id);
    assert_eq!(trajectory.channel, channel);
}

#[tokio::test]
async fn test_trajectory_metrics_record() {
    let registry = Registry::new();
    let metrics = TrajectoryMetrics::new(&registry);

    // Record some trajectories
    metrics.record_trajectory(1.0, 2, 3);
    metrics.record_trajectory(2.0, 4, 6);

    assert_eq!(metrics.trajectories_total.get(), 2);
    // Average iterations: (2 + 4) / 2 = 3.0
    assert_eq!(metrics.avg_iterations_per_trajectory.get(), 3.0);
}

#[tokio::test]
async fn test_trajectory_metrics_tool_calls() {
    let registry = Registry::new();
    let metrics = TrajectoryMetrics::new(&registry);

    metrics.record_tool_call(100, "server-a", "tool-x", true);
    metrics.record_tool_call(200, "server-a", "tool-x", true);
    metrics.record_tool_call(150, "server-b", "tool-y", false);

    assert_eq!(
        metrics
            .tool_calls_total
            .with_label_values(&["server-a", "tool-x"])
            .get(),
        2
    );
    assert_eq!(
        metrics
            .tool_calls_total
            .with_label_values(&["server-b", "tool-y"])
            .get(),
        1
    );
}

#[tokio::test]
async fn test_trajectory_not_found_returns_none() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store);

    let result = backend.get_trajectory("non-existent-id").await;
    assert!(result.expect("query should succeed").is_none());
}
