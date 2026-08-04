//! HTTP surface: REST for actions, SSE for live run events.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use boitata_agent::{Agent, Task};
use boitata_core::audit::AuditSink;
use boitata_orchestrator::Executor;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

use crate::events::ChannelAuditSink;
use crate::state::{AppState, RunHandle, RunResult, RunStatus, RunSummary};

/// Build the application router: the JSON/SSE API under `/api`, with the embedded
/// web UI served for everything else (SPA fallback lives in `assets`).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/runs", post(create_run).get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/blueprints", get(list_blueprints))
        .with_state(state)
        .fallback(crate::assets::static_handler)
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

    let id = Uuid::new_v4();
    let (tx, _) = broadcast::channel(1024);
    let history = Arc::new(Mutex::new(Vec::new()));
    let handle = Arc::new(RunHandle {
        id,
        task: req.task.clone(),
        blueprint: req.blueprint.clone(),
        started_at: chrono::Utc::now(),
        status: std::sync::RwLock::new(RunStatus::Running),
        result: std::sync::RwLock::new(None),
        cancel: CancellationToken::new(),
        finished: CancellationToken::new(),
        tx,
        history,
    });
    app.register_run(handle.clone());
    info!(%id, blueprint = ?req.blueprint, "run started");

    tokio::spawn(run_job(app.clone(), handle, req.task, req.blueprint));

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
            // Best-effort and panic-proof: this runs on the run task's unwind
            // path, so it must never `unwrap()` a (possibly poisoned) lock and
            // double-panic — that would abort the process. Skipping the status
            // write is acceptable; `finished` still fires below so SSE closes.
            if let Ok(mut status) = self.handle.status.write() {
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
/// if the run panics.
async fn run_job(app: AppState, handle: Arc<RunHandle>, task: String, blueprint: Option<String>) {
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
        *handle.result.write().unwrap() = Some(result);
    }
    *handle.status.write().unwrap() = status;
    guard.recorded = true; // outcome recorded; Drop now only cancels `finished`
    info!(id = %handle.id, "run finished");
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
    let cfg = &app.config;
    // Resolve the name against the vetted catalog (create_run already checked it,
    // but this keeps the network never able to pick an arbitrary path). Load from
    // the file rather than a compiled cache so an edited blueprint is picked up.
    let path = app
        .blueprints
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown blueprint `{name}`"))?;
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("blueprint path for `{name}` is not valid UTF-8"))?;
    let graph = boitata_orchestrator::load(path)?;
    let mut executor = Executor::new(app.provider.clone(), app.tools.clone())
        .with_policy((*app.policy).clone())
        .with_system_prompt(cfg.system_prompt.clone())
        .with_max_iterations(cfg.max_iterations)
        .with_compact_threshold(cfg.auto_compact_threshold)
        .with_audit(sink as Arc<dyn AuditSink>)
        .with_max_retries(cfg.blueprint_max_retries);
    if let Some(max_steps) = cfg.blueprint_max_steps {
        executor = executor.with_max_steps(max_steps);
    }
    let state = executor
        .run_with_cancel(&graph, task, handle.cancel.clone())
        .await?;
    Ok(RunResult::from_state(&state))
}

/// `GET /api/runs` — newest first.
async fn list_runs(State(app): State<AppState>) -> Json<Vec<RunSummary>> {
    let mut runs: Vec<RunSummary> = app
        .runs
        .read()
        .unwrap()
        .values()
        .map(|h| h.summary())
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    Json(runs)
}

/// `GET /api/runs/{id}` — summary, final result (if any), and the full event log.
async fn get_run(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RunDetail>, ApiError> {
    let handle = app.get_run(id).ok_or_else(ApiError::not_found)?;
    let events = handle.history.lock().unwrap().clone();
    Ok(Json(RunDetail {
        summary: handle.summary(),
        result: handle.result.read().unwrap().clone(),
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

/// `GET /api/blueprints` — the names of the blueprints the server can run (from
/// `--blueprints-dir`), sorted. Empty when none are configured; the UI dropdown
/// then offers only the single-agent path. These are the only names `POST
/// /api/runs` accepts.
async fn list_blueprints(State(app): State<AppState>) -> Json<Vec<String>> {
    Json(app.blueprints.keys().cloned().collect())
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
    let history = handle.history.lock().unwrap().clone();
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
        AppState::new(config, provider, tools, policy, blueprints)
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
}
