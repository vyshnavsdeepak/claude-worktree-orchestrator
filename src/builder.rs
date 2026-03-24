use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, Duration};

use crate::config::Config;
use crate::events::EventLog;
use crate::github;
use crate::monitor::BackoffState;
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

#[derive(Debug)]
pub struct BranchExistsError(pub u64);

impl std::fmt::Display for BranchExistsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "branch already exists for #{}", self.0)
    }
}

impl std::error::Error for BranchExistsError {}

pub fn is_rate_limited(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("try again in")
        || lower.contains("overloaded")
        || lower.contains("api error")
}

pub fn parse_retry_after(text: &str) -> u64 {
    let lower = text.to_lowercase();
    if let Some(pos) = lower.find("try again in ") {
        let after = &lower[pos + 13..];
        let num_end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if let Ok(n) = after[..num_end].parse::<u64>() {
            if after[num_end..].contains("minute") {
                return n * 60;
            }
            return n;
        }
    }
    120
}

pub fn parse_tasks(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && *l != "NONE")
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn build_prompt(discussion: &str, existing_str: &str) -> String {
    format!(
        r#"You are a pragmatic builder bot. Read this product discussion and extract 1-2 concrete implementable tasks not already filed.

{discussion}

EXISTING OPEN ISSUES (do not duplicate):
{existing_str}

Rules:
- Only output tasks that are concrete and implementable in code
- Skip anything vague or already covered by existing issues
- If nothing new and concrete, output exactly: NONE

For each task output one JSON per line (no other text):
{{"title": "Short imperative title", "body": "Detailed spec of what to implement and why"}}

Output ONLY json lines or NONE."#
    )
}

async fn create_worktree(
    config: &Config,
    issue_num: u64,
    title: &str,
    branch_override: Option<&str>,
    base_branch_override: Option<&str>,
) -> anyhow::Result<()> {
    let branch = branch_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.branch_name_with_title(issue_num, title));
    let worktree = config.worktree_path(issue_num);
    let default_branch = config.default_branch();
    let base = base_branch_override.unwrap_or(default_branch.as_str());
    let start_point = format!("origin/{base}");
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
        .await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("already exists") {
            return Err(BranchExistsError(issue_num).into());
        }
        anyhow::bail!(
            "git worktree add failed for #{issue_num} (branch '{branch}', path '{worktree}'): {stderr}"
        );
    }
    Ok(())
}

/// Attach worktree to an existing branch (no -b flag).
pub async fn reuse_worktree(config: &Config, issue_num: u64, title: &str) -> anyhow::Result<()> {
    let branch = config.branch_name_with_title(issue_num, title);
    let worktree = config.worktree_path(issue_num);
    let out = tokio::process::Command::new("git")
        .args([
            "-C",
            &config.repo_root,
            "worktree",
            "add",
            &worktree,
            &branch,
        ])
        .output()
        .await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!(
            "Reuse failed for #{issue_num}: could not attach worktree to existing branch '{branch}' at '{worktree}': {stderr}"
        );
    }
    Ok(())
}

/// Remove existing worktree, delete branch, then recreate fresh from origin/main.
pub async fn reset_and_create_worktree(
    config: &Config,
    issue_num: u64,
    title: &str,
) -> anyhow::Result<()> {
    let branch = config.branch_name_with_title(issue_num, title);
    let worktree = config.worktree_path(issue_num);
    let default_branch = config.default_branch();

    // Remove the worktree first (branch can't be deleted while checked out)
    if Path::new(&worktree).exists() {
        let out = tokio::process::Command::new("git")
            .args([
                "-C",
                &config.repo_root,
                "worktree",
                "remove",
                "--force",
                &worktree,
            ])
            .output()
            .await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            anyhow::bail!(
                "Reset failed for #{issue_num}: could not remove worktree at '{worktree}': {stderr}. \
                 Try manually: git worktree remove --force {worktree}"
            );
        }
    }
    // Prune stale worktree references so git branch -D doesn't think it's still checked out
    let _ = tokio::process::Command::new("git")
        .args(["-C", &config.repo_root, "worktree", "prune"])
        .output()
        .await;

    // Delete the local branch
    let out = tokio::process::Command::new("git")
        .args(["-C", &config.repo_root, "branch", "-D", &branch])
        .output()
        .await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!(
            "Reset failed for #{issue_num}: could not delete branch '{branch}': {stderr}. \
             Try manually: git branch -D {branch}"
        );
    }

    // Recreate fresh
    create_worktree(config, issue_num, title, None, None)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Reset failed for #{issue_num}: removed worktree and deleted branch '{branch}' \
                 but could not recreate from origin/{default_branch}: {e}"
            )
        })
}

#[allow(clippy::too_many_arguments)]
pub async fn launch_worker(
    config: &Arc<Config>,
    issue_num: u64,
    title: &str,
    body: &str,
    log_tx: &mpsc::UnboundedSender<String>,
    event_log: &EventLog,
    state_dir: &StateDir,
    branch_override: Option<&str>,
    plan_mode: bool,
    base_branch_override: Option<&str>,
) {
    let branch = branch_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.branch_name_with_title(issue_num, title));
    let worktree = config.worktree_path(issue_num);

    let worktree_existed = Path::new(&worktree).exists();
    if worktree_existed {
        log(
            log_tx,
            format!("[builder] Worktree {worktree} already exists, reusing"),
        );
    } else {
        match create_worktree(
            config,
            issue_num,
            title,
            branch_override,
            base_branch_override,
        )
        .await
        {
            Ok(()) => {
                log(log_tx, format!("[builder] Worktree created at {worktree}"));
                // Run post-worktree-create hooks
                if !config.post_worktree_create.is_empty() {
                    for cmd in &config.post_worktree_create {
                        let parts: Vec<&str> = cmd.split_whitespace().collect();
                        if let Some((prog, args)) = parts.split_first() {
                            let result = tokio::process::Command::new(prog)
                                .args(args)
                                .current_dir(&worktree)
                                .output()
                                .await;
                            let status = result
                                .as_ref()
                                .map(|o| o.status.to_string())
                                .unwrap_or_else(|e| e.to_string());
                            log(log_tx, format!("[builder] hook '{cmd}': {status}"));
                        }
                    }
                }
            }
            Err(e) => {
                if e.downcast_ref::<BranchExistsError>().is_some() {
                    log(
                        log_tx,
                        format!(
                            "[builder] Branch '{branch}' already exists for #{issue_num} — \
                             choose: reuse existing branch, reset (delete + recreate), or skip"
                        ),
                    );
                    let _ = log_tx.send(format!("__BRANCH_CONFLICT_{issue_num}__"));
                } else {
                    log(log_tx, format!("[builder] {e}"));
                }
                return;
            }
        }
    }

    let max_concurrent =
        crate::config::RuntimeConfig::effective_max_concurrent(config, &state_dir.runtime_config());
    let active = crate::monitor::count_active_workers(config).await;
    if active >= max_concurrent {
        let msg = format!(
            "#{issue_num} queued — at capacity ({active}/{max_concurrent}). Increase in settings (c)"
        );
        log(log_tx, format!("[builder] {msg}"));
        toast(log_tx, "WARN", &msg);
        return;
    }

    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-session", "-d", "-s", &config.session])
        .output()
        .await;

    let window = config.window_name(issue_num);

    // Check if a window with this name already exists
    let existing = tokio::process::Command::new(&config.tmux)
        .args([
            "list-windows",
            "-t",
            &config.session,
            "-F",
            "#{window_name}",
        ])
        .output()
        .await;
    if let Ok(out) = existing {
        let names = String::from_utf8_lossy(&out.stdout);
        if names.lines().any(|l| l.trim() == window) {
            // Window exists — check if it has an idle Claude REPL we can resume
            let target = format!("{}:{}", config.session, window);
            let pane_out = tokio::process::Command::new(&config.tmux)
                .args(["capture-pane", "-t", &target, "-p"])
                .output()
                .await;
            let is_claude_idle = pane_out
                .as_ref()
                .map(|o| {
                    let text = String::from_utf8_lossy(&o.stdout);
                    let last_lines: String =
                        text.lines().rev().take(5).collect::<Vec<_>>().join("\n");
                    last_lines.contains("❯") || last_lines.contains("bypass permissions")
                })
                .unwrap_or(false);

            if is_claude_idle {
                let resume_msg = format!(
                    "Continue working on issue #{issue_num}: {title}. Check your progress — if you haven't started, begin implementing. If you already have work in progress, continue from where you left off. Push and open a PR when done."
                );
                let _ = tokio::process::Command::new(&config.tmux)
                    .args(["send-keys", "-t", &target, "-l", &resume_msg])
                    .output()
                    .await;
                let _ = tokio::process::Command::new(&config.tmux)
                    .args(["send-keys", "-t", &target, "Enter"])
                    .output()
                    .await;
                log(
                    log_tx,
                    format!("[builder] Resumed existing Claude session in {window}"),
                );
            } else {
                log(
                    log_tx,
                    format!("[builder] Window {window} already exists (active), skipping"),
                );
            }
            return;
        }
    }

    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-window", "-t", &config.session, "-n", &window])
        .output()
        .await;

    let default_branch = config.default_branch();
    let claude_prompt = if plan_mode {
        format!(
            "Plan the implementation of GitHub issue #{issue_num} in this repo.\n\nTitle: {title}\n\nSpec:\n{body}\n\nInstructions:\n- Read the relevant source files to understand the codebase\n- Write a detailed implementation plan covering: which files to change, approach, key decisions, edge cases\n- After presenting the plan, stop. Do NOT write any code yet.\n- Do NOT commit, push, or open a PR.\n- Wait for further instructions."
        )
    } else {
        format!(
            "Implement GitHub issue #{issue_num} in this repo.\n\nTitle: {title}\n\nSpec:\n{body}\n\nInstructions:\n- Read the relevant source files first to understand the codebase\n- Implement the feature\n- Commit with a clear message (no Co-Authored-By)\n- Push branch {branch}\n- Open a PR to {default_branch} referencing #{issue_num} in the PR body\n- Work autonomously, do not ask for confirmation"
        )
    };

    let script_path = format!("/tmp/cwo-worker-{issue_num}.sh");
    let flags = config.claude_flags.join(" ");
    // For plan mode: start with no prompt — /plan is sent first, then task via @file
    // If worktree already existed, use --continue to resume the previous session
    let script = if worktree_existed {
        format!(
            "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude {flags} --continue\n",
            worktree
        )
    } else if plan_mode {
        format!(
            "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude {flags}\n",
            worktree
        )
    } else {
        format!(
            "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude {flags} '{}'\n",
            worktree,
            claude_prompt.replace('\'', "'\\''")
        )
    };
    if let Err(e) = std::fs::write(&script_path, &script) {
        log(
            log_tx,
            format!("[builder] Failed to write script {script_path}: {e}"),
        );
        return;
    }
    let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));

    let target = format!("{}:{window}", config.session);
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["send-keys", "-t", &target, &script_path, "Enter"])
        .output()
        .await;

    log(
        log_tx,
        format!(
            "[builder] Launched Claude in {}:{window} for issue #{issue_num}",
            config.session
        ),
    );

    // For plan mode: wait for Claude to be ready → /plan → wait → send task via @file
    if plan_mode {
        // Write task to file to avoid multi-line send-keys issues
        let task_file = format!("/tmp/cwo-task-{issue_num}.md");
        let _ = std::fs::write(&task_file, &claude_prompt);

        let config2 = config.clone();
        let log_tx2 = log_tx.clone();
        let target2 = target.clone();
        tokio::spawn(async move {
            // Helper: poll until ❯ is visible and Claude is not streaming
            let wait_for_idle = |target: String, config: std::sync::Arc<crate::config::Config>| {
                Box::pin(async move {
                    for _ in 0..150u32 {
                        // up to 5 min
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        let out = tokio::process::Command::new(&config.tmux)
                            .args(["capture-pane", "-t", &target, "-p", "-S", "-50"])
                            .output()
                            .await;
                        if let Ok(o) = out {
                            let text = String::from_utf8_lossy(&o.stdout);
                            let idle = text.contains('❯')
                                && !text.contains("esc to interrupt")
                                && !text.contains("Philosophising")
                                && !text.contains("Dilly-dallying");
                            if idle {
                                return true;
                            }
                        }
                    }
                    false
                })
            };

            // Step 1: wait for Claude to be ready
            if !wait_for_idle(target2.clone(), config2.clone()).await {
                log(
                    &log_tx2,
                    format!("[builder] Timed out waiting for Claude in {target2}"),
                );
                return;
            }

            // Step 2: send /plan
            let _ = tokio::process::Command::new(&config2.tmux)
                .args(["send-keys", "-t", &target2, "-l", "/plan"])
                .output()
                .await;
            let _ = tokio::process::Command::new(&config2.tmux)
                .args(["send-keys", "-t", &target2, "Enter"])
                .output()
                .await;
            log(&log_tx2, format!("[builder] Sent /plan to {target2}"));

            // Step 3: wait for plan-mode to be acknowledged
            if !wait_for_idle(target2.clone(), config2.clone()).await {
                log(
                    &log_tx2,
                    format!("[builder] Timed out waiting for /plan ack in {target2}"),
                );
                return;
            }

            // Step 4: tell Claude to read the task file (single line, avoids @-picker and newline issues)
            let task_msg = format!("Read the task at {task_file} and follow the instructions.");
            let _ = tokio::process::Command::new(&config2.tmux)
                .args(["send-keys", "-t", &target2, "-l", &task_msg])
                .output()
                .await;
            let _ = tokio::process::Command::new(&config2.tmux)
                .args(["send-keys", "-t", &target2, "Enter"])
                .output()
                .await;
            log(
                &log_tx2,
                format!("[builder] Sent task to {target2} in plan mode"),
            );
        });
    }

    event_log.emit(
        "worker_launched",
        &[
            ("issue", serde_json::json!(issue_num)),
            ("branch", serde_json::json!(branch)),
        ],
    );
}

async fn process_task(
    config: &Arc<Config>,
    task: &Task,
    log_tx: &mpsc::UnboundedSender<String>,
    event_log: &EventLog,
    state_dir: &StateDir,
) {
    log(log_tx, format!("[builder] Creating issue: {}", task.title));

    let issue_num = match github::create_issue(&config.repo, &task.title, &task.body).await {
        Ok(n) => n,
        Err(e) => {
            log(log_tx, format!("[builder] Error creating issue: {e}"));
            return;
        }
    };

    log(log_tx, format!("[builder] Created issue #{issue_num}"));
    event_log.emit(
        "issue_created",
        &[
            ("issue", serde_json::json!(issue_num)),
            ("title", serde_json::json!(task.title)),
        ],
    );
    let title_preview: String = task.title.chars().take(30).collect();
    toast(
        log_tx,
        "SUCCESS",
        &format!("Filed #{issue_num}: {title_preview}"),
    );

    if let Some(disc) = config.discussion_issue {
        let comment = format!(
            "🤖 **Builder:** Picked this up → created #{}: **{}**. Spinning up a worktree now.",
            issue_num, task.title
        );
        let _ = github::post_comment(&config.repo, disc, &comment).await;
    }

    launch_worker(
        config,
        issue_num,
        &task.title,
        &task.body,
        log_tx,
        event_log,
        state_dir,
        None,
        false,
        None,
    )
    .await;
}

async fn handle_command(
    config: &Arc<Config>,
    cmd: &str,
    log_tx: &mpsc::UnboundedSender<String>,
    event_log: &EventLog,
    state_dir: &StateDir,
) {
    let lower = cmd.to_lowercase();

    if lower.starts_with("merge pr ") {
        let pr_num_str = cmd["merge pr ".len()..].trim();
        if let Ok(pr_num) = pr_num_str.parse::<u64>() {
            log(log_tx, format!("[builder] Merging PR #{pr_num} via gh"));
            match github::merge_pr(&config.repo, pr_num).await {
                Ok(()) => {
                    log(log_tx, format!("[builder] PR #{pr_num} merged"));
                    toast(log_tx, "SUCCESS", &format!("Merged PR #{pr_num}!"));
                }
                Err(e) => {
                    log(log_tx, format!("[builder] PR #{pr_num} merge failed: {e}"));
                    toast(log_tx, "ERROR", &format!("PR #{pr_num} merge failed"));
                }
            }
        }
    } else if lower.contains("merge all") || lower.contains("merge prs") {
        log(log_tx, "[builder] Command: checking and merging open PRs");
        crate::monitor::check_and_merge_open_prs(config, log_tx, event_log, state_dir).await;
    } else if lower.contains("rebase all") {
        log(log_tx, "[builder] Command: triggering rebase");
        crate::monitor::notify_rebase(config, log_tx, state_dir).await;
    } else if lower.starts_with("nudge all") || lower.starts_with("broadcast ") {
        let msg = if lower.starts_with("broadcast ") {
            &cmd["broadcast ".len()..]
        } else {
            "continue with the task"
        };
        log(
            log_tx,
            format!("[builder] Broadcasting to idle workers: {msg}"),
        );
        let windows = crate::monitor::list_windows(config).await;
        for (idx, _) in &windows {
            let target = format!("{}:{}", config.session, idx);
            let pane = tokio::process::Command::new(&config.tmux)
                .args(["capture-pane", "-t", &target, "-p"])
                .output()
                .await
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            if pane.contains("bypass permissions on") {
                let _ = tokio::process::Command::new(&config.tmux)
                    .args(["send-keys", "-t", &target, msg, "Enter"])
                    .output()
                    .await;
            }
        }
    } else {
        log(
            log_tx,
            format!("[builder] Unrecognized command (logged only): {cmd}"),
        );
    }
}

pub async fn run(
    config: Arc<Config>,
    log_tx: mpsc::UnboundedSender<String>,
    backoff: Arc<Mutex<BackoffState>>,
    mut cmd_rx: mpsc::UnboundedReceiver<String>,
    event_log: EventLog,
    state_dir: Arc<StateDir>,
) {
    log(&log_tx, "[builder] Starting builder loop...");

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            log(&log_tx, format!("[builder] Command received: {cmd}"));
            handle_command(&config, &cmd, &log_tx, &event_log, &state_dir).await;
        }

        {
            let state = backoff.lock().await;
            if state.in_backoff() {
                let remaining = state.remaining_secs();
                log(
                    &log_tx,
                    format!("[builder] In backoff, {remaining}s remaining. Sleeping 30s..."),
                );
                drop(state);
                sleep(Duration::from_secs(30)).await;
                continue;
            }
        }

        crate::monitor::resume_after_backoff(&config, &backoff, &log_tx).await;

        let discussion_issue = match config.discussion_issue {
            Some(n) => n,
            None => {
                log(
                    &log_tx,
                    "[builder] No discussion_issue set, skipping task extraction",
                );
                sleep(Duration::from_secs(config.builder_sleep_secs)).await;
                continue;
            }
        };

        log(&log_tx, "[builder] Reading discussion...");
        let discussion = match github::get_discussion(&config.repo, discussion_issue).await {
            Ok(d) => d,
            Err(e) => {
                log(&log_tx, format!("[builder] Error reading discussion: {e}"));
                sleep(Duration::from_secs(30)).await;
                continue;
            }
        };

        let existing = match github::list_open_issues(&config.repo).await {
            Ok(e) => e,
            Err(e) => {
                log(&log_tx, format!("[builder] Error listing issues: {e}"));
                sleep(Duration::from_secs(30)).await;
                continue;
            }
        };

        let existing_str = existing
            .iter()
            .map(|(n, t)| format!("#{n}: {t}"))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = build_prompt(&discussion, &existing_str);

        log(&log_tx, "[builder] Calling Claude to extract tasks...");
        let tasks_output = match github::invoke_claude(&prompt).await {
            Ok(t) => t,
            Err(e) => {
                let err_str = e.to_string();
                if is_rate_limited(&err_str) {
                    let wait = parse_retry_after(&err_str);
                    log(
                        &log_tx,
                        format!("[builder] Rate limited, backing off {wait}s"),
                    );
                    toast(&log_tx, "WARNING", &format!("Rate limited — {wait}s"));
                    backoff.lock().await.set(wait);
                    sleep(Duration::from_secs(30)).await;
                    continue;
                }
                log(&log_tx, format!("[builder] Claude error: {e}"));
                sleep(Duration::from_secs(30)).await;
                continue;
            }
        };

        let preview: String = tasks_output.chars().take(120).collect();
        log(&log_tx, format!("[builder] Claude returned: {preview}"));

        if !tasks_output.trim().is_empty() && tasks_output.trim() != "NONE" {
            let raw_tasks = parse_tasks(&tasks_output);
            if let Some(raw) = raw_tasks.into_iter().next() {
                if let Ok(task) = serde_json::from_value::<Task>(raw) {
                    process_task(&config, &task, &log_tx, &event_log, &state_dir).await;
                } else {
                    log(
                        &log_tx,
                        "[builder] No valid task JSON found in Claude output.",
                    );
                }
            }
        } else {
            log(&log_tx, "[builder] No new tasks.");
        }

        log(&log_tx, "[builder] Writing builder status...");
        crate::monitor::write_builder_status(&config, &log_tx, &state_dir.builder_status()).await;

        log(&log_tx, "[builder] Checking worker health...");
        crate::monitor::check_worker_health(&config, &log_tx, &event_log, &state_dir).await;

        log(&log_tx, "[builder] Monitoring windows...");
        crate::monitor::monitor_windows(&config, &backoff, &log_tx, &state_dir).await;

        log(&log_tx, "[builder] Promoting orphaned worktrees...");
        crate::monitor::promote_orphaned_worktrees(&config, &log_tx, &state_dir).await;

        log(&log_tx, "[builder] Checking for merged PRs...");
        crate::monitor::notify_rebase(&config, &log_tx, &state_dir).await;

        log(&log_tx, "[builder] Checking and merging open PRs...");
        crate::monitor::check_and_merge_open_prs(&config, &log_tx, &event_log, &state_dir).await;

        if state_dir.just_merged().exists() {
            log(
                &log_tx,
                "[builder] Merge detected — triggering immediate rebase...",
            );
            crate::monitor::notify_rebase(&config, &log_tx, &state_dir).await;
            let _ = log_tx.send("__NEXT_SCAN_30__".to_string());
            sleep(Duration::from_secs(30)).await;
            continue;
        }

        log(&log_tx, "[builder] Cleaning up orphaned worktrees...");
        crate::monitor::cleanup_orphaned_worktrees(&config, &log_tx).await;

        log(&log_tx, "[builder] Cleaning up finished windows...");
        crate::monitor::cleanup_finished(&config, &log_tx).await;

        let _ = log_tx.send(format!("__NEXT_SCAN_{}__", config.builder_sleep_secs));
        log(
            &log_tx,
            format!(
                "[builder] Sleeping {}s before next scan...",
                config.builder_sleep_secs
            ),
        );
        sleep(Duration::from_secs(config.builder_sleep_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_rate_limited_detects_rate_limit() {
        assert!(is_rate_limited("Error: rate limit exceeded"));
        assert!(is_rate_limited("HTTP 429 Too Many Requests"));
        assert!(is_rate_limited("try again in 60 seconds"));
        assert!(is_rate_limited("Service overloaded"));
    }

    #[test]
    fn is_rate_limited_false_for_normal_error() {
        assert!(!is_rate_limited("connection refused"));
        assert!(!is_rate_limited("parse error in JSON"));
    }

    #[test]
    fn parse_retry_after_seconds() {
        assert_eq!(parse_retry_after("try again in 45 seconds"), 45);
    }

    #[test]
    fn parse_retry_after_minutes() {
        assert_eq!(parse_retry_after("try again in 2 minutes"), 120);
    }

    #[test]
    fn parse_retry_after_defaults_when_no_match() {
        assert_eq!(parse_retry_after("no timing info"), 120);
    }

    #[test]
    fn parse_tasks_returns_valid_json_objects() {
        let output = r#"{"title":"Add foo","body":"Implement foo bar"}
{"title":"Add baz","body":"Implement baz qux"}"#;
        let tasks = parse_tasks(output);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["title"], "Add foo");
    }

    #[test]
    fn parse_tasks_skips_none_and_empty() {
        let output = "NONE\n\n";
        let tasks = parse_tasks(output);
        assert!(tasks.is_empty());
    }

    #[test]
    fn parse_tasks_skips_invalid_json() {
        let output = "not json\n{\"title\":\"Valid\",\"body\":\"ok\"}";
        let tasks = parse_tasks(output);
        assert_eq!(tasks.len(), 1);
    }
}
