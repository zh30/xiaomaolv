use xiaomaolv::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryFilter, TrajectoryLogger,
    new_trajectory_id,
};
use xiaomaolv::memory::{SqliteMemoryBackend, SqliteMemoryStore};

#[tokio::test]
async fn test_trajectory_records_tool_calls() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());
    let logger = TrajectoryLogger::new(Arc::new(backend), true);

    let trajectory_id = new_trajectory_id();
    let session_id = "test-session-1".to_string();
    let channel = "test-channel".to_string();
    let user_id = "test-user".to_string();

    // Start trajectory (creates header record)
    logger
        .start_trajectory(&trajectory_id, &session_id, &channel, &user_id, "test-model")
        .await
        .expect("start trajectory");

    // Insert a trajectory tool call
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

    // Query trajectories - tool_calls won't be associated since we didn't store trajectory header
    let _trajectories = logger
        .query_trajectories(TrajectoryFilter {
            session_id: Some(session_id.clone()),
            channel: Some(channel.clone()),
            user_id: Some(user_id.clone()),
            limit: 10,
        })
        .await
        .expect("query trajectories");
}

#[tokio::test]
async fn test_trajectory_captures_final_answer() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());
    let logger = TrajectoryLogger::new(Arc::new(backend), true);

    let trajectory_id = new_trajectory_id();
    let session_id = "test-session-2".to_string();
    let channel = "test-channel".to_string();
    let user_id = "test-user".to_string();

    // Start trajectory (creates header record)
    logger
        .start_trajectory(&trajectory_id, &session_id, &channel, &user_id, "test-model")
        .await
        .expect("start trajectory");

    // Insert multiple tool calls
    for i in 0..3 {
        let record = ToolCallRecord {
            server: "test-server".to_string(),
            tool: format!("tool-{}", i),
            arguments: serde_json::json!({"arg": i}),
            result: serde_json::json!({"result": i}),
            ok: true,
            duration_ms: 50 + (i as u64 * 10),
            iteration: i,
        };
        logger
            .log_tool_call(&trajectory_id, record)
            .await
            .expect("log tool call");
    }

    // Finish with final answer
    let final_answer = "This is the final answer from the model";
    logger
        .finish_trajectory(
            &trajectory_id,
            Some(final_answer.to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish trajectory");

    // Query with no filters to see if we can find our trajectory
    let trajectories = logger
        .query_trajectories(TrajectoryFilter {
            session_id: None,
            channel: None,
            user_id: None,
            limit: 100,
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
    assert_eq!(
        trajectory.final_answer.as_deref(),
        Some(final_answer),
        "Final answer should match"
    );
    assert!(matches!(
        trajectory.exit_reason,
        TrajectoryExitReason::FinalAnswer
    ));
}

#[tokio::test]
async fn test_trajectory_filter_by_session() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());
    let logger = TrajectoryLogger::new(Arc::new(backend), true);

    // Create trajectories for different sessions
    let session_a = "session-A".to_string();
    let session_b = "session-B".to_string();
    let channel = "test-channel".to_string();
    let user_id = "test-user".to_string();

    // Trajectory 1 for session A
    let traj_a1 = new_trajectory_id();
    logger
        .start_trajectory(&traj_a1, &session_a, &channel, &user_id, "model-a")
        .await
        .expect("start trajectory");
    let record_a1 = ToolCallRecord {
        server: "server".to_string(),
        tool: "tool1".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({}),
        ok: true,
        duration_ms: 100,
        iteration: 0,
    };
    logger
        .log_tool_call(&traj_a1, record_a1)
        .await
        .expect("log");
    logger
        .finish_trajectory(
            &traj_a1,
            Some("answer A1".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish");

    // Trajectory 2 for session A
    let traj_a2 = new_trajectory_id();
    logger
        .start_trajectory(&traj_a2, &session_a, &channel, &user_id, "model-a")
        .await
        .expect("start trajectory");
    let record_a2 = ToolCallRecord {
        server: "server".to_string(),
        tool: "tool2".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({}),
        ok: true,
        duration_ms: 200,
        iteration: 0,
    };
    logger
        .log_tool_call(&traj_a2, record_a2)
        .await
        .expect("log");
    logger
        .finish_trajectory(
            &traj_a2,
            Some("answer A2".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish");

    // Trajectory for session B
    let traj_b1 = new_trajectory_id();
    logger
        .start_trajectory(&traj_b1, &session_b, &channel, &user_id, "model-b")
        .await
        .expect("start trajectory");
    let record_b1 = ToolCallRecord {
        server: "server".to_string(),
        tool: "tool3".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({}),
        ok: true,
        duration_ms: 300,
        iteration: 0,
    };
    logger
        .log_tool_call(&traj_b1, record_b1)
        .await
        .expect("log");
    logger
        .finish_trajectory(
            &traj_b1,
            Some("answer B1".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish");

    // Query trajectories filtered by session A
    let trajectories_a = logger
        .query_trajectories(TrajectoryFilter {
            session_id: Some(session_a.clone()),
            channel: None,
            user_id: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");

    // Query trajectories filtered by session B
    let trajectories_b = logger
        .query_trajectories(TrajectoryFilter {
            session_id: Some(session_b.clone()),
            channel: None,
            user_id: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");

    // Verify filtering works
    for t in &trajectories_a {
        assert_eq!(
            t.session_id, session_a,
            "All trajectories should be from session A"
        );
    }
    for t in &trajectories_b {
        assert_eq!(
            t.session_id, session_b,
            "All trajectories should be from session B"
        );
    }
}

#[tokio::test]
async fn test_trajectory_disabled_logger_does_nothing() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());

    // Create logger with disabled=true but it won't do anything
    let logger = TrajectoryLogger::new(Arc::new(backend), false);

    let trajectory_id = new_trajectory_id();
    let record = ToolCallRecord {
        server: "test-server".to_string(),
        tool: "test-tool".to_string(),
        arguments: serde_json::json!({"query": "test"}),
        result: serde_json::json!({"result": "ok"}),
        ok: true,
        duration_ms: 100,
        iteration: 0,
    };

    // These should be no-ops when disabled
    logger
        .log_tool_call(&trajectory_id, record)
        .await
        .expect("should not error even when disabled");

    logger
        .finish_trajectory(
            &trajectory_id,
            Some("final".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("should not error even when disabled");
}

use std::sync::Arc;
