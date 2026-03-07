use serde::Serialize;

use crate::poller::WorkerState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub ok: bool,
}

#[derive(Serialize)]
pub struct WorkerSummary {
    pub total: usize,
    pub active: usize,
    pub idle: usize,
    pub queued: usize,
    pub done: usize,
}

#[derive(Serialize)]
pub struct WorkersResponse {
    pub workers: Vec<WorkerState>,
    pub summary: WorkerSummary,
}

impl WorkersResponse {
    pub fn from_workers(workers: &[WorkerState]) -> Self {
        let mut summary = WorkerSummary {
            total: workers.len(),
            active: 0,
            idle: 0,
            queued: 0,
            done: 0,
        };
        for w in workers {
            match w.status.as_str() {
                "active" => summary.active += 1,
                "idle" | "sleeping" => summary.idle += 1,
                "queued" | "waiting" => summary.queued += 1,
                "done" | "posted" => summary.done += 1,
                _ => {}
            }
        }
        WorkersResponse {
            workers: workers.to_vec(),
            summary,
        }
    }
}

#[derive(Serialize)]
pub struct StatsResponse {
    pub merged_count: u64,
    pub failed_count: u64,
    pub active_count: u64,
    pub avg_merge_secs: Option<u64>,
}

#[derive(Serialize)]
pub struct ConfigResponse {
    pub merge_policy: String,
    pub auto_review: bool,
    pub review_timeout_secs: u64,
    pub auto_relaunch: bool,
    pub max_relaunch_attempts: u32,
    pub stale_timeout_secs: u64,
    pub max_concurrent: usize,
}

#[derive(serde::Deserialize)]
pub struct LaunchIssueRequest {
    pub issue_num: u64,
}

#[derive(serde::Deserialize)]
pub struct LaunchDirectRequest {
    pub prompt: String,
}

#[derive(serde::Deserialize)]
pub struct SendTextRequest {
    pub text: String,
}
