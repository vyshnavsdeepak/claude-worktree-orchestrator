use std::sync::Arc;
use tokio::sync::mpsc;

use crate::builder::launch_worker;
use crate::config::Config;
use crate::events::EventLog;
use crate::github;

fn toast(tx: &mpsc::UnboundedSender<String>, level: &str, msg: &str) {
    let _ = tx.send(format!("__TOAST_{level}_{msg}__"));
}

fn log(tx: &mpsc::UnboundedSender<String>, msg: impl Into<String>) {
    let _ = tx.send(msg.into());
}

#[derive(serde::Deserialize)]
struct Task {
    title: String,
    body: String,
}

fn parse_tasks(output: &str) -> Vec<Task> {
    output
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && *l != "NONE")
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Free-form prompt: Claude extracts tasks, files issues, spins up workers.
pub async fn run(
    config: Arc<Config>,
    prompt: String,
    log_tx: mpsc::UnboundedSender<String>,
    event_log: EventLog,
) {
    toast(&log_tx, "INFO", "Parsing with Claude...");

    let system_prompt = format!(
        r#"Extract 1-3 concrete implementable GitHub issue tasks from this request:
{prompt}
Output one JSON per line or NONE:
{{"title": "...", "body": "..."}}"#
    );

    let output = match github::invoke_claude(&system_prompt).await {
        Ok(o) => o,
        Err(e) => {
            log(&log_tx, format!("[prompt] Claude error: {e}"));
            toast(&log_tx, "ERROR", "Claude failed");
            return;
        }
    };

    if output.trim().is_empty() || output.trim() == "NONE" {
        toast(&log_tx, "INFO", "No tasks extracted");
        return;
    }

    let tasks = parse_tasks(&output);
    if tasks.is_empty() {
        toast(&log_tx, "INFO", "No valid tasks found");
        return;
    }

    for task in &tasks {
        let issue_num = match github::create_issue(&config.repo, &task.title, &task.body).await {
            Ok(n) => n,
            Err(e) => {
                log(&log_tx, format!("[prompt] Error creating issue: {e}"));
                toast(&log_tx, "ERROR", "Failed to create issue");
                continue;
            }
        };

        let title_preview: String = task.title.chars().take(30).collect();
        toast(
            &log_tx,
            "SUCCESS",
            &format!("Filed #{issue_num}: {title_preview}"),
        );

        launch_worker(
            &config,
            issue_num,
            &task.title,
            &task.body,
            &log_tx,
            &event_log,
        )
        .await;
    }
}

/// Spin up a worker directly for an existing issue number.
pub async fn run_new_job(
    config: Arc<Config>,
    issue_num: u64,
    log_tx: mpsc::UnboundedSender<String>,
    event_log: EventLog,
) {
    toast(
        &log_tx,
        "INFO",
        &format!("Launching worker for #{issue_num}..."),
    );

    let (title, body) = match github::get_issue(&config.repo, issue_num).await {
        Ok(r) => r,
        Err(e) => {
            log(
                &log_tx,
                format!("[prompt] Error fetching issue #{issue_num}: {e}"),
            );
            toast(&log_tx, "ERROR", &format!("Failed to fetch #{issue_num}"));
            return;
        }
    };

    launch_worker(&config, issue_num, &title, &body, &log_tx, &event_log).await;
}
