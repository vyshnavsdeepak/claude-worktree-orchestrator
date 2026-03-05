use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, Duration};

use crate::config::Config;
use crate::events::EventLog;
use crate::poller::{self, DagState, WorkerState};

fn toast(tx: &mpsc::UnboundedSender<String>, level: &str, msg: &str) {
    let _ = tx.send(format!("__TOAST_{level}_{msg}__"));
}

fn log(tx: &mpsc::UnboundedSender<String>, msg: impl Into<String>) {
    let _ = tx.send(msg.into());
}

/// Return names of tasks eligible to launch: not yet launched, all deps completed.
fn eligible_tasks(config: &Config, state: &DagState) -> Vec<String> {
    config
        .tasks
        .iter()
        .filter(|t| !state.launched.contains(&t.name))
        .filter(|t| t.depends_on.iter().all(|d| state.completed.contains(d)))
        .map(|t| t.name.clone())
        .collect()
}

/// Launch a DAG task worker. Similar to prompt::run_direct but with known task name.
async fn launch_task(
    config: &Arc<Config>,
    task_name: &str,
    prompt: &str,
    log_tx: &mpsc::UnboundedSender<String>,
    event_log: &EventLog,
) {
    let branch = config.task_branch_name(task_name);
    let worktree = config.task_worktree_path(task_name);
    let window_name = config.task_window_name(task_name);

    // Create worktree
    if std::path::Path::new(&worktree).exists() {
        log(
            log_tx,
            format!("[dag] Worktree {worktree} already exists, reusing"),
        );
    } else {
        let out = tokio::process::Command::new("git")
            .args([
                "-C",
                &config.repo_root,
                "worktree",
                "add",
                &worktree,
                "-b",
                &branch,
            ])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {
                log(log_tx, format!("[dag] Created worktree at {worktree}"));
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                log(log_tx, format!("[dag] Worktree failed: {stderr}"));
                toast(
                    log_tx,
                    "ERROR",
                    &format!("Task '{task_name}' worktree failed"),
                );
                return;
            }
            Err(e) => {
                log(log_tx, format!("[dag] git error: {e}"));
                toast(log_tx, "ERROR", &format!("Task '{task_name}' git error"));
                return;
            }
        }
    }

    // Create tmux session/window
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-session", "-d", "-s", &config.session])
        .output()
        .await;
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-window", "-t", &config.session, "-n", &window_name])
        .output()
        .await;

    // Build the claude prompt
    let claude_prompt = format!(
        "{prompt}\n\nInstructions:\n\
        - Read the relevant source files first to understand the codebase\n\
        - Implement the requested changes\n\
        - Commit with a clear message (no Co-Authored-By)\n\
        - Push branch {branch}\n\
        - Open a PR to main with a clear description\n\
        - Work autonomously, do not ask for confirmation"
    );

    let script_path = format!("/tmp/cwo-task-{task_name}.sh");
    let flags = config.claude_flags.join(" ");
    let script = format!(
        "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude {flags} '{}'\n",
        worktree,
        claude_prompt.replace('\'', r"'\''")
    );
    if let Err(e) = std::fs::write(&script_path, &script) {
        log(log_tx, format!("[dag] Failed to write script: {e}"));
        return;
    }
    let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));

    let target = format!("{}:{window_name}", config.session);
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["send-keys", "-t", &target, &script_path, "Enter"])
        .output()
        .await;

    log(
        log_tx,
        format!("[dag] Launched task '{task_name}' in {window_name}"),
    );
    toast(log_tx, "SUCCESS", &format!("Launched task: {task_name}"));

    event_log.emit(
        "worker_launched",
        &[
            ("branch", serde_json::json!(branch)),
            ("task", serde_json::json!(task_name)),
        ],
    );
}

/// Count currently active workers (both issue-based and DAG task workers).
fn count_running(workers: &[WorkerState]) -> usize {
    workers
        .iter()
        .filter(|w| {
            matches!(
                w.status.as_str(),
                "active" | "idle" | "sleeping" | "probing"
            )
        })
        .count()
}

/// Main DAG scheduler loop.
pub async fn run(
    config: Arc<Config>,
    mut worker_rx: watch::Receiver<Vec<WorkerState>>,
    log_tx: mpsc::UnboundedSender<String>,
    event_log: EventLog,
) {
    log(
        &log_tx,
        format!(
            "[dag] Starting DAG scheduler with {} tasks",
            config.tasks.len()
        ),
    );

    // Load persisted state
    let mut state = poller::load_dag_state();

    loop {
        // Wait for worker state updates
        sleep(Duration::from_secs(config.poll_interval_secs.max(2))).await;

        let workers = worker_rx.borrow_and_update().clone();

        // Check for newly completed tasks.
        // A task is complete when its worker reaches a terminal state:
        // - "done"/"posted": PR created (typical for code tasks)
        // - "idle": Claude finished and is waiting at prompt (tests, analysis, etc.)
        // - "shell": Claude exited (task ran to completion)
        let mut changed = false;
        for task in &config.tasks {
            if state.launched.contains(&task.name) && !state.completed.contains(&task.name) {
                let wn = config.task_window_name(&task.name);
                if let Some(w) = workers.iter().find(|w| w.window_name == wn) {
                    if matches!(w.status.as_str(), "done" | "posted" | "idle" | "shell") {
                        log(
                            &log_tx,
                            format!(
                                "[dag] Task '{}' completed (status: {})",
                                task.name, w.status
                            ),
                        );
                        toast(&log_tx, "SUCCESS", &format!("Task '{}' done!", task.name));
                        state.completed.insert(task.name.clone());
                        changed = true;
                    }
                }
            }
        }

        // Find eligible tasks
        let eligible = eligible_tasks(&config, &state);
        if eligible.is_empty() {
            if changed {
                poller::save_dag_state(&state);
            }
            // Check if all tasks are complete
            if state.completed.len() == config.tasks.len() && !config.tasks.is_empty() {
                log(&log_tx, "[dag] All tasks complete!");
                toast(&log_tx, "SUCCESS", "All DAG tasks complete!");
                poller::save_dag_state(&state);
                // Keep running to stay visible in TUI
                sleep(Duration::from_secs(60)).await;
                continue;
            }
            continue;
        }

        // Launch eligible tasks respecting max_concurrent
        let running = count_running(&workers);
        let capacity = config.max_concurrent.saturating_sub(running);

        for task_name in eligible.iter().take(capacity) {
            let task = config.tasks.iter().find(|t| &t.name == task_name).unwrap();
            log(
                &log_tx,
                format!("[dag] Launching task '{}' (deps satisfied)", task_name),
            );
            launch_task(&config, task_name, &task.prompt, &log_tx, &event_log).await;
            state.launched.insert(task_name.clone());
            changed = true;
        }

        if eligible.len() > capacity {
            let queued: Vec<&str> = eligible.iter().skip(capacity).map(|s| s.as_str()).collect();
            log(
                &log_tx,
                format!(
                    "[dag] {} tasks queued (at capacity {}/{}): {}",
                    queued.len(),
                    running,
                    config.max_concurrent,
                    queued.join(", ")
                ),
            );
        }

        if changed {
            poller::save_dag_state(&state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TaskDef;

    fn test_config(tasks: Vec<TaskDef>) -> Config {
        Config {
            session: "test".into(),
            repo: "owner/repo".into(),
            discussion_issue: None,
            repo_root: "/tmp/repo".into(),
            tmux: "/usr/bin/tmux".into(),
            worktree_dir: ".claude/worktrees".into(),
            branch_prefix: "feature/issue-".into(),
            window_prefix: "issue-".into(),
            shell_prompts: vec!["$ ".into()],
            max_concurrent: 3,
            builder_sleep_secs: 300,
            poll_interval_secs: 1,
            run_builder: false,
            merge_policy: "auto".into(),
            auto_review: true,
            review_timeout_secs: 600,
            auto_relaunch: true,
            max_relaunch_attempts: 3,
            stale_timeout_secs: 300,
            claude_flags: vec!["--dangerously-skip-permissions".into()],
            tasks,
            issues: Vec::new(),
        }
    }

    #[test]
    fn eligible_tasks_returns_root_tasks() {
        let config = test_config(vec![
            TaskDef {
                name: "a".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "b".into(),
                prompt: "".into(),
                depends_on: vec!["a".into()],
            },
        ]);
        let state = DagState::default();
        let eligible = eligible_tasks(&config, &state);
        assert_eq!(eligible, vec!["a"]);
    }

    #[test]
    fn eligible_tasks_unblocks_after_dep_complete() {
        let config = test_config(vec![
            TaskDef {
                name: "a".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "b".into(),
                prompt: "".into(),
                depends_on: vec!["a".into()],
            },
        ]);
        let mut state = DagState::default();
        state.launched.insert("a".into());
        state.completed.insert("a".into());
        let eligible = eligible_tasks(&config, &state);
        assert_eq!(eligible, vec!["b"]);
    }

    #[test]
    fn eligible_tasks_skips_launched() {
        let config = test_config(vec![TaskDef {
            name: "a".into(),
            prompt: "".into(),
            depends_on: vec![],
        }]);
        let mut state = DagState::default();
        state.launched.insert("a".into());
        let eligible = eligible_tasks(&config, &state);
        assert!(eligible.is_empty());
    }

    #[test]
    fn eligible_tasks_waits_for_all_deps() {
        let config = test_config(vec![
            TaskDef {
                name: "a".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "b".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "c".into(),
                prompt: "".into(),
                depends_on: vec!["a".into(), "b".into()],
            },
        ]);
        let mut state = DagState::default();
        state.launched.insert("a".into());
        state.completed.insert("a".into());
        state.launched.insert("b".into());
        // b not completed yet
        let eligible = eligible_tasks(&config, &state);
        assert!(eligible.is_empty()); // c not eligible, b not done
    }

    #[test]
    fn fan_out_fan_in_works() {
        let config = test_config(vec![
            TaskDef {
                name: "a".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "b".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "c".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "summary".into(),
                prompt: "".into(),
                depends_on: vec!["a".into(), "b".into(), "c".into()],
            },
        ]);

        // Initially: a, b, c are eligible
        let state = DagState::default();
        let mut eligible = eligible_tasks(&config, &state);
        eligible.sort();
        assert_eq!(eligible, vec!["a", "b", "c"]);

        // After all 3 complete: summary is eligible
        let mut state = DagState::default();
        for n in &["a", "b", "c"] {
            state.launched.insert(n.to_string());
            state.completed.insert(n.to_string());
        }
        let eligible = eligible_tasks(&config, &state);
        assert_eq!(eligible, vec!["summary"]);
    }
}
