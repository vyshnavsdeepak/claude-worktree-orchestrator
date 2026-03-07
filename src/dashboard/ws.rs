use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;

use super::types::WorkersResponse;
use super::DashboardContext;

pub async fn handler(
    ws: WebSocketUpgrade,
    State(ctx): State<Arc<DashboardContext>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, ctx))
}

async fn handle_socket(mut socket: WebSocket, ctx: Arc<DashboardContext>) {
    let mut rx = ctx.worker_rx.clone();

    loop {
        let changed = rx.changed().await;
        if changed.is_err() {
            break;
        }
        let workers = rx.borrow_and_update().clone();
        let resp = WorkersResponse::from_workers(&workers);

        let payload = serde_json::json!({
            "type": "workers",
            "data": resp,
        });

        let Ok(text) = serde_json::to_string(&payload) else {
            continue;
        };

        if socket.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}
