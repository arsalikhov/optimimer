mod cache;
mod charts;
mod datetime;
mod db;
mod engine;
mod finance;
mod memory;
mod models;
mod openrouter;
mod scheduler;
mod shopper;
mod vault;
mod convo;
mod store;
mod telegram;
mod transcribe;
mod vision;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use db::Db;
use models::{RunRequest, RunResponse, Workflow, WorkflowInput};
use std::path::Path as FilePath;
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

    // One SQLite database holds all persistent state (agents, timezones,
    // schedules, shopping lists). Override the location with OPTIMIMER_DB.
    let db_path = std::env::var("OPTIMIMER_DB").unwrap_or_else(|_| "optimimer.db".to_string());
    let db = Db::open(FilePath::new(&db_path)).expect("open sqlite database");

    // Cache for expensive deterministic LLM calls (CSV parse, receipt OCR).
    cache::init(db.clone());

    // Transactions ledger (/spent, /earned, /balance, CSV import) — SQLite.
    finance::init(db.clone());

    // Conversation history + knowledge graph the agent recalls from.
    memory::init(db.clone());

    // Markdown vault (notes, tasks, summaries) — an Obsidian vault on disk.
    match vault::init() {
        Ok(p) => {
            tracing::info!("vault at {}", p.display());
            // Ledger mirror for Obsidian Bases: catch up on rows without a note
            // (first run after an import, or a vault that was wiped) and make
            // sure the .base views and Home dashboard exist.
            // FINANCE_PLACEHOLDERS=1 fills an EMPTY ledger with labelled sample
            // rows so the dashboard renders; they vanish on the first real entry.
            if std::env::var("FINANCE_PLACEHOLDERS").map(|v| v == "1").unwrap_or(false) && finance::global().all().is_empty() {
                let today = chrono::Utc::now().date_naive();
                let n = finance::seed_placeholders(today);
                tracing::info!("seeded {n} placeholder transactions (FINANCE_PLACEHOLDERS=1)");
            }
            let n = vault::backfill_transactions(&finance::global().all());
            if n > 0 {
                tracing::info!("finance mirror: wrote {n} transaction note(s)");
            }
            charts::refresh();
            charts::refresh_gantt();
            tokio::spawn(charts::run_gantt_worker());
            // Every task/note file gets a graph node; then memory graph → vault
            // notes, rebuilt if the folder was wiped.
            let indexed = memory::index_vault();
            if indexed > 0 {
                tracing::info!("memory: indexed {indexed} vault file(s)");
            }
            if vault::memory_mirror_count() == 0 && memory::global().node_count() > 0 {
                tracing::info!("memory mirror: wrote {} note(s)", memory::mirror_all());
            }
            match vault::ensure_starter_files() {
                Ok(w) if !w.is_empty() => tracing::info!("wrote vault starter files: {}", w.join(", ")),
                Ok(_) => {}
                Err(e) => tracing::warn!("could not write vault starter files: {e}"),
            }
        }
        Err(e) => tracing::error!("cannot create VAULT_DIR: {e}"),
    }

    let store = Store::new(db.clone());

    // One-time imports from the legacy JSON stores. Each is a no-op once its
    // table has rows, and the JSON files are left untouched as a backup.
    store.migrate_json(FilePath::new(
        &std::env::var("OPTIMIMER_DATA").unwrap_or_else(|_| "workflows.json".to_string()),
    ));
    telegram::migrate_json(
        &db,
        FilePath::new(&std::env::var("OPTIMIMER_TZ_DATA").unwrap_or_else(|_| "chat_tz.json".to_string())),
        FilePath::new(&std::env::var("OPTIMIMER_LISTS_DATA").unwrap_or_else(|_| "lists.json".to_string())),
    );

    let state = AppState {
        store: store.clone(),
    };

    // Seed the slash-command agents (cmd-todo, cmd-note, …) from disk so the
    // Telegram commands work out of the box. Existing entries are left untouched.
    seed_command_agents(&state.store);

    // Scheduler: backs /remind, /notify, and the auto-clear-meeting rule. The
    // worker fires due entries every 30s.
    let sched = scheduler::init(db.clone());
    sched.migrate_json(FilePath::new(
        &std::env::var("OPTIMIMER_SCHEDULE_DATA").unwrap_or_else(|_| "schedules.json".to_string()),
    ));
    tokio::spawn(scheduler::run_worker(sched));

    // Shopper: backs /watch — hourly re-checks of product pages, pinging the
    // chat when something comes back in stock.
    tokio::spawn(shopper::run_worker(shopper::init(db.clone())));

    // Convo notes: summaries + transcripts of forwarded messages and long voice
    // memos (index in SQLite; the Markdown copy lands in the vault).
    convo::init(db.clone());

    // Telegram is the primary interface — run the long-polling bot alongside
    // the HTTP API. No-ops with a warning if TELEGRAM_BOT_TOKEN is unset.
    tokio::spawn(telegram::run_bot(store.clone(), db.clone()));

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

    // Bind to localhost only by default — the API has no auth and exposes agent
    // CRUD, so it must not be reachable from the LAN. Override with OPTIMIMER_BIND
    // (e.g. 0.0.0.0:8799) only behind a trusted network or a reverse proxy.
    let addr = std::env::var("OPTIMIMER_BIND").unwrap_or_else(|_| "127.0.0.1:8799".to_string());
    tracing::info!("optimimer backend listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
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
    let mut seen_commands = std::collections::HashSet::new();
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
        if is_command {
            seen_commands.insert(stem.clone());
        } else if store.get(&stem).is_some() {
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

    // The files are the source of truth for `cmd-*` agents, so prune any that no
    // longer have a backing file — otherwise a deleted command lingers in the
    // store (and keeps responding) forever. Non-`cmd-*` agents are left alone;
    // those may be user-created in the UI and have no file by design.
    //
    // Guard: if NO command files were found, the dir is probably empty/misdeployed
    // — don't prune, or we'd wipe every command agent from the store.
    if seen_commands.is_empty() {
        tracing::warn!("no cmd-* agent files found in '{dir}' — skipping prune to avoid wiping commands");
        return;
    }
    for wf in store.list() {
        if wf.id.starts_with("cmd-") && !seen_commands.contains(&wf.id) {
            store.delete(&wf.id);
            tracing::info!("pruned orphaned command agent '{}' (no file in '{dir}')", wf.id);
        }
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
