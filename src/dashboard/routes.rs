use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use crate::config::RuntimeConfig;

use super::types::*;
use super::DashboardContext;

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

pub async fn workers(State(ctx): State<Arc<DashboardContext>>) -> Json<WorkersResponse> {
    let workers = ctx.worker_rx.borrow().clone();
    Json(WorkersResponse::from_workers(&workers))
}

pub async fn stats(State(ctx): State<Arc<DashboardContext>>) -> Json<StatsResponse> {
    let s = ctx.event_log.stats();
    Json(StatsResponse {
        merged_count: s.merged_count,
        failed_count: s.failed_count,
        active_count: s.active_count,
        avg_merge_secs: s.avg_merge_secs(),
    })
}

pub async fn config(State(ctx): State<Arc<DashboardContext>>) -> Json<ConfigResponse> {
    let rt = RuntimeConfig::load(&ctx.state_dir.runtime_config())
        .unwrap_or_else(|| RuntimeConfig::from_config(&ctx.config));
    Json(ConfigResponse {
        merge_policy: rt.merge_policy,
        auto_review: rt.auto_review,
        review_timeout_secs: rt.review_timeout_secs,
        auto_relaunch: rt.auto_relaunch,
        max_relaunch_attempts: rt.max_relaunch_attempts,
        stale_timeout_secs: rt.stale_timeout_secs,
        max_concurrent: rt.max_concurrent,
    })
}

pub async fn launch_issue(
    State(ctx): State<Arc<DashboardContext>>,
    Json(body): Json<LaunchIssueRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let Some(ref tx) = ctx.prompt_tx else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "prompt handler not available".to_string(),
        ));
    };
    tx.send(format!("__NEWJOB_{}__", body.issue_num))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn launch_direct(
    State(ctx): State<Arc<DashboardContext>>,
    Json(body): Json<LaunchDirectRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let Some(ref tx) = ctx.prompt_tx else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "prompt handler not available".to_string(),
        ));
    };
    tx.send(format!("__DIRECT_{}__", body.prompt))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn send_to_worker(
    State(ctx): State<Arc<DashboardContext>>,
    Path(name): Path<String>,
    Json(body): Json<SendTextRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let target = format!("{}:{}", ctx.config.session, name);
    let status = tokio::process::Command::new(&ctx.config.tmux)
        .args(["send-keys", "-t", &target, &body.text, "Enter"])
        .status()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if status.success() {
        Ok(StatusCode::OK)
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!("tmux send-keys failed for window '{name}'"),
        ))
    }
}

pub async fn interrupt_worker(
    State(ctx): State<Arc<DashboardContext>>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let target = format!("{}:{}", ctx.config.session, name);
    let status = tokio::process::Command::new(&ctx.config.tmux)
        .args(["send-keys", "-t", &target, "C-c"])
        .status()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if status.success() {
        Ok(StatusCode::OK)
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!("tmux send-keys C-c failed for window '{name}'"),
        ))
    }
}

pub async fn merge_pr(
    State(ctx): State<Arc<DashboardContext>>,
    Path(pr_num): Path<u64>,
) -> Result<StatusCode, (StatusCode, String)> {
    crate::github::merge_pr(&ctx.config.repo, pr_num)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::OK)
}

pub async fn update_config(
    State(ctx): State<Arc<DashboardContext>>,
    Json(body): Json<RuntimeConfig>,
) -> StatusCode {
    body.save(&ctx.state_dir.runtime_config());
    StatusCode::OK
}
