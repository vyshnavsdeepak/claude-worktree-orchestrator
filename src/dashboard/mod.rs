mod routes;
mod types;
mod ws;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::{mpsc, watch};
use tower_http::cors::CorsLayer;

use axum::response::Html;

use crate::config::Config;
use crate::events::EventLog;
use crate::poller::WorkerState;
use crate::state::StateDir;

const INDEX_HTML: &str = include_str!("static/index.html");

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub struct DashboardContext {
    pub config: Arc<Config>,
    pub worker_rx: watch::Receiver<Vec<WorkerState>>,
    pub event_log: EventLog,
    pub state_dir: Arc<StateDir>,
    pub prompt_tx: Option<mpsc::UnboundedSender<String>>,
}

pub async fn start(ctx: Arc<DashboardContext>, port: u16) {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(routes::health))
        .route("/api/workers", get(routes::workers))
        .route("/api/stats", get(routes::stats))
        .route(
            "/api/config",
            get(routes::config).put(routes::update_config),
        )
        .route("/api/workers/launch", post(routes::launch_issue))
        .route("/api/workers/launch-direct", post(routes::launch_direct))
        .route("/api/workers/{name}/send", post(routes::send_to_worker))
        .route(
            "/api/workers/{name}/interrupt",
            post(routes::interrupt_worker),
        )
        .route("/api/merge/{pr_num}", post(routes::merge_pr))
        .route("/api/ws", get(ws::handler))
        .layer(CorsLayer::permissive())
        .with_state(ctx);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[dashboard] Failed to bind port {port}: {e}");
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("[dashboard] Server error: {e}");
    }
}
