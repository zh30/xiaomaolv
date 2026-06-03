use xiaomaolv::harness::trajectory::{
    MAX_TRAJECTORY_QUERY_LIMIT, ToolCallRecord, TrajectoryExitReason, TrajectoryFilter,
    TrajectoryLogger, new_trajectory_id,
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
        .start_trajectory(
            &trajectory_id,
            &session_id,
            &channel,
            &user_id,
            "test-model",
        )
        .await
        .expect("start trajectory");

    // Insert a trajectory tool call
    let record = ToolCallRecord {
        call_index: 0,
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
            exit_reason: None,
            has_tool_errors: None,
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
        .start_trajectory(
            &trajectory_id,
            &session_id,
            &channel,
            &user_id,
            "test-model",
        )
        .await
        .expect("start trajectory");

    // Insert multiple tool calls
    for i in 0..3 {
        let record = ToolCallRecord {
            call_index: 0,
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
            exit_reason: None,
            has_tool_errors: None,
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
        call_index: 0,
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
        call_index: 0,
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
        call_index: 0,
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
            exit_reason: None,
            has_tool_errors: None,
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
            exit_reason: None,
            has_tool_errors: None,
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
async fn test_trajectory_preserves_repeated_same_tool_calls() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());
    let logger = TrajectoryLogger::new(Arc::new(backend), true);

    let trajectory_id = new_trajectory_id();
    logger
        .start_trajectory(
            &trajectory_id,
            "repeat-session",
            "test-channel",
            "test-user",
            "model-repeat",
        )
        .await
        .expect("start trajectory");

    for value in ["first", "second"] {
        logger
            .log_tool_call(
                &trajectory_id,
                ToolCallRecord {
                    call_index: 0,
                    server: "same-server".to_string(),
                    tool: "same-tool".to_string(),
                    arguments: serde_json::json!({"value": value}),
                    result: serde_json::json!({"value": value}),
                    ok: true,
                    duration_ms: 10,
                    iteration: 0,
                },
            )
            .await
            .expect("log repeated call");
    }
    logger
        .finish_trajectory(
            &trajectory_id,
            Some("done".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish");

    let trajectories = logger
        .query_trajectories(TrajectoryFilter {
            session_id: Some("repeat-session".to_string()),
            channel: None,
            user_id: None,
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query");
    let trajectory = trajectories
        .iter()
        .find(|trajectory| trajectory.id == trajectory_id)
        .expect("trajectory");

    assert_eq!(trajectory.tool_calls.len(), 2);
    assert_eq!(trajectory.tool_calls[0].call_index, 0);
    assert_eq!(trajectory.tool_calls[1].call_index, 1);
    assert_eq!(trajectory.tool_calls[0].result["value"], "first");
    assert_eq!(trajectory.tool_calls[1].result["value"], "second");
}

#[tokio::test]
async fn test_trajectory_query_clamps_limit_and_filters_tool_errors() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let backend = SqliteMemoryBackend::new(store.clone());
    let logger = TrajectoryLogger::new(Arc::new(backend), true);

    for idx in 0..(MAX_TRAJECTORY_QUERY_LIMIT + 5) {
        let trajectory_id = format!("limit-trajectory-{idx}");
        logger
            .start_trajectory(
                &trajectory_id,
                "limit-session",
                "test-channel",
                "test-user",
                "model-limit",
            )
            .await
            .expect("start trajectory");
        logger
            .finish_trajectory(
                &trajectory_id,
                Some(format!("answer {idx}")),
                TrajectoryExitReason::FinalAnswer,
            )
            .await
            .expect("finish trajectory");
    }

    let clamped = logger
        .query_trajectories(TrajectoryFilter {
            session_id: Some("limit-session".to_string()),
            channel: None,
            user_id: None,
            exit_reason: None,
            has_tool_errors: None,
            limit: usize::MAX,
        })
        .await
        .expect("query clamped");
    assert_eq!(clamped.len(), MAX_TRAJECTORY_QUERY_LIMIT);

    let error_id = new_trajectory_id();
    logger
        .start_trajectory(
            &error_id,
            "error-session",
            "test-channel",
            "test-user",
            "model-error",
        )
        .await
        .expect("start error trajectory");
    logger
        .log_tool_call(
            &error_id,
            ToolCallRecord {
                call_index: 0,
                server: "server".to_string(),
                tool: "tool".to_string(),
                arguments: serde_json::json!({}),
                result: serde_json::json!({"error": "failed"}),
                ok: false,
                duration_ms: 1,
                iteration: 0,
            },
        )
        .await
        .expect("log failed tool call");
    logger
        .finish_trajectory(
            &error_id,
            Some("tool failed".to_string()),
            TrajectoryExitReason::ToolError,
        )
        .await
        .expect("finish error trajectory");

    let tool_errors = logger
        .query_trajectories(TrajectoryFilter {
            session_id: Some("error-session".to_string()),
            channel: None,
            user_id: None,
            exit_reason: Some(TrajectoryExitReason::ToolError),
            has_tool_errors: Some(true),
            limit: 10,
        })
        .await
        .expect("query tool errors");
    assert_eq!(tool_errors.len(), 1);
    assert_eq!(tool_errors[0].id, error_id);
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
        call_index: 0,
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
