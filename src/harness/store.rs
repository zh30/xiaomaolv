use async_trait::async_trait;

use crate::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryFilter, TrajectoryRecord,
};
use crate::memory::{
    CompactionSummaryLoadRequest, CompactionSummaryRecord, CompactionSummaryUpsertRequest,
    SqliteMemoryStore,
};

#[async_trait]
pub trait HarnessStore: Send + Sync {
    async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()>;

    async fn insert_trajectory_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()>;

    async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()>;

    async fn get_trajectory(&self, trajectory_id: &str)
    -> anyhow::Result<Option<TrajectoryRecord>>;

    async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>>;

    async fn load_compaction_summary(
        &self,
        req: CompactionSummaryLoadRequest,
    ) -> anyhow::Result<Option<CompactionSummaryRecord>>;

    async fn upsert_compaction_summary(
        &self,
        req: CompactionSummaryUpsertRequest,
    ) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct SqliteHarnessStore {
    store: SqliteMemoryStore,
}

impl SqliteHarnessStore {
    pub fn new(store: SqliteMemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl HarnessStore for SqliteHarnessStore {
    async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        self.store
            .start_trajectory(trajectory_id, session_id, channel, user_id, model)
            .await
    }

    async fn insert_trajectory_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()> {
        self.store
            .insert_trajectory_tool_call(trajectory_id, record)
            .await
    }

    async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()> {
        self.store
            .finish_trajectory(trajectory_id, final_answer, exit_reason)
            .await
    }

    async fn get_trajectory(
        &self,
        trajectory_id: &str,
    ) -> anyhow::Result<Option<TrajectoryRecord>> {
        self.store.get_trajectory(trajectory_id).await
    }

    async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>> {
        self.store.query_trajectories(filter).await
    }

    async fn load_compaction_summary(
        &self,
        req: CompactionSummaryLoadRequest,
    ) -> anyhow::Result<Option<CompactionSummaryRecord>> {
        self.store.load_compaction_summary(req).await
    }

    async fn upsert_compaction_summary(
        &self,
        req: CompactionSummaryUpsertRequest,
    ) -> anyhow::Result<()> {
        self.store.upsert_compaction_summary(req).await
    }
}
