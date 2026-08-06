//! HTTP surface: REST for actions, SSE for live run events.

use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use boitata_agent::{Agent, Task};
use boitata_core::audit::AuditSink;
use boitata_orchestrator::{Executor, SqliteCheckpointer};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

use crate::events::ChannelAuditSink;
use crate::state::{AppState, RunHandle, RunResult, RunStatus, RunSummary};

pub fn router(state: AppState) -> Router {
    // Only the `/api` surface is gated by the token; it's the part that can drive
    // the shell/file/git agent. The embedded web UI (the SPA shell and its hashed
    // asset bundle) carries no secrets and must load in a plain browser — a
    // navigation can't send an `Authorization` header — so serving it behind the
    // gate would 401 the very UI that binding non-loopback exists to expose. The
    // UI's own `/api` calls still require the token.
    let api = Router::new()
        .route("/api/runs", post(create_run).get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/blueprints", get(list_blueprints))
        .route("/api/blueprints/{name}", get(get_blueprint))
        .route("/api/blueprints/{name}/source", get(get_blueprint_source))
        // Cap request bodies so a client can't OOM the server by streaming a
        // multi-GB JSON payload (axum 0.8 applies no default limit).
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // When an API token is configured, every `/api` request must carry it —
        // gating the shell/file/git agent behind a shared secret (see
        // `require_token`).
        .layer(middleware::from_fn_with_state(state.clone(), require_token));

    api.fallback(crate::assets::static_handler)
        .with_state(state)
}

/// Largest accepted request body. A run task is plain text; 1 MiB is generous
/// and bounds memory under a hostile client.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Auth gate: when an API token is configured (`api_token` / `BOITATA_API_TOKEN`),
/// every request must carry it as `Authorization: Bearer <token>`, or as
/// `?token=<token>` (so browser `EventSource` streams, which can't set headers,
/// can authenticate). With no token configured the API is open — intended for
/// loopback/single-user use. The comparison is constant-time to avoid a timing
/// oracle on the shared secret.
async fn require_token(State(app): State<AppState>, request: Request, next: Next) -> Response {
    let Some(expected) = app.config.resolve_api_token() else {
        return next.run(request).await;
    };
    let header_token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer ").map(str::trim));
    let query_token = request.uri().query().and_then(token_from_query);
    let provided = header_token.or(query_token);
    match provided {
        Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => next.run(request).await,
        _ => ApiError::unauthorized().into_response(),
    }
}

/// Extract the `token=<value>` query parameter (if present and non-empty).
fn token_from_query(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token=").filter(|v| !v.is_empty()))
}

/// Byte-wise compare that does not short-circuit, so the time taken is
/// independent of how many leading bytes match.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Request body for starting a run. `blueprint` is the *name* of one of the
/// server's configured blueprints (see `--blueprints-dir`); omit it to run the
/// single-agent path. An unknown name — or any name when none are configured — is
/// rejected. Only these vetted names are accepted, never a filesystem path.
#[derive(Debug, Deserialize)]
struct StartRun {
    task: String,
    #[serde(default)]
    blueprint: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunDetail {
    #[serde(flatten)]
    summary: RunSummary,
    result: Option<RunResult>,
    events: Vec<crate::events::RunEvent>,
}

/// `POST /api/runs` — validate, register the run, spawn it, return `{ id }`.
async fn create_run(
    State(app): State<AppState>,
    Json(req): Json<StartRun>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if req.task.trim().is_empty() {
        return Err(ApiError::bad_request("task must not be empty"));
    }
    // A blueprint is referenced by *name* and must be one the server was
    // configured with (`--blueprints-dir`). Resolving an arbitrary path from a
    // network request would be a path-traversal / local-file-inclusion vector, so
    // only these vetted names are accepted.
    if let Some(name) = &req.blueprint
        && !app.blueprints.contains_key(name)
    {
        return Err(ApiError::bad_request(if app.blueprints.is_empty() {
            format!(
                "unknown blueprint `{name}`: the server has no blueprints configured \
                 (start it with --blueprints-dir <dir>)"
            )
        } else {
            let available: Vec<&str> = app.blueprints.keys().map(String::as_str).collect();
            format!(
                "unknown blueprint `{name}` (available: {})",
                available.join(", ")
            )
        }));
    }

    // Bound concurrent in-flight runs so a flood of requests can't exhaust the
    // host or burn provider budget. The permit is held until the run finishes.
    let permit = app
        .run_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::service_unavailable("server is at its concurrent-run limit"))?;

    let id = Uuid::new_v4();
    let (tx, _) = broadcast::channel(1024);
    let history = Arc::new(Mutex::new(Vec::new()));
    let handle = Arc::new(RunHandle {
        id,
        task: req.task.clone(),
        blueprint: req.blueprint.clone(),
        started_at: chrono::Utc::now(),
        status: parking_lot::RwLock::new(RunStatus::Running),
        result: parking_lot::RwLock::new(None),
        cancel: CancellationToken::new(),
        finished: CancellationToken::new(),
        tx,
        history,
    });
    app.register_run(handle.clone());
    info!(%id, blueprint = ?req.blueprint, "run started");

    tokio::spawn(run_job(
        app.clone(),
        handle,
        req.task,
        req.blueprint,
        permit,
    ));

    Ok((StatusCode::ACCEPTED, Json(json!({ "id": id }))))
}

/// Guarantees a run always reaches a terminal state. Its `Drop` fires
/// `finished` (so SSE streams close) and, if the run task panicked before
/// recording an outcome, marks the run `Failed` instead of leaving it stuck on
/// `Running` forever. On the normal path `disarm` is called after the status is
/// written, so `Drop` only cancels `finished`.
struct FinishGuard {
    handle: Arc<RunHandle>,
    recorded: bool,
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        if !self.recorded {
            // Mark the run Failed if it never recorded an outcome. parking_lot
            // locks never poison, so this can't panic on the unwind path; if the
            // guard is somehow unavailable we still cancel `finished` below.
            {
                let mut status = self.handle.status.write();
                *status = RunStatus::Failed {
                    error: Some("run task panicked".into()),
                };
            }
            tracing::error!(id = %self.handle.id, "run task panicked");
        }
        self.handle.finished.cancel();
    }
}

/// The background task: assemble an agent or executor with a [`ChannelAuditSink`],
/// run it under the handle's cancel token, then record the outcome. A
/// [`FinishGuard`] ensures `finished` fires and the status leaves `Running` even
/// if the run panics. `_permit` is held for the run's duration, releasing the
/// concurrent-run slot when the task ends.
async fn run_job(
    app: AppState,
    handle: Arc<RunHandle>,
    task: String,
    blueprint: Option<String>,
    _permit: OwnedSemaphorePermit,
) {
    let mut guard = FinishGuard {
        handle: handle.clone(),
        recorded: false,
    };
    let sink = Arc::new(ChannelAuditSink::new(
        handle.tx.clone(),
        handle.history.clone(),
    ));
    let outcome = match &blueprint {
        Some(name) => run_blueprint(&app, handle.clone(), sink, name, task).await,
        None => run_agent(&app, handle.clone(), sink, task).await,
    };
    record_outcome(&handle, outcome, &mut guard);
}

/// Record a run's final status and result on its handle, then disarm the
/// [`FinishGuard`] so its `Drop` only fires `finished`. Shared by the fresh-run
/// and resume paths.
fn record_outcome(
    handle: &Arc<RunHandle>,
    outcome: anyhow::Result<RunResult>,
    guard: &mut FinishGuard,
) {
    let status = if handle.cancel.is_cancelled() {
        RunStatus::Cancelled
    } else {
        match &outcome {
            Ok(result) if result.success => RunStatus::Succeeded,
            Ok(result) => RunStatus::Failed {
                error: result.error.clone(),
            },
            Err(e) => RunStatus::Failed {
                error: Some(format!("{e:#}")),
            },
        }
    };
    if let Ok(result) = outcome {
        *handle.result.write() = Some(result);
    }
    *handle.status.write() = status;
    guard.recorded = true; // outcome recorded; Drop now only cancels `finished`
    info!(id = %handle.id, "run finished");
}

/// The background task for a resumed run: rebuild the executor and continue the
/// blueprint from its persisted checkpoint, then record the outcome. `_permit`
/// releases the concurrent-run slot when the task ends.
async fn resume_job(
    app: AppState,
    handle: Arc<RunHandle>,
    name: String,
    _permit: OwnedSemaphorePermit,
) {
    let mut guard = FinishGuard {
        handle: handle.clone(),
        recorded: false,
    };
    let sink = Arc::new(ChannelAuditSink::new(
        handle.tx.clone(),
        handle.history.clone(),
    ));
    let outcome = resume_blueprint(&app, handle.clone(), sink, &name).await;
    record_outcome(&handle, outcome, &mut guard);
}

/// Continue a blueprint run from its checkpoint. Mirrors [`run_blueprint`] but
/// calls the executor's resume path (no task — it's restored from the
/// checkpoint).
async fn resume_blueprint(
    app: &AppState,
    handle: Arc<RunHandle>,
    sink: Arc<ChannelAuditSink>,
    name: &str,
) -> anyhow::Result<RunResult> {
    let path = resolve_blueprint_path(app, name)?;
    let graph = boitata_orchestrator::load(&path)?;
    let executor = blueprint_executor(app, sink, handle.id, name);
    let state = executor
        .resume_with_cancel(&graph, handle.cancel.clone())
        .await?;
    Ok(RunResult::from_state(&state))
}

async fn run_agent(
    app: &AppState,
    handle: Arc<RunHandle>,
    sink: Arc<ChannelAuditSink>,
    task: String,
) -> anyhow::Result<RunResult> {
    let cfg = &app.config;
    let mut agent = Agent::new(app.provider.clone(), app.tools.clone())
        .with_policy((*app.policy).clone())
        .with_audit(sink as Arc<dyn AuditSink>);
    if let Some(prompt) = cfg.system_prompt.clone() {
        agent = agent.with_system_prompt(prompt);
    }
    if let Some(max) = cfg.max_iterations {
        agent = agent.with_max_iterations(max);
    }
    if let Some(threshold) = cfg.auto_compact_threshold {
        agent = agent.with_compact_threshold(threshold);
    }
    let result = agent
        .run_with_cancel(Task::new(task), handle.cancel.clone())
        .await?;
    Ok(RunResult::from_task(result))
}

async fn run_blueprint(
    app: &AppState,
    handle: Arc<RunHandle>,
    sink: Arc<ChannelAuditSink>,
    name: &str,
    task: String,
) -> anyhow::Result<RunResult> {
    // Resolve the name against the vetted catalog (create_run already checked it,
    // but this keeps the network never able to pick an arbitrary path). Load from
    // the file rather than a compiled cache so an edited blueprint is picked up.
    let path = resolve_blueprint_path(app, name)?;
    let graph = boitata_orchestrator::load(&path)?;
    let executor = blueprint_executor(app, sink, handle.id, name);
    let state = executor
        .run_with_cancel(&graph, task, handle.cancel.clone())
        .await?;
    Ok(RunResult::from_state(&state))
}

/// Resolve a catalog blueprint name to its file path, rejecting unknown names
/// (the network can never pick an arbitrary path).
fn resolve_blueprint_path(app: &AppState, name: &str) -> anyhow::Result<String> {
    let path = app
        .blueprints
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown blueprint `{name}`"))?;
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("blueprint path for `{name}` is not valid UTF-8"))
}

/// Assemble a blueprint executor with the server's provider/tools/policy/agent
/// settings, the run's audit sink, and a checkpointer keyed by the run id — so
/// the run is resumable (see [`resume_run`]). `name` is the catalog key, recorded
/// as the checkpoint's blueprint label so a resume can reload the same graph.
fn blueprint_executor(
    app: &AppState,
    sink: Arc<ChannelAuditSink>,
    run_id: Uuid,
    name: &str,
) -> Executor {
    let cfg = &app.config;
    let checkpointer = Arc::new(SqliteCheckpointer::new(app.store.clone()));
    let mut executor = Executor::new(app.provider.clone(), app.tools.clone())
        .with_policy((*app.policy).clone())
        .with_system_prompt(cfg.system_prompt.clone())
        .with_max_iterations(cfg.max_iterations)
        .with_compact_threshold(cfg.auto_compact_threshold)
        // Forward the server's effective provider config into any sandbox a
        // `provision` node creates, so an in-container agent inherits it.
        .with_env_defaults(boitata_core::runtime::provider_env(cfg))
        .with_audit(sink as Arc<dyn AuditSink>)
        .with_checkpointer(checkpointer)
        .with_run_id(run_id.to_string())
        .with_blueprint_label(name)
        .with_max_retries(cfg.blueprint_max_retries);
    if let Some(max_steps) = cfg.blueprint_max_steps {
        executor = executor.with_max_steps(max_steps);
    }
    executor
}

/// `GET /api/runs` — newest first. Includes both in-memory runs and resumable
/// runs recovered from the state database (e.g. after a restart), the latter
/// reported as `suspended`. In-memory entries take precedence for a given id.
async fn list_runs(State(app): State<AppState>) -> Json<Vec<RunSummary>> {
    let mut runs: Vec<RunSummary> = app.runs.read().values().map(|h| h.summary()).collect();
    let live: std::collections::HashSet<Uuid> = runs.iter().map(|r| r.id).collect();

    // Fold in resumable checkpoints not already represented by a live run.
    if let Ok(records) = app.store.list_checkpoints(true).await {
        for r in records {
            let Ok(id) = Uuid::parse_str(&r.run_id) else {
                continue;
            };
            if live.contains(&id) {
                continue;
            }
            runs.push(RunSummary {
                id,
                task: r.task,
                blueprint: Some(r.blueprint),
                status: RunStatus::Suspended,
                started_at: r.updated_at,
            });
        }
    }

    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    Json(runs)
}

/// `GET /api/runs/{id}` — summary, final result (if any), and the full event log.
async fn get_run(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RunDetail>, ApiError> {
    let handle = app.get_run(id).ok_or_else(ApiError::not_found)?;
    let events = handle.history.lock().clone();
    Ok(Json(RunDetail {
        summary: handle.summary(),
        result: handle.result.read().clone(),
        events,
    }))
}

/// `POST /api/runs/{id}/cancel` — request cancellation; the run stops cooperatively.
async fn cancel_run(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let handle = app.get_run(id).ok_or_else(ApiError::not_found)?;
    handle.cancel.cancel();
    Ok(StatusCode::ACCEPTED)
}

/// `POST /api/runs/{id}/resume` — continue an interrupted blueprint run from its
/// persisted checkpoint, spawning it under the same id. Works even after a server
/// restart, when the run is no longer in the in-memory registry. Returns `{ id }`.
async fn resume_run(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // A run already executing in memory must not be resumed concurrently.
    if let Some(handle) = app.get_run(id)
        && matches!(*handle.status.read(), RunStatus::Running)
    {
        return Err(ApiError::conflict("run is already running"));
    }

    let record = app
        .store
        .get_checkpoint(&id.to_string())
        .await
        .map_err(|e| ApiError::internal(format!("failed to read checkpoint: {e:#}")))?
        .ok_or_else(|| ApiError::not_found_msg("no checkpoint to resume for this run"))?;

    if !record.status.is_resumable() {
        return Err(ApiError::conflict(
            "run is not resumable (it already completed or failed)",
        ));
    }

    // The checkpoint's blueprint label is the catalog name the run was started
    // with; it must still be one the server offers (never an arbitrary path).
    let name = record.blueprint.clone();
    if !app.blueprints.contains_key(&name) {
        return Err(ApiError::bad_request(format!(
            "cannot resume: blueprint `{name}` is not in the server's catalog"
        )));
    }

    // Bound concurrent runs (same as create_run). Acquired here so a conflict
    // below releases the slot via drop.
    let permit = app
        .run_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::service_unavailable("server is at its concurrent-run limit"))?;

    // Rebuild the run handle under the same id with fresh event plumbing and a new
    // cancel token, then continue it in the background.
    let (tx, _) = broadcast::channel(1024);
    let history = Arc::new(Mutex::new(Vec::new()));
    let handle = Arc::new(RunHandle {
        id,
        task: record.task.clone(),
        blueprint: Some(name.clone()),
        started_at: chrono::Utc::now(),
        status: parking_lot::RwLock::new(RunStatus::Running),
        result: parking_lot::RwLock::new(None),
        cancel: CancellationToken::new(),
        finished: CancellationToken::new(),
        tx,
        history,
    });
    // Atomically claim the id: the running-check above and this insert are not a
    // single critical section, so two concurrent resumes could both pass it.
    // `try_register_run` closes that window — the second caller gets a conflict
    // instead of driving the same checkpoint twice.
    if !app.try_register_run(handle.clone()) {
        return Err(ApiError::conflict("run is already running"));
    }
    info!(%id, blueprint = %name, "run resumed");

    tokio::spawn(resume_job(app.clone(), handle, name, permit));
    Ok((StatusCode::ACCEPTED, Json(json!({ "id": id }))))
}

/// `GET /api/blueprints` — the names of the blueprints the server can run (from
/// `--blueprints-dir`), sorted. Empty when none are configured; the UI dropdown
/// then offers only the single-agent path. These are the only names `POST
/// /api/runs` accepts.
async fn list_blueprints(State(app): State<AppState>) -> Json<Vec<String>> {
    Json(app.blueprints.keys().cloned().collect())
}

/// `GET /api/blueprints/{name}` — the blueprint's graph for display: its nodes
/// (each tagged deterministic vs probabilistic) and edges (with any
/// success/failure condition), so the web UI can draw it. Only configured names
/// resolve; the file is re-read so an edit is reflected without a restart.
async fn get_blueprint(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<boitata_orchestrator::BlueprintGraph>, ApiError> {
    let path = app
        .blueprints
        .get(&name)
        .ok_or_else(|| ApiError::not_found_msg(format!("unknown blueprint `{name}`")))?;
    let src = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| ApiError::internal(format!("failed to read blueprint `{name}`: {e}")))?;
    let graph = boitata_orchestrator::describe(&src)
        .map_err(|e| ApiError::internal(format!("blueprint `{name}` is invalid: {e:#}")))?;
    Ok(Json(graph))
}

/// `GET /api/blueprints/{name}/source` — the blueprint's raw definition (the YAML
/// file as written), so the UI can show the exact source behind the rendered
/// graph. Only configured names resolve; the file is re-read so an edit shows up
/// without a restart.
async fn get_blueprint_source(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<BlueprintSource>, ApiError> {
    let path = app
        .blueprints
        .get(&name)
        .ok_or_else(|| ApiError::not_found_msg(format!("unknown blueprint `{name}`")))?;
    let source = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| ApiError::internal(format!("failed to read blueprint `{name}`: {e}")))?;
    Ok(Json(BlueprintSource { name, source }))
}

/// The raw definition of a blueprint, returned by `get_blueprint_source`.
#[derive(Serialize)]
struct BlueprintSource {
    name: String,
    source: String,
}

/// `GET /api/runs/{id}/events` — Server-Sent Events. Replays the history buffer,
/// then streams live events until the run finishes. Events carry `seq` so the
/// replay/live handoff can dedupe.
async fn run_events(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let handle = app.get_run(id).ok_or_else(ApiError::not_found)?;

    // Subscribe before snapshotting history so no event slips through the gap;
    // any overlap is removed by the `seq` dedupe below.
    let mut rx = handle.tx.subscribe();
    let history = handle.history.lock().clone();
    let finished = handle.finished.clone();
    let already_finished = finished.is_cancelled();

    let stream = async_stream::stream! {
        let mut last_seq: Option<u64> = None;
        for ev in history {
            last_seq = Some(ev.seq);
            yield Ok(to_sse(&ev));
        }
        // If the run already ended, the history above is complete — stop.
        if already_finished {
            return;
        }
        loop {
            tokio::select! {
                biased;
                recv = rx.recv() => match recv {
                    Ok(ev) => {
                        if last_seq.is_none_or(|last| ev.seq > last) {
                            last_seq = Some(ev.seq);
                            yield Ok(to_sse(&ev));
                        }
                    }
                    // Dropped events under load: keep going rather than close.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = finished.cancelled() => {
                    // Drain anything buffered after `finished` tripped.
                    while let Ok(ev) = rx.try_recv() {
                        if last_seq.is_none_or(|last| ev.seq > last) {
                            last_seq = Some(ev.seq);
                            yield Ok(to_sse(&ev));
                        }
                    }
                    break;
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn to_sse(ev: &crate::events::RunEvent) -> Event {
    Event::default().json_data(ev).unwrap_or_else(|e| {
        // A serialization failure means a bug in an event payload; surface it in
        // logs rather than silently shipping an empty frame to the client.
        tracing::warn!("failed to serialize run event (seq {}): {e}", ev.seq);
        Event::default().data("{}")
    })
}

/// A small JSON error response.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: "run not found".into(),
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn not_found_msg(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid API token".into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }
    fn service_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use boitata_core::config::Config;
    use boitata_core::runtime;
    use tower::ServiceExt; // for `oneshot`

    /// An `AppState` backed by the (keyless) ollama provider so no network or API
    /// key is needed. The non-LLM endpoints under test never actually run a task.
    async fn test_state() -> AppState {
        state_with_blueprints(std::collections::BTreeMap::new()).await
    }

    /// Like [`test_state`], but with a preconfigured blueprint catalog.
    async fn state_with_blueprints(
        blueprints: std::collections::BTreeMap<String, std::path::PathBuf>,
    ) -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boitata.toml");
        std::fs::write(&path, "provider = \"ollama\"\nmodel = \"llama3\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        let provider = runtime::build_provider(&config).unwrap();
        let tools = runtime::build_tools(&config).await.unwrap();
        let policy = runtime::build_policy(&config).unwrap();
        let store = boitata_store::Store::open_in_memory().unwrap();
        AppState::new(config, provider, tools, policy, blueprints, store)
    }

    /// Like [`test_state`], but with the HTTP API gated by `token` (so auth can be
    /// exercised without touching real provider keys).
    async fn state_with_token(token: &str) -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boitata.toml");
        std::fs::write(
            &path,
            format!("provider = \"ollama\"\nmodel = \"llama3\"\napi_token = \"{token}\"\n"),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        let provider = runtime::build_provider(&config).unwrap();
        let tools = runtime::build_tools(&config).await.unwrap();
        let policy = runtime::build_policy(&config).unwrap();
        let store = boitata_store::Store::open_in_memory().unwrap();
        AppState::new(
            config,
            provider,
            tools,
            policy,
            std::collections::BTreeMap::new(),
            store,
        )
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn list_runs_starts_empty() {
        let app = router(test_state().await);
        let resp = app
            .oneshot(Request::get("/api/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await, json!([]));
    }

    #[tokio::test]
    async fn api_token_gates_requests_when_configured() {
        let app = router(state_with_token("s3cret").await);
        // Missing token → 401.
        let resp = app
            .clone()
            .oneshot(Request::get("/api/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Wrong token → 401.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/runs")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Correct token → 200 (list_runs, which doesn't spawn a run).
        let resp = app
            .oneshot(
                Request::get("/api/runs")
                    .header("authorization", "Bearer s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_query_param_authenticates_sse() {
        // EventSource (browser) can't set headers; it must use ?token=.
        let app = router(state_with_token("s3cret").await);
        let resp = app
            .oneshot(
                Request::get("/api/runs?token=s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn no_token_configured_leaves_api_open() {
        // Default (no token): requests pass through (loopback single-user model).
        let app = router(test_state().await);
        let resp = app
            .oneshot(Request::get("/api/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let app = router(test_state().await);
        let big = "a".repeat(MAX_BODY_BYTES + 1);
        let body = format!("{{\"task\":\"{big}\"}}");
        let resp = app
            .oneshot(
                Request::post("/api/runs")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// Insert a checkpoint row directly, to exercise the resume/listing paths
    /// without actually running a graph.
    async fn put_checkpoint(
        store: &boitata_store::Store,
        id: Uuid,
        blueprint: &str,
        status: boitata_store::RunState,
    ) {
        store
            .upsert_checkpoint(boitata_store::CheckpointUpsert {
                run_id: id.to_string(),
                blueprint: blueprint.to_string(),
                task: "resume me".to_string(),
                step: 1,
                frontier: vec!["b".to_string()],
                state: r#"{"task":"resume me","messages":[],"status":null,"vars":{}}"#.to_string(),
                status,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn resume_unknown_run_is_404() {
        let app = router(test_state().await);
        let uri = format!("/api/runs/{}/resume", Uuid::new_v4());
        let resp = app
            .oneshot(Request::post(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn resume_completed_run_is_conflict() {
        let state = test_state().await;
        let store = state.store.clone();
        let app = router(state);
        let id = Uuid::new_v4();
        put_checkpoint(&store, id, "whatever", boitata_store::RunState::Completed).await;

        let uri = format!("/api/runs/{id}/resume");
        let resp = app
            .oneshot(Request::post(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn resume_unknown_blueprint_is_bad_request() {
        // Catalog is empty, so a resumable checkpoint naming `ghost` can't reload.
        let state = test_state().await;
        let store = state.store.clone();
        let app = router(state);
        let id = Uuid::new_v4();
        put_checkpoint(&store, id, "ghost", boitata_store::RunState::Suspended).await;

        let uri = format!("/api/runs/{id}/resume");
        let resp = app
            .oneshot(Request::post(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_runs_includes_suspended_checkpoints() {
        let state = test_state().await;
        let store = state.store.clone();
        let app = router(state);
        let id = Uuid::new_v4();
        put_checkpoint(&store, id, "fix_test", boitata_store::RunState::Suspended).await;

        let resp = app
            .oneshot(Request::get("/api/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["status"]["state"], "suspended");
        assert_eq!(arr[0]["id"], id.to_string());
    }

    #[tokio::test]
    async fn unknown_run_is_404() {
        let app = router(test_state().await);
        let uri = format!("/api/runs/{}", Uuid::new_v4());
        let resp = app
            .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn blueprints_lists_none() {
        // The server hosts no blueprints (they are local YAML files run via the
        // CLI), so the endpoint returns an empty list.
        let app = router(test_state().await);
        let resp = app
            .oneshot(Request::get("/api/blueprints").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await, json!([]));
    }

    #[tokio::test]
    async fn empty_task_is_rejected() {
        let app = router(test_state().await);
        let resp = app
            .oneshot(
                Request::post("/api/runs")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "task": "  " }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_blueprint_is_rejected() {
        let app = router(test_state().await);
        let resp = app
            .oneshot(
                Request::post("/api/runs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "task": "x", "blueprint": "nope" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn configured_blueprints_are_listed_and_accepted() {
        // With a blueprints catalog, the endpoint lists the names and a run
        // referencing a known name is accepted (spawned), while an unknown one is
        // still rejected.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tidy.yaml"),
            "name: tidy\nentry: a\nnodes:\n  a: {type: tool, tool: cargo_fmt}\n",
        )
        .unwrap();
        let catalog = boitata_orchestrator::discover(dir.path()).unwrap();
        let app = router(state_with_blueprints(catalog).await);

        // Listed.
        let resp = app
            .clone()
            .oneshot(Request::get("/api/blueprints").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(body_json(resp).await, json!(["tidy"]));

        // A known name is accepted.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/runs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "task": "x", "blueprint": "tidy" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // An unknown name is rejected even when a catalog exists.
        let resp = app
            .oneshot(
                Request::post("/api/runs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "task": "x", "blueprint": "nope" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn get_blueprint_returns_its_graph() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("verify.yaml"),
            "name: verify\nentry: work\nnodes:\n  work: {type: agent, prompt: \"go {task}\"}\n  \
             check: {type: script, run: \"cargo test\"}\nedges:\n  - {from: work, to: check}\n  \
             - {from: check, when: failure, to: work}\n  - {from: check, when: success, to: END}\n",
        )
        .unwrap();
        let catalog = boitata_orchestrator::discover(dir.path()).unwrap();
        let app = router(state_with_blueprints(catalog).await);

        // A known blueprint returns its graph with node classifications.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/blueprints/verify")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let g = body_json(resp).await;
        assert_eq!(g["entry"], "work");
        let work = g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "work")
            .unwrap();
        assert_eq!(work["execution"], "probabilistic");

        // An unknown blueprint is a 404.
        let resp = app
            .oneshot(
                Request::get("/api/blueprints/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_blueprint_source_returns_raw_definition() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "name: tidy\nentry: a\nnodes:\n  a: {type: tool, tool: cargo_fmt}\n";
        std::fs::write(dir.path().join("tidy.yaml"), yaml).unwrap();
        let catalog = boitata_orchestrator::discover(dir.path()).unwrap();
        let app = router(state_with_blueprints(catalog).await);

        // A known blueprint returns its raw YAML source verbatim.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/blueprints/tidy/source")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["name"], "tidy");
        assert_eq!(body["source"], yaml);

        // An unknown blueprint is a 404.
        let resp = app
            .oneshot(
                Request::get("/api/blueprints/nope/source")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
