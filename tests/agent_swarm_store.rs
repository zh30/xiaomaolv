use xiaomaolv::memory::{
    AgentSwarmNodeExitStatus, AgentSwarmNodeState, AgentSwarmRunStatus, CreateAgentSwarmRunRequest,
    FinishAgentSwarmRunRequest, SqliteMemoryStore, UpsertAgentSwarmNodeRequest,
};

#[tokio::test]
async fn agent_swarm_store_create_list_tree_show_and_cleanup() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");

    store
        .create_agent_swarm_run(CreateAgentSwarmRunRequest {
            run_id: "run-1".to_string(),
            channel: "telegram".to_string(),
            session_id: "tg:100".to_string(),
            user_id: "42".to_string(),
            root_task: "做一个前后端联调方案".to_string(),
            status: AgentSwarmRunStatus::Running,
            started_at_unix: 1_800_000_000,
        })
        .await
        .expect("create run");

    store
        .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
            agent_id: "run-1:1".to_string(),
            run_id: "run-1".to_string(),
            channel: "telegram".to_string(),
            parent_agent_id: None,
            depth: 0,
            nickname: "Orchestrator".to_string(),
            role_name: "orchestrator".to_string(),
            role_definition: "主代理".to_string(),
            task: "拆分任务并汇总".to_string(),
            state: AgentSwarmNodeState::Running,
            exit_status: None,
            summary: None,
            error: None,
            started_at_unix: 1_800_000_000,
            finished_at_unix: None,
        })
        .await
        .expect("upsert root node running");

    store
        .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
            agent_id: "run-1:2".to_string(),
            run_id: "run-1".to_string(),
            channel: "telegram".to_string(),
            parent_agent_id: Some("run-1:1".to_string()),
            depth: 1,
            nickname: "FE-1".to_string(),
            role_name: "frontend".to_string(),
            role_definition: "前端负责人".to_string(),
            task: "输出页面结构".to_string(),
            state: AgentSwarmNodeState::Exited,
            exit_status: Some(AgentSwarmNodeExitStatus::Success),
            summary: Some("完成页面草图".to_string()),
            error: None,
            started_at_unix: 1_800_000_001,
            finished_at_unix: Some(1_800_000_005),
        })
        .await
        .expect("upsert child node exited");

    store
        .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
            agent_id: "run-1:1".to_string(),
            run_id: "run-1".to_string(),
            channel: "telegram".to_string(),
            parent_agent_id: None,
            depth: 0,
            nickname: "Orchestrator".to_string(),
            role_name: "orchestrator".to_string(),
            role_definition: "主代理".to_string(),
            task: "拆分任务并汇总".to_string(),
            state: AgentSwarmNodeState::Exited,
            exit_status: Some(AgentSwarmNodeExitStatus::Partial),
            summary: Some("已汇总可用子结果".to_string()),
            error: None,
            started_at_unix: 1_800_000_000,
            finished_at_unix: Some(1_800_000_010),
        })
        .await
        .expect("upsert root node exited");

    store
        .finish_agent_swarm_run(FinishAgentSwarmRunRequest {
            channel: "telegram".to_string(),
            run_id: "run-1".to_string(),
            status: AgentSwarmRunStatus::Partial,
            finished_at_unix: 1_800_000_010,
            error: None,
        })
        .await
        .expect("finish run");

    let runs = store
        .list_agent_swarm_runs("telegram", Some("42"), 10)
        .await
        .expect("list runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, "run-1");
    assert_eq!(runs[0].status, AgentSwarmRunStatus::Partial);

    let nodes = store
        .load_agent_swarm_nodes_by_run("telegram", "run-1")
        .await
        .expect("load run nodes");
    assert_eq!(nodes.len(), 2);
    assert!(nodes.iter().any(|n| n.nickname == "Orchestrator"));
    assert!(nodes.iter().any(|n| n.nickname == "FE-1"));

    let single = store
        .load_agent_swarm_node("telegram", "run-1:2")
        .await
        .expect("load single node")
        .expect("node exists");
    assert_eq!(single.role_name, "frontend");
    assert_eq!(single.exit_status, Some(AgentSwarmNodeExitStatus::Success));

    let deleted = store
        .cleanup_agent_swarm_audit("telegram", 1_800_000_011)
        .await
        .expect("cleanup");
    assert_eq!(deleted, 1);

    let left = store
        .list_agent_swarm_runs("telegram", Some("42"), 10)
        .await
        .expect("list after cleanup");
    assert!(left.is_empty());
}
