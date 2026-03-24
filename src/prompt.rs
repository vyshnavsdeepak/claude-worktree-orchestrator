use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

use std::os::unix::fs::PermissionsExt;

use crate::builder;
use crate::builder::launch_worker;
use crate::config::Config;
use crate::events::EventLog;
use crate::github;
use crate::state::StateDir;

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
    state_dir: Arc<StateDir>,
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
            &state_dir,
            None,
            false,
            None,
        )
        .await;
    }
}

/// Spin up a worker directly for an existing issue number.
#[allow(clippy::too_many_arguments)]
pub async fn run_new_job(
    config: Arc<Config>,
    issue_num: u64,
    log_tx: mpsc::UnboundedSender<String>,
    event_log: EventLog,
    state_dir: Arc<StateDir>,
    branch_override: Option<String>,
    plan_mode: bool,
    base_branch: Option<String>,
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

    launch_worker(
        &config,
        issue_num,
        &title,
        &body,
        &log_tx,
        &event_log,
        &state_dir,
        branch_override.as_deref(),
        plan_mode,
        base_branch.as_deref(),
    )
    .await;
}

/// Resolve a branch conflict by reusing the existing branch.
pub async fn resolve_reuse(
    config: Arc<Config>,
    issue_num: u64,
    log_tx: mpsc::UnboundedSender<String>,
    event_log: EventLog,
    state_dir: Arc<StateDir>,
) {
    let (title, body) = match github::get_issue(&config.repo, issue_num).await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!(
                "[resolve] Cannot reuse branch for #{issue_num}: failed to fetch issue from GitHub: {e}"
            );
            log(&log_tx, &msg);
            toast(&log_tx, "ERROR", &msg);
            return;
        }
    };

    let branch = config.branch_name_with_title(issue_num, &title);
    let worktree = config.worktree_path(issue_num);
    if !Path::new(&worktree).exists() {
        log(
            &log_tx,
            format!("[resolve] Attaching worktree to existing branch '{branch}' for #{issue_num}"),
        );
        if let Err(e) = builder::reuse_worktree(&config, issue_num, &title).await {
            log(&log_tx, format!("[resolve] {e}"));
            toast(
                &log_tx,
                "ERROR",
                &format!("Reuse failed for #{issue_num} — see log for details"),
            );
            return;
        }
        log(
            &log_tx,
            format!("[resolve] Worktree created at '{worktree}' using existing branch '{branch}'"),
        );
    }

    builder::launch_worker(
        &config, issue_num, &title, &body, &log_tx, &event_log, &state_dir, None, false, None,
    )
    .await;
}

/// Resolve a branch conflict by deleting the old branch and creating fresh.
pub async fn resolve_reset(
    config: Arc<Config>,
    issue_num: u64,
    log_tx: mpsc::UnboundedSender<String>,
    event_log: EventLog,
    state_dir: Arc<StateDir>,
) {
    let (title, body) = match github::get_issue(&config.repo, issue_num).await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!(
                "[resolve] Cannot reset branch for #{issue_num}: failed to fetch issue from GitHub: {e}"
            );
            log(&log_tx, &msg);
            toast(&log_tx, "ERROR", &msg);
            return;
        }
    };

    let branch = config.branch_name_with_title(issue_num, &title);
    let default_branch = config.default_branch();
    let worktree = config.worktree_path(issue_num);

    log(
        &log_tx,
        format!(
            "[resolve] Resetting #{issue_num}: removing worktree + deleting branch '{branch}', \
             recreating from origin/{default_branch}"
        ),
    );
    if let Err(e) = builder::reset_and_create_worktree(&config, issue_num, &title).await {
        log(&log_tx, format!("[resolve] {e}"));
        toast(
            &log_tx,
            "ERROR",
            &format!("Reset failed for #{issue_num} — see log for details"),
        );
        return;
    }
    log(
        &log_tx,
        format!("[resolve] Fresh worktree created at '{worktree}' from origin/{default_branch}"),
    );

    builder::launch_worker(
        &config, issue_num, &title, &body, &log_tx, &event_log, &state_dir, None, false, None,
    )
    .await;
}

/// Launch a worker directly from a prompt — no GitHub issue, no extraction.
/// Creates a worktree with a unique ID and launches Claude with the raw prompt.
pub async fn run_direct(
    config: Arc<Config>,
    prompt: String,
    log_tx: mpsc::UnboundedSender<String>,
    event_log: EventLog,
) {
    // Generate a unique ID from timestamp
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % 100_000; // keep it short

    let slug: String = prompt
        .chars()
        .take(40)
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    // Collapse consecutive dashes
    let mut collapsed = String::new();
    for c in slug.chars() {
        if c == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(c);
    }
    let slug = if collapsed.is_empty() {
        "task".to_string()
    } else {
        // Truncate to ~20 chars at a dash boundary for window name
        if collapsed.len() > 20 {
            match collapsed[..20].rfind('-') {
                Some(i) => collapsed[..i].to_string(),
                None => collapsed[..20].to_string(),
            }
        } else {
            collapsed
        }
    };

    let branch = format!("direct/{id}-{slug}");
    let window_name = format!("d-{slug}");
    let worktree = format!("{}/{}/{window_name}", config.repo_root, config.worktree_dir);

    // Create worktree from default branch
    let default_branch = config.default_branch();
    let start_point = format!("origin/{default_branch}");
    let out = tokio::process::Command::new("git")
        .args([
            "-C",
            &config.repo_root,
            "worktree",
            "add",
            &worktree,
            "-b",
            &branch,
            &start_point,
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            log(&log_tx, format!("[direct] Created worktree at {worktree}"));
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            log(&log_tx, format!("[direct] Worktree failed: {stderr}"));
            toast(&log_tx, "ERROR", "Failed to create worktree");
            return;
        }
        Err(e) => {
            log(&log_tx, format!("[direct] git error: {e}"));
            toast(&log_tx, "ERROR", "git worktree failed");
            return;
        }
    }

    // Create tmux window
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-session", "-d", "-s", &config.session])
        .output()
        .await;
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-window", "-t", &config.session, "-n", &window_name])
        .output()
        .await;

    // Launch Claude with the raw prompt
    let claude_prompt = format!(
        "{prompt}\n\nInstructions:\n\
        - Read the relevant source files first to understand the codebase\n\
        - Implement the requested changes\n\
        - Commit with a clear message (no Co-Authored-By)\n\
        - Push branch {branch}\n\
        - Open a PR to {default_branch} with a clear description\n\
        - Work autonomously, do not ask for confirmation"
    );

    let script_path = format!("/tmp/cwo-direct-{id}.sh");
    let flags = config.claude_flags.join(" ");
    let script = format!(
        "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude {flags} '{}'\n",
        worktree,
        claude_prompt.replace('\'', r"'\''")
    );
    if let Err(e) = std::fs::write(&script_path, &script) {
        log(&log_tx, format!("[direct] Failed to write script: {e}"));
        return;
    }
    let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));

    let target = format!("{}:{window_name}", config.session);
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["send-keys", "-t", &target, &script_path, "Enter"])
        .output()
        .await;

    let preview: String = prompt.chars().take(40).collect();
    log(
        &log_tx,
        format!("[direct] Launched worker in {window_name}: {preview}"),
    );
    toast(&log_tx, "SUCCESS", &format!("Launched {window_name}"));

    event_log.emit(
        "worker_launched",
        &[
            ("branch", serde_json::json!(branch)),
            ("direct_prompt", serde_json::json!(preview)),
        ],
    );
}
