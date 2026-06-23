mod datetime;
mod engine;
mod models;
mod notion;
mod openrouter;
mod scheduler;
mod store;
mod telegram;
mod transcribe;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use models::{RunRequest, RunResponse, Workflow, WorkflowInput};
use store::Store;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    store: Store,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok(); // load backend/.env if present

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let data_path = std::env::var("OPTIMIMER_DATA")
        .unwrap_or_else(|_| "workflows.json".to_string())
        .into();
    let state = AppState {
        store: Store::load(data_path),
    };

    // Seed the slash-command agents (cmd-notion, cmd-remind, …) from disk so the
    // Telegram commands work out of the box. Existing entries are left untouched.
    seed_command_agents(&state.store);

    // Scheduler: backs /remind, /notify, and the auto-clear-meeting rule. The
    // worker fires due entries every 30s.
    let sched_path = std::env::var("OPTIMIMER_SCHEDULE_DATA")
        .unwrap_or_else(|_| "schedules.json".to_string())
        .into();
    let sched = scheduler::init(sched_path);
    tokio::spawn(scheduler::run_worker(sched));

    // Telegram is the primary interface — run the long-polling bot alongside
    // the HTTP API. No-ops with a warning if TELEGRAM_BOT_TOKEN is unset.
    tokio::spawn(telegram::run_bot(state.store.clone()));

    let app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/workflows", get(list_workflows).post(create_workflow))
        .route(
            "/api/workflows/:id",
            get(get_workflow).put(update_workflow).delete(delete_workflow),
        )
        .route("/api/workflows/:id/run", post(run_workflow))
        .layer(CorsLayer::very_permissive())
        .with_state(state);

    let addr = "0.0.0.0:8799";
    tracing::info!("optimimer backend listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// Load `cmd-*.json` workflow files from the agents directory and sync them into
/// the store (keyed by `cmd-<filename-stem>`). The files are the source of truth
/// for the Telegram slash-command agents, so they're re-applied on every startup
/// — edit the JSON to change a command's behaviour (UI edits to `cmd-*` agents do
/// not survive a restart).
fn seed_command_agents(store: &Store) {
    let dir = std::env::var("OPTIMIMER_AGENTS_DIR").unwrap_or_else(|_| "agents".to_string());
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => {
            tracing::info!("no agents dir at '{dir}' — skipping command-agent seeding");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // `cmd-*` command agents are canonical: always re-seed so edits to the
        // bundled definitions take effect on restart. Other bundled example
        // agents seed only if absent, so a user's UI edits aren't clobbered.
        let is_command = stem.starts_with("cmd-");
        if !is_command && store.get(&stem).is_some() {
            continue;
        }
        let input: WorkflowInput = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(w) => w,
            None => {
                tracing::warn!("skipping malformed agent file: {}", path.display());
                continue;
            }
        };
        store.upsert(Workflow {
            id: stem.clone(),
            name: input.name,
            nodes: input.nodes,
            edges: input.edges,
            updated_at: Utc::now().to_rfc3339(),
        });
        tracing::info!("seeded agent '{stem}'");
    }
}

async fn list_workflows(State(s): State<AppState>) -> Json<Vec<Workflow>> {
    Json(s.store.list())
}

async fn get_workflow(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Workflow>, StatusCode> {
    s.store.get(&id).map(Json).ok_or(StatusCode::NOT_FOUND)
}

async fn create_workflow(
    State(s): State<AppState>,
    Json(input): Json<WorkflowInput>,
) -> Json<Workflow> {
    let wf = Workflow {
        id: Uuid::new_v4().to_string(),
        name: input.name,
        nodes: input.nodes,
        edges: input.edges,
        updated_at: Utc::now().to_rfc3339(),
    };
    Json(s.store.upsert(wf))
}

async fn update_workflow(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<WorkflowInput>,
) -> Result<Json<Workflow>, StatusCode> {
    if s.store.get(&id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let wf = Workflow {
        id,
        name: input.name,
        nodes: input.nodes,
        edges: input.edges,
        updated_at: Utc::now().to_rfc3339(),
    };
    Ok(Json(s.store.upsert(wf)))
}

async fn delete_workflow(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    if s.store.delete(&id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn run_workflow(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RunRequest>,
) -> Result<Json<RunResponse>, StatusCode> {
    // Prefer the inline (unsaved) workflow if provided, else the stored one.
    let wf = match req.workflow {
        Some(w) => w,
        None => s.store.get(&id).ok_or(StatusCode::NOT_FOUND)?,
    };
    Ok(Json(engine::run(&wf, req.input).await))
}
