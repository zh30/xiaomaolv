use super::*;

impl MessageService {
    pub async fn observe(&self, incoming: IncomingMessage) -> anyhow::Result<()> {
        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id,
                user_id: incoming.user_id,
                channel: incoming.channel,
                message: StoredMessage {
                    role: MessageRole::User,
                    content: incoming.text,
                },
            })
            .await
            .context("failed to persist observed user message")
    }

    pub async fn upsert_group_aliases(
        &self,
        channel: String,
        chat_id: i64,
        aliases: Vec<String>,
    ) -> anyhow::Result<()> {
        if aliases.is_empty() {
            return Ok(());
        }
        self.memory
            .upsert_group_aliases(GroupAliasUpsertRequest {
                channel,
                chat_id,
                aliases,
            })
            .await
            .context("failed to persist telegram group aliases")
    }

    pub async fn load_group_aliases(
        &self,
        channel: String,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        self.memory
            .load_group_aliases(GroupAliasLoadRequest {
                channel,
                chat_id,
                limit,
            })
            .await
            .context("failed to load telegram group aliases")
    }

    pub async fn upsert_group_user_profile(
        &self,
        channel: String,
        chat_id: i64,
        user_id: i64,
        preferred_name: String,
        username: Option<String>,
    ) -> anyhow::Result<()> {
        if preferred_name.trim().is_empty() {
            return Ok(());
        }
        self.memory
            .upsert_group_user_profile(GroupUserProfileUpsertRequest {
                channel,
                chat_id,
                user_id,
                preferred_name,
                username,
            })
            .await
            .context("failed to persist telegram group user profile")
    }

    pub async fn load_group_user_profiles(
        &self,
        channel: String,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        self.memory
            .load_group_user_profiles(GroupUserProfileLoadRequest {
                channel,
                chat_id,
                limit,
            })
            .await
            .context("failed to load telegram group user profiles")
    }

    pub async fn create_telegram_scheduler_job(
        &self,
        req: CreateTelegramSchedulerJobRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .create_telegram_scheduler_job(req)
            .await
            .context("failed to create telegram scheduler job")
    }

    pub async fn list_telegram_scheduler_jobs_by_owner(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.memory
            .list_telegram_scheduler_jobs_by_owner(TelegramSchedulerJobListRequest {
                channel,
                chat_id,
                owner_user_id,
                limit,
            })
            .await
            .context("failed to list telegram scheduler jobs")
    }

    pub async fn query_telegram_scheduler_stats(
        &self,
        channel: String,
        now_unix: i64,
    ) -> anyhow::Result<TelegramSchedulerStats> {
        self.memory
            .query_telegram_scheduler_stats(TelegramSchedulerStatsRequest { channel, now_unix })
            .await
            .context("failed to query telegram scheduler stats")
    }

    pub async fn load_telegram_scheduler_job(
        &self,
        channel: String,
        job_id: String,
    ) -> anyhow::Result<Option<TelegramSchedulerJobRecord>> {
        self.memory
            .load_telegram_scheduler_job(channel, job_id)
            .await
            .context("failed to load telegram scheduler job")
    }

    pub async fn update_telegram_scheduler_job_status(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        job_id: String,
        status: TelegramSchedulerJobStatus,
    ) -> anyhow::Result<bool> {
        self.memory
            .update_telegram_scheduler_job_status(UpdateTelegramSchedulerJobStatusRequest {
                channel,
                chat_id,
                owner_user_id,
                job_id,
                status,
            })
            .await
            .context("failed to update telegram scheduler job status")
    }

    pub async fn claim_due_telegram_scheduler_jobs(
        &self,
        channel: String,
        now_unix: i64,
        limit: usize,
        lease_secs: i64,
        lease_token: String,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.memory
            .claim_due_telegram_scheduler_jobs(ClaimDueTelegramSchedulerJobsRequest {
                channel,
                now_unix,
                limit,
                lease_secs,
                lease_token,
            })
            .await
            .context("failed to claim due telegram scheduler jobs")
    }

    pub async fn complete_telegram_scheduler_job_run(
        &self,
        req: CompleteTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .complete_telegram_scheduler_job_run(req)
            .await
            .context("failed to complete telegram scheduler job run")
    }

    pub async fn fail_telegram_scheduler_job_run(
        &self,
        req: FailTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .fail_telegram_scheduler_job_run(req)
            .await
            .context("failed to fail telegram scheduler job run")
    }

    pub async fn upsert_telegram_scheduler_pending_intent(
        &self,
        req: UpsertTelegramSchedulerPendingIntentRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .upsert_telegram_scheduler_pending_intent(req)
            .await
            .context("failed to upsert telegram scheduler pending intent")
    }

    pub async fn load_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        now_unix: i64,
    ) -> anyhow::Result<Option<TelegramSchedulerPendingIntentRecord>> {
        self.memory
            .load_telegram_scheduler_pending_intent(channel, chat_id, owner_user_id, now_unix)
            .await
            .context("failed to load telegram scheduler pending intent")
    }

    pub async fn delete_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
    ) -> anyhow::Result<bool> {
        self.memory
            .delete_telegram_scheduler_pending_intent(channel, chat_id, owner_user_id)
            .await
            .context("failed to delete telegram scheduler pending intent")
    }

    pub async fn get_trajectory_detail(
        &self,
        trajectory_id: String,
    ) -> anyhow::Result<Option<crate::harness::trajectory::TrajectoryRecord>> {
        let Some(store) = &self.harness_store else {
            return Ok(None);
        };
        store.get_trajectory(&trajectory_id).await
    }

    pub async fn query_trajectories(
        &self,
        filter: crate::harness::trajectory::TrajectoryFilter,
    ) -> anyhow::Result<Vec<crate::harness::trajectory::TrajectoryRecord>> {
        let Some(store) = &self.harness_store else {
            return Ok(Vec::new());
        };
        store.query_trajectories(filter).await
    }

    pub async fn get_run_scorecard(
        &self,
        trajectory_id: &str,
    ) -> anyhow::Result<Option<crate::harness::trajectory::RunScorecard>> {
        let Some(store) = &self.harness_store else {
            return Ok(None);
        };
        store.get_run_scorecard(trajectory_id).await
    }

    pub async fn query_run_scorecards(
        &self,
        filter: crate::harness::trajectory::RunScorecardFilter,
    ) -> anyhow::Result<Vec<crate::harness::trajectory::RunScorecard>> {
        let Some(store) = &self.harness_store else {
            return Ok(Vec::new());
        };
        store.query_run_scorecards(filter).await
    }

    pub async fn run_scorecard_summary(
        &self,
        since_unix: i64,
        until_unix: i64,
    ) -> anyhow::Result<crate::harness::trajectory::ScorecardSummary> {
        let Some(store) = &self.harness_store else {
            return Ok(crate::harness::trajectory::ScorecardSummary {
                window_start_unix: since_unix,
                window_end_unix: until_unix,
                run_count: 0,
                mean_score: None,
                low_count: 0,
                mid_count: 0,
                high_count: 0,
                failed_tool_calls: 0,
                verification_failures: 0,
                exit_reasons: std::collections::BTreeMap::new(),
            });
        };
        store.run_scorecard_summary(since_unix, until_unix).await
    }

    /// Max age of the run a lightweight reply-marker may attach to.
    const REPLY_FEEDBACK_MAX_AGE_SECS: i64 = 2 * 3_600;

    /// Binds a channel reply-marker to the most recent finished run under
    /// `session_prefix` (e.g. `tg:{chat_id}`) and records it as trajectory
    /// feedback the proposal engine consumes. Returns `false` when the
    /// feature is unwired or no recent run exists — callers then treat the
    /// marker as a normal message.
    pub async fn record_evolution_reply_feedback(
        &self,
        session_prefix: &str,
        channel: &str,
        user_id: &str,
        score: f64,
        comment: Option<String>,
    ) -> anyhow::Result<bool> {
        let (Some(evolution_store), Some(harness_store)) =
            (self.evolution_store.as_ref(), self.harness_store.as_ref())
        else {
            return Ok(false);
        };
        let min_finished_unix = chrono::Utc::now().timestamp() - Self::REPLY_FEEDBACK_MAX_AGE_SECS;
        let Some(trajectory) = harness_store
            .latest_finished_trajectory(session_prefix, min_finished_unix)
            .await?
        else {
            return Ok(false);
        };
        if trajectory.channel != channel {
            return Ok(false);
        }
        evolution_store
            .record_feedback(
                crate::harness::evolution::EvolutionFeedbackDraft {
                    trajectory_id: trajectory.id.clone(),
                    score,
                    tags: vec!["telegram-reply".to_string()],
                    comment: comment.map(|text| {
                        let mut chars = text.chars();
                        let mut clipped: String = chars.by_ref().take(200).collect();
                        if chars.next().is_some() {
                            clipped.push('…');
                        }
                        clipped
                    }),
                },
                &format!("telegram:{user_id}"),
            )
            .await?;
        info!(
            trajectory_id = %trajectory.id,
            score,
            "recorded user reply-marker as trajectory feedback"
        );
        Ok(true)
    }

    /// Durable Telegram update admission: `true` when the caller owns this
    /// `update_id` (fresh insert, or a lease-expired in-flight row reclaimed
    /// under a bounded new attempt). `false` marks a duplicate delivery that
    /// must not be processed again. When no admission store is wired the
    /// gate is fail-open — updates always process.
    pub async fn admit_telegram_update(
        &self,
        update_id: i64,
        kind: &str,
        summary_json: &str,
        lease_secs: i64,
    ) -> anyhow::Result<bool> {
        match self.telegram_admission_store.as_ref() {
            Some(store) => store.admit(update_id, kind, summary_json, lease_secs).await,
            None => Ok(true),
        }
    }

    pub async fn finish_telegram_admission(
        &self,
        update_id: i64,
        detail: &str,
    ) -> anyhow::Result<()> {
        if let Some(store) = self.telegram_admission_store.as_ref() {
            store.finish(update_id, detail).await?;
        }
        Ok(())
    }

    pub async fn fail_telegram_admission(&self, update_id: i64, error: &str) -> anyhow::Result<()> {
        if let Some(store) = self.telegram_admission_store.as_ref() {
            store.fail(update_id, error).await?;
        }
        Ok(())
    }

    /// Marks long-dead in-flight admissions as failed and returns their ids
    /// so the channel can surface the loss.
    pub async fn sweep_stale_telegram_admissions(
        &self,
        older_than_secs: i64,
    ) -> anyhow::Result<Vec<i64>> {
        match self.telegram_admission_store.as_ref() {
            Some(store) => store.sweep_stale(older_than_secs).await,
            None => Ok(Vec::new()),
        }
    }

    pub async fn detect_telegram_scheduler_intent(
        &self,
        text: &str,
        timezone: &str,
        now_unix: i64,
        pending_draft_json: Option<&str>,
    ) -> anyhow::Result<Option<TelegramSchedulerIntent>> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }

        let mut input =
            format!("当前时间戳(now_unix): {now_unix}\n默认时区: {timezone}\n用户输入: {text}");
        if let Some(draft) = pending_draft_json
            && !draft.trim().is_empty()
        {
            input.push_str("\n待确认草案(JSON): ");
            input.push_str(draft.trim());
        }

        let reply = self
            .provider
            .complete(CompletionRequest::json(vec![
                StoredMessage {
                    role: MessageRole::System,
                    content: [
                        "你是 Telegram 定时任务意图解析器。",
                        "只输出 JSON，不要 Markdown，不要解释。",
                        "JSON schema:",
                        "{\"action\":\"create|update|delete|pause|resume|cancel|list|none\",\"confidence\":0..1,\"task_kind\":\"reminder|agent|null\",\"payload\":\"string|null\",\"schedule_kind\":\"once|cron|null\",\"run_at\":\"RFC3339或unix秒字符串|null\",\"cron_expr\":\"string|null\",\"timezone\":\"IANA时区或null\",\"job_id\":\"string|null\",\"job_operation\":\"delete|pause|resume|null\"}",
                        "规则:",
                        "1) 若不是定时任务意图，action=none, confidence<=0.4",
                        "2) 若有明确时间并要求提醒，优先 action=create",
                        "3) 对修改已存在草案可输出 action=update",
                        "4) 若表达暂停/恢复/删除已存在任务，输出 action=pause|resume|delete，并尽量给出 job_id",
                        "4) 只返回一个合法 JSON 对象",
                        ]
                        .join("\n"),
                    },
                    StoredMessage {
                        role: MessageRole::User,
                        content: input,
                    },
                ]))
            .await
            .context("failed to detect telegram scheduler intent")?;

        Ok(parse_scheduler_intent_json(&reply))
    }
}

pub(super) fn parse_scheduler_intent_json(reply: &str) -> Option<TelegramSchedulerIntent> {
    let json_text = extract_json_payload(reply.trim())?;
    let mut parsed: TelegramSchedulerIntent = serde_json::from_str(&json_text).ok()?;
    parsed.action = parsed.action.trim().to_ascii_lowercase();
    parsed.confidence = parsed.confidence.clamp(0.0, 1.0);
    parsed.task_kind = parsed
        .task_kind
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    parsed.schedule_kind = parsed
        .schedule_kind
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    parsed.payload = parsed
        .payload
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.run_at = parsed
        .run_at
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.cron_expr = parsed
        .cron_expr
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.timezone = parsed
        .timezone
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.job_id = parsed
        .job_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.job_operation = parsed
        .job_operation
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    Some(parsed)
}
