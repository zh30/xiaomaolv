use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::harness::loop_engine::{
    ApproveGoalRequest, CreateGoalRequest, CreateSignalRequest, LoopEngine, PlanGoalRequest,
    PublishArtifactRequest, SignalTrust,
};

use super::{ApiError, AppState, check_rate_limit, constant_time_eq, verify_api_key};

const HARNESS_ACTOR_HEADER: &str = "x-harness-actor";

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/harness/goals", get(list_goals).post(post_goal))
        .route("/v1/harness/goals/{id}", get(get_goal))
        .route("/v1/harness/goals/{id}/events", get(get_goal_events))
        .route(
            "/v1/harness/goals/{id}/events/stream",
            get(get_goal_event_stream),
        )
        .route("/v1/harness/goals/{id}/plan", post(post_goal_plan))
        .route("/v1/harness/goals/{id}/approve", post(post_goal_approve))
        .route("/v1/harness/goals/{id}/resume", post(post_goal_resume))
        .route(
            "/v1/harness/goals/{id}/verify/manual",
            post(post_goal_manual_verification),
        )
        .route("/v1/harness/signals", get(list_signals).post(post_signal))
        .route("/v1/harness/signals/{id}", get(get_signal))
        .route(
            "/v1/harness/signals/{id}/propose-goal",
            post(post_signal_propose_goal),
        )
        .route("/v1/harness/self-tests/{suite}", post(post_self_test))
        .route("/v1/harness/self-test-runs/{id}", get(get_self_test_run))
        .route(
            "/v1/harness/trajectories/{id}/frames",
            get(get_trajectory_frames),
        )
        .route(
            "/v1/harness/trajectories/{id}/replay/structural",
            post(post_structural_replay),
        )
        .route(
            "/v1/harness/artifacts",
            get(list_artifacts).post(post_artifact),
        )
        .route("/v1/harness/artifacts/{id}", get(get_artifact))
}

#[derive(Debug, Deserialize)]
struct GoalHttpRequest {
    objective: String,
    #[serde(default)]
    source_signal_ids: Vec<String>,
    #[serde(default = "default_auto_plan")]
    auto_plan: bool,
}

fn default_auto_plan() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default = "default_list_limit")]
    limit: usize,
}

fn default_list_limit() -> usize {
    100
}

async fn list_goals(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    let goals = engine.list_goals(query.limit).await.map_err(internal)?;
    Ok(Json(serde_json::json!({"goals": goals})))
}

async fn post_goal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<GoalHttpRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                objective: request.objective,
                source_signal_ids: request.source_signal_ids,
            },
            &actor,
        )
        .await
        .map_err(bad_request)?;
    let value = if request.auto_plan {
        serde_json::to_value(
            engine
                .plan_goal_recommended(&goal.id, &actor)
                .await
                .map_err(conflict)?,
        )
    } else {
        serde_json::to_value(goal)
    }
    .map_err(|error| ApiError::Internal(error.into()))?;
    Ok(Json(value))
}

async fn get_goal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(goal_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    let goal = engine
        .get_goal(&goal_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError::NotFound(format!("goal not found: {goal_id}")))?;
    Ok(Json(
        serde_json::to_value(goal).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_event_limit")]
    limit: usize,
}

fn default_event_limit() -> usize {
    100
}

async fn get_goal_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(goal_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    if engine.get_goal(&goal_id).await.map_err(internal)?.is_none() {
        return Err(ApiError::NotFound(format!("goal not found: {goal_id}")));
    }
    let events = engine
        .list_goal_events(&goal_id, query.after, query.limit)
        .await
        .map_err(bad_request)?;
    let next_cursor = events.last().map_or(query.after, |event| event.sequence);
    Ok(Json(serde_json::json!({
        "events": events,
        "next_cursor": next_cursor,
    })))
}

struct GoalEventStreamState {
    engine: Arc<LoopEngine>,
    goal_id: String,
    cursor: i64,
    pending: VecDeque<crate::harness::loop_engine::LoopEventRecord>,
}

async fn get_goal_event_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(goal_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    if engine.get_goal(&goal_id).await.map_err(internal)?.is_none() {
        return Err(ApiError::NotFound(format!("goal not found: {goal_id}")));
    }
    let stream = futures::stream::unfold(
        GoalEventStreamState {
            engine,
            goal_id,
            cursor: query.after,
            pending: VecDeque::new(),
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    state.cursor = event.sequence;
                    let payload = serde_json::to_string(&event).unwrap_or_else(|_| {
                        "{\"error\":\"failed to serialize loop event\"}".to_string()
                    });
                    let sse = Event::default()
                        .id(event.sequence.to_string())
                        .event(event.event_type)
                        .data(payload);
                    return Some((Ok(sse), state));
                }
                match state
                    .engine
                    .list_goal_events(&state.goal_id, state.cursor, 100)
                    .await
                {
                    Ok(events) if !events.is_empty() => {
                        state.pending = events.into();
                    }
                    Ok(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                    Err(error) => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let sse = Event::default()
                            .event("harness.stream_error")
                            .data(serde_json::json!({"message": error.to_string()}).to_string());
                        return Some((Ok(sse), state));
                    }
                }
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("harness-keepalive"),
    ))
}

async fn post_goal_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(goal_id): Path<String>,
    Json(request): Json<PlanGoalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    let planned = engine
        .plan_goal(&goal_id, request, &actor)
        .await
        .map_err(conflict)?;
    Ok(Json(
        serde_json::to_value(planned).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn post_goal_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(goal_id): Path<String>,
    Json(request): Json<ApproveGoalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    let approved = engine
        .approve_goal(&goal_id, request, &actor)
        .await
        .map_err(conflict)?;
    Ok(Json(
        serde_json::to_value(approved).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn post_goal_resume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(goal_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    let resumed = engine
        .resume_goal(&goal_id, &actor)
        .await
        .map_err(|error| {
            if error.to_string().contains("not found") {
                ApiError::NotFound(error.to_string())
            } else {
                internal(error)
            }
        })?;
    Ok(Json(
        serde_json::to_value(resumed).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

#[derive(Debug, Deserialize)]
struct ManualVerificationRequest {
    label: String,
}

async fn post_goal_manual_verification(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(goal_id): Path<String>,
    Json(request): Json<ManualVerificationRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    engine
        .record_manual_verification(&goal_id, &request.label, &actor)
        .await
        .map_err(conflict)?;
    let report = engine
        .verify_goal(&goal_id, &actor)
        .await
        .map_err(conflict)?;
    Ok(Json(
        serde_json::to_value(report).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn post_signal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateSignalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = ingest_context(&state, &headers).await?;
    if request.trust == SignalTrust::Internal {
        return Err(ApiError::BadRequest(
            "scoped ingest clients cannot assert internal trust".to_string(),
        ));
    }
    let result = engine
        .ingest_signal(request, &actor)
        .await
        .map_err(bad_request)?;
    Ok(Json(
        serde_json::to_value(result).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn list_signals(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    let signals = engine.list_signals(query.limit).await.map_err(internal)?;
    Ok(Json(serde_json::json!({"signals": signals})))
}

async fn get_signal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(signal_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    let signal = engine
        .get_signal(&signal_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError::NotFound(format!("signal not found: {signal_id}")))?;
    Ok(Json(
        serde_json::to_value(signal).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

#[derive(Debug, Deserialize)]
struct ProposeSignalGoalRequest {
    objective: String,
}

async fn post_signal_propose_goal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(signal_id): Path<String>,
    Json(request): Json<ProposeSignalGoalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    let goal = engine
        .propose_goal_from_signal(&signal_id, &request.objective, &actor)
        .await
        .map_err(bad_request)?;
    Ok(Json(
        serde_json::to_value(goal).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn post_self_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(suite): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    let run = engine
        .run_self_tests(&suite, &actor)
        .await
        .map_err(bad_request)?;
    Ok(Json(
        serde_json::to_value(run).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn get_self_test_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    let run = engine
        .get_self_test_run(&run_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError::NotFound(format!("self-test run not found: {run_id}")))?;
    Ok(Json(
        serde_json::to_value(run).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn get_trajectory_frames(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(trajectory_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    let frames = engine
        .list_trajectory_frames(&trajectory_id)
        .await
        .map_err(internal)?;
    Ok(Json(serde_json::json!({"frames": frames})))
}

async fn post_structural_replay(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(trajectory_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    let run = engine
        .run_structural_replay(&trajectory_id, &actor)
        .await
        .map_err(bad_request)?;
    Ok(Json(
        serde_json::to_value(run).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn post_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PublishArtifactRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, actor) = operator_context(&state, &headers).await?;
    let result = engine
        .publish_artifact(request, &actor)
        .await
        .map_err(bad_request)?;
    Ok(Json(
        serde_json::to_value(result).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn list_artifacts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    let artifacts = engine.list_artifacts(query.limit).await.map_err(internal)?;
    Ok(Json(serde_json::json!({"artifacts": artifacts})))
}

async fn get_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(artifact_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (engine, _) = operator_context(&state, &headers).await?;
    let artifact = engine
        .get_artifact(&artifact_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError::NotFound(format!("artifact not found: {artifact_id}")))?;
    Ok(Json(
        serde_json::to_value(artifact).map_err(|error| ApiError::Internal(error.into()))?,
    ))
}

async fn operator_context(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Arc<LoopEngine>, String), ApiError> {
    let has_operator_key = state
        .api_key
        .read()
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("app api key lock poisoned")))?
        .is_some();
    if !has_operator_key {
        return Err(ApiError::Unauthorized(
            "loop engine control plane requires app.api_key".to_string(),
        ));
    }
    verify_api_key(state, headers)?;
    check_rate_limit(state, headers)?;
    let runtime = state.runtime.read().await;
    if !runtime.loop_engine_enabled {
        return Err(ApiError::NotFound("loop engine is disabled".to_string()));
    }
    Ok((
        runtime.loop_engine.clone(),
        actor_label(headers, "operator:http")?,
    ))
}

async fn ingest_context(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Arc<LoopEngine>, String), ApiError> {
    check_rate_limit(state, headers)?;
    let runtime = state.runtime.read().await;
    if !runtime.loop_engine_enabled {
        return Err(ApiError::NotFound("loop engine is disabled".to_string()));
    }
    let expected = runtime.loop_ingest_api_key.as_deref().ok_or_else(|| {
        ApiError::Unauthorized(
            "signal ingestion requires agent.harness.loop_engine.ingest_api_key".to_string(),
        )
    })?;
    let provided = bearer_token(headers).ok_or_else(|| {
        ApiError::Unauthorized("invalid or missing signal ingest key".to_string())
    })?;
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(ApiError::Unauthorized(
            "invalid or missing signal ingest key".to_string(),
        ));
    }
    Ok((
        runtime.loop_engine.clone(),
        actor_label(headers, "ingest:http")?,
    ))
}

fn actor_label(headers: &HeaderMap, fallback: &str) -> Result<String, ApiError> {
    let value = headers
        .get(HARNESS_ACTOR_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback);
    if value.len() > 160 || value.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(
            "x-harness-actor must be 1..=160 printable bytes".to_string(),
        ));
    }
    Ok(value.to_string())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let mut parts = header.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn bad_request(error: anyhow::Error) -> ApiError {
    ApiError::BadRequest(error.to_string())
}

fn conflict(error: anyhow::Error) -> ApiError {
    ApiError::Conflict(error.to_string())
}

fn internal(error: anyhow::Error) -> ApiError {
    ApiError::Internal(error)
}
