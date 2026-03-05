use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::config::Config;
use crate::events::EventLog;
use crate::github;

// ─── BackoffState ────────────────────────────────────────────────────────────

const BACKOFF_FILE: &str = "/tmp/cwo-backoff-until.txt";
const BACKOFF_RESUMED_FILE: &str = "/tmp/cwo-resumed.txt";

pub struct BackoffState {
    until_unix: u64,
    pub needs_resume: bool,
}

impl BackoffState {
    pub fn new() -> Self {
        let until_unix = std::fs::read_to_string(BACKOFF_FILE)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Self {
            until_unix,
            needs_resume: false,
        }
    }

    pub fn in_backoff(&self) -> bool {
        self.until_unix > now_unix()
    }

    pub fn set(&mut self, wait_secs: u64) {
        self.until_unix = now_unix() + wait_secs + 30;
        let _ = std::fs::write(BACKOFF_FILE, self.until_unix.to_string());
        let _ = std::fs::write(BACKOFF_RESUMED_FILE, "");
        self.needs_resume = true;
    }

    pub fn clear(&mut self) {
        self.until_unix = 0;
        let _ = std::fs::remove_file(BACKOFF_FILE);
    }

    pub fn remaining_secs(&self) -> i64 {
        self.until_unix as i64 - now_unix() as i64
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn log(tx: &mpsc::UnboundedSender<String>, msg: impl Into<String>) {
    let _ = tx.send(msg.into());
}

fn toast(tx: &mpsc::UnboundedSender<String>, level: &str, msg: &str) {
    let _ = tx.send(format!("__TOAST_{level}_{msg}__"));
}

/// Read the current branch name from a worktree directory.
/// Falls back to the computed branch name if git fails.
async fn worktree_branch(worktree: &str, fallback: &str) -> String {
    match tokio::process::Command::new("git")
        .args(["-C", worktree, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if branch.is_empty() || branch == "HEAD" {
                fallback.to_string()
            } else {
                branch
            }
        }
        _ => fallback.to_string(),
    }
}

async fn capture_pane(config: &Config, idx: usize) -> String {
    let target = format!("{}:{}", config.session, idx);
    let Ok(out) = tokio::process::Command::new(&config.tmux)
        .args(["capture-pane", "-t", &target, "-p", "-S", "-500"])
        .output()
        .await
    else {
        return String::new();
    };
    String::from_utf8_lossy(&out.stdout).to_string()
}

async fn send_keys(config: &Config, target: &str, text: &str) {
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["send-keys", "-t", target, text, "Enter"])
        .output()
        .await;
}

/// Send text to a Claude TUI pane. Uses literal mode (-l) for the text
/// and a separate Enter keystroke so Claude receives a proper submit
/// instead of a pasted newline.
async fn send_to_claude(config: &Config, target: &str, text: &str) {
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["send-keys", "-t", target, "-l", text])
        .output()
        .await;
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["send-keys", "-t", target, "Enter"])
        .output()
        .await;
}

/// Return the pane index of the probe (bottom split) pane for a window, if one exists.
/// With pane-base-index=1 the main pane is always index 1; a split probe pane
/// gets index 2+. Returns None when the window has only one pane.
async fn probe_pane_index(config: &Config, window_name: &str) -> Option<usize> {
    let out = tokio::process::Command::new(&config.tmux)
        .args([
            "list-panes",
            "-t",
            &format!("{}:{}", config.session, window_name),
            "-F",
            "#{pane_index}",
        ])
        .output()
        .await
        .ok()?;
    let mut indices: Vec<usize> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<usize>().ok())
        .collect();
    indices.sort_unstable();
    if indices.len() >= 2 {
        Some(*indices.last().unwrap())
    } else {
        None
    }
}

/// Spawn a non-interactive `claude --print` in a split pane (bottom 35%) of
/// the given window. The top pane (interactive Claude or shell) is left untouched.
pub async fn send_print_pane(
    config: &Config,
    window_name: &str,
    worktree: &str,
    prompt: &str,
    log_tx: &mpsc::UnboundedSender<String>,
) {
    let win_target = format!("{}:{}", config.session, window_name);

    // Kill any existing probe pane (only if a second pane actually exists)
    if let Some(idx) = probe_pane_index(config, window_name).await {
        let _ = tokio::process::Command::new(&config.tmux)
            .args(["kill-pane", "-t", &format!("{win_target}.{idx}")])
            .output()
            .await;
    }

    // Create bottom split (35% height), don't steal focus from top pane.
    // -P -F prints the new pane's index so we know exactly where to send keys.
    let out = tokio::process::Command::new(&config.tmux)
        .args([
            "split-window",
            "-t",
            &win_target,
            "-v",
            "-p",
            "35",
            "-d",
            "-P",
            "-F",
            "#{pane_index}",
        ])
        .output()
        .await;
    let probe_idx = match out {
        Ok(o) if o.status.success() => {
            match String::from_utf8_lossy(&o.stdout).trim().parse::<usize>() {
                Ok(n) => n,
                Err(_) => {
                    log(
                        log_tx,
                        format!("[print] Could not parse new pane index for {window_name}"),
                    );
                    return;
                }
            }
        }
        _ => {
            log(
                log_tx,
                format!("[print] Could not split pane for {window_name}"),
            );
            return;
        }
    };

    let script_path = format!("/tmp/cwo-monitor-{window_name}.sh");
    let script = format!(
        "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nclaude --dangerously-skip-permissions --print '{}'\n",
        worktree,
        prompt.replace('\'', r"'\''"),
    );
    if std::fs::write(&script_path, &script).is_ok() {
        let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
    }

    let bottom = format!("{win_target}.{probe_idx}");
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["send-keys", "-t", &bottom, &script_path, "Enter"])
        .output()
        .await;

    log(
        log_tx,
        format!("[print] Spawned --print Claude in {window_name} pane {probe_idx}"),
    );
}

pub async fn bottom_pane_active(config: &Config, window_name: &str) -> bool {
    let Some(idx) = probe_pane_index(config, window_name).await else {
        return false;
    };
    let target = format!("{}:{}.{idx}", config.session, window_name);
    let current_cmd = tokio::process::Command::new(&config.tmux)
        .args([
            "display-message",
            "-t",
            &target,
            "-p",
            "#{pane_current_command}",
        ])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    !matches!(current_cmd.as_str(), "zsh" | "bash" | "sh" | "fish" | "")
}

/// Parse the last JSON object from a block of text (used to read --print output).
pub fn parse_print_json(output: &str) -> Option<serde_json::Value> {
    output
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .next()
}

pub async fn list_windows(config: &Config) -> Vec<(usize, String)> {
    let Ok(out) = tokio::process::Command::new(&config.tmux)
        .args([
            "list-windows",
            "-t",
            &config.session,
            "-F",
            "#{window_index}:#{window_name}",
        ])
        .output()
        .await
    else {
        return Vec::new();
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let mut windows = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(2, ':');
        if let (Some(idx_str), Some(name)) = (parts.next(), parts.next()) {
            if let Ok(idx) = idx_str.parse::<usize>() {
                // Skip plain shell windows that aren't issue workers
                if !name.starts_with(config.window_prefix.as_str()) {
                    continue;
                }
                windows.push((idx, name.to_string()));
            }
        }
    }
    windows
}

pub fn extract_issue_num(config: &Config, name: &str) -> Option<u64> {
    name.strip_prefix(config.window_prefix.as_str())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Extract issue number from a branch name like "feature/issue-326-fix-something".
/// Strips the branch_prefix, then takes digits before the first dash.
pub fn issue_num_from_branch(config: &Config, branch: &str) -> Option<u64> {
    let after_prefix = branch.strip_prefix(config.branch_prefix.as_str())?;
    // after_prefix is like "326-fix-something" or just "326"
    let num_part = after_prefix.split('-').next()?;
    num_part.parse::<u64>().ok()
}

fn classify_pane(config: &Config, pane: &str) -> &'static str {
    let spinner_words = [
        "Crunching",
        "Brewing",
        "Baking",
        "Cogitating",
        "Thinking",
        "Analyzing",
    ];
    if spinner_words.iter().any(|w| pane.contains(w)) {
        return "active";
    }
    if pane.contains("bypass permissions on") {
        return "claude_repl";
    }
    if config.is_shell_prompt(pane) {
        return "shell";
    }
    "unknown"
}

// ─── ISO 8601 helpers ─────────────────────────────────────────────────────────

fn unix_to_iso8601(ts: u64) -> String {
    let time = ts % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;
    let s = time % 60;
    let mut days = ts / 86400;

    let mut year = 1970u32;
    loop {
        let leap = is_leap(year);
        let days_in_year = if leap { 366u64 } else { 365u64 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let months = if is_leap(year) {
        [31u64, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31u64, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u32;
    for &dim in &months {
        if days < dim {
            break;
        }
        days -= dim;
        month += 1;
    }
    let day = days + 1;

    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn is_leap(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Load runtime config overrides, falling back to compiled Config values.
fn runtime_config(config: &Config) -> crate::config::RuntimeConfig {
    crate::config::RuntimeConfig::load()
        .unwrap_or_else(|| crate::config::RuntimeConfig::from_config(config))
}

// ─── Public functions ─────────────────────────────────────────────────────────

pub async fn count_active_workers(config: &Config) -> usize {
    let windows = list_windows(config).await;
    let mut count = 0;
    for (idx, _) in windows {
        let pane = capture_pane(config, idx).await;
        let s = classify_pane(config, &pane);
        if s == "active" || s == "claude_repl" {
            count += 1;
        }
    }
    count
}

pub async fn write_builder_status(config: &Config, _log_tx: &mpsc::UnboundedSender<String>) {
    let windows = list_windows(config).await;
    let mut prs: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for (_, name) in &windows {
        let Some(issue_num) = extract_issue_num(config, name) else {
            continue;
        };
        if let Ok(pr_nums) = github::list_prs_for_issue(&config.repo, issue_num).await {
            if let Some(&pr_num) = pr_nums.first() {
                prs.insert(name.clone(), format!("#{pr_num}"));
            }
        }
    }

    let status = serde_json::json!({ "prs": prs });
    if let Ok(json) = serde_json::to_string(&status) {
        let _ = std::fs::write("/tmp/cwo-status.json", json);
    }
}

fn build_monitor_prompt(
    config: &Config,
    issue_num: u64,
    worktree: &str,
    branch: &str,
    pane_log: &str,
    open_prs: &[u64],
    conflict: bool,
) -> String {
    let pr_info = if open_prs.is_empty() {
        "No open PRs found for this issue.".to_string()
    } else {
        let nums = open_prs
            .iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Open PR(s) for this issue: {nums}")
    };
    let conflict_note = if conflict {
        " NOTE: a rebase conflict was detected on this branch."
    } else {
        ""
    };
    let log_snippet: String = pane_log
        .lines()
        .rev()
        .take(60)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are an AI builder bot monitoring GitHub issue #{issue_num}. \
        Repo: {repo}. \
        Worktree: {worktree}. Branch: {branch}. \
        {pr_info}.{conflict_note} \
        \n\nHere is the current terminal log:\n---\n{log_snippet}\n---\n\
        \nBased on the log above:\
        \n1. What has this worker accomplished so far?\
        \n2. What is blocking progress or needs attention?\
        \n3. Take the necessary action now (use git, gh, or any shell commands).\
        \n   - If no PR: commit uncommitted work, push, gh pr create --base main --body 'Closes #{issue_num}'\
        \n   - If conflicts: git fetch origin && git rebase origin/main, resolve each file, git add, git rebase --continue, git push --force-with-lease origin HEAD\
        \n   - If PR open and CI clean: output done\
        \n   - If PR open and review needed: address the feedback\
        \nAt the end output exactly one JSON line (no other text after it):\
        \n{{\"issue\":{issue_num},\"status\":\"idle|working|done|conflict|stuck\",\"action_taken\":\"...\",\"pr\":null}}",
        repo = config.repo,
    )
}

fn build_review_prompt(
    config: &Config,
    issue_num: u64,
    pr_num: u64,
    worktree: &str,
    pane_log: &str,
    review_ctx: &str,
) -> String {
    let log_snippet: String = pane_log
        .lines()
        .rev()
        .take(30)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let _ = config;
    format!(
        "You are working on GitHub issue #{issue_num} in worktree {worktree}.\n\
        PR #{pr_num} is BLOCKED and cannot merge. Here is the review/CI context:\n\
        ---\n{review_ctx}\n---\n\
        Current terminal:\n---\n{log_snippet}\n---\n\
        Address every review comment and fix every CI failure shown above. Then:\n\
        - git add -A && git commit -m 'Address review feedback'\n\
        - git push --force-with-lease origin HEAD\n\
        At the end output exactly one JSON line:\n\
        {{\"issue\":{issue_num},\"pr\":{pr_num},\"status\":\"working|done\",\"action_taken\":\"...\"}}"
    )
}

pub async fn monitor_windows(
    config: &Config,
    _backoff: &Arc<Mutex<BackoffState>>,
    log_tx: &mpsc::UnboundedSender<String>,
) {
    let windows = list_windows(config).await;

    for (idx, name) in &windows {
        let Some(issue_num) = extract_issue_num(config, name) else {
            continue;
        };
        let pane = capture_pane(config, *idx).await;
        let state = classify_pane(config, &pane);

        if state == "active" {
            continue;
        }

        if bottom_pane_active(config, name).await {
            continue;
        }

        let worktree = config.worktree_path(issue_num);

        if state == "shell" && !std::path::Path::new(&worktree).exists() {
            continue;
        }

        if state == "shell" {
            let had_claude = pane.contains("claude") || pane.contains(&config.branch_prefix);
            if !had_claude {
                let active = count_active_workers(config).await;
                if active >= config.max_concurrent {
                    log(
                        log_tx,
                        format!("[monitor] #{issue_num}: at capacity, skipping"),
                    );
                    continue;
                }
                let fallback = config.branch_name(issue_num);
                let branch = worktree_branch(&worktree, &fallback).await;
                let claude_prompt = format!(
                    "Continue implementing GitHub issue #{issue_num}. Check git log, git status, existing code. Finish the implementation, commit, push {branch}, open a PR referencing #{issue_num}. Work autonomously."
                );
                let script_path = format!("/tmp/cwo-worker-{issue_num}.sh");
                let script = format!(
                    "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude --dangerously-skip-permissions '{}'\n",
                    worktree, claude_prompt.replace('\'', r"'\''")
                );
                if std::fs::write(&script_path, &script).is_ok() {
                    let _ = std::fs::set_permissions(
                        &script_path,
                        std::fs::Permissions::from_mode(0o755),
                    );
                    let target = format!("{}:{}", config.session, idx);
                    send_keys(config, &target, &script_path).await;
                    log(
                        log_tx,
                        format!("[monitor] #{issue_num}: relaunched interactive Claude"),
                    );
                    toast(log_tx, "WARNING", &format!("Relaunched #{issue_num}"));
                }
                continue;
            }
        }

        let pr_nums = github::list_prs_for_issue(&config.repo, issue_num)
            .await
            .unwrap_or_default();
        let conflict = has_conflict_marker(issue_num);

        log(
            log_tx,
            format!("[monitor] #{issue_num}: spawning AI log-reader probe (state={state})"),
        );
        toast(log_tx, "INFO", &format!("Reading #{issue_num} logs…"));

        let fallback = config.branch_name(issue_num);
        let branch = worktree_branch(&worktree, &fallback).await;
        let prompt = build_monitor_prompt(
            config, issue_num, &worktree, &branch, &pane, &pr_nums, conflict,
        );
        send_print_pane(config, name, &worktree, &prompt, log_tx).await;

        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }
}

pub async fn cleanup_finished(config: &Config, log_tx: &mpsc::UnboundedSender<String>) {
    let windows = list_windows(config).await;

    for (idx, name) in &windows {
        let Some(issue_num) = extract_issue_num(config, name) else {
            continue;
        };

        let state = github::get_issue_state(&config.repo, issue_num)
            .await
            .unwrap_or_default();
        if state != "CLOSED" {
            continue;
        }

        log(
            log_tx,
            format!("[cleanup] Issue #{issue_num} closed — removing window {idx} and worktree"),
        );
        toast(
            log_tx,
            "SUCCESS",
            &format!("Closed #{issue_num} — cleaned up"),
        );

        let worktree = config.worktree_path(issue_num);
        if std::path::Path::new(&worktree).exists() {
            let _ = tokio::process::Command::new("git")
                .args([
                    "-C",
                    &config.repo_root,
                    "worktree",
                    "remove",
                    "--force",
                    &worktree,
                ])
                .output()
                .await;
        }

        let target = format!("{}:{}", config.session, idx);
        let _ = tokio::process::Command::new(&config.tmux)
            .args(["kill-window", "-t", &target])
            .output()
            .await;

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

const REBASE_CHECK_FILE: &str = "/tmp/cwo-last-merge-check.txt";
pub const JUST_MERGED_FILE: &str = "/tmp/cwo-just-merged.txt";

async fn test_rebase(worktree: &str, issue_num: u64) -> bool {
    let out = tokio::process::Command::new("git")
        .args(["-C", worktree, "rebase", "origin/main"])
        .output()
        .await;

    let clean = out.map(|o| o.status.success()).unwrap_or(false);

    if !clean {
        let _ = tokio::process::Command::new("git")
            .args(["-C", worktree, "rebase", "--abort"])
            .output()
            .await;
        let _ = std::fs::write(
            format!("/tmp/cwo-issue-{issue_num}-conflict.txt"),
            "conflict",
        );
    } else {
        let _ = std::fs::remove_file(format!("/tmp/cwo-issue-{issue_num}-conflict.txt"));
    }

    clean
}

pub fn has_conflict_marker(issue_num: u64) -> bool {
    std::path::Path::new(&format!("/tmp/cwo-issue-{issue_num}-conflict.txt")).exists()
}

pub async fn notify_rebase(config: &Config, log_tx: &mpsc::UnboundedSender<String>) {
    let last_check = std::fs::read_to_string(REBASE_CHECK_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| unix_to_iso8601(now_unix().saturating_sub(600)));

    let now_ts = unix_to_iso8601(now_unix());
    let _ = std::fs::write(REBASE_CHECK_FILE, &now_ts);

    let merged = github::merged_prs_since(&config.repo, &last_check)
        .await
        .unwrap_or_default();

    let new_merges = !merged.is_empty();
    if new_merges {
        let merged_count = merged.len();
        let merged_titles: Vec<String> = merged.iter().map(|(n, t)| format!("#{n} {t}")).collect();
        log(
            log_tx,
            format!(
                "[rebase] Detected {merged_count} merged PR(s): {}",
                merged_titles.join(", ")
            ),
        );
        toast(
            log_tx,
            "INFO",
            &format!("{merged_count} PR(s) merged — checking conflicts"),
        );
    }

    // Always fetch latest main so test_rebase works against current upstream
    let _ = tokio::process::Command::new("git")
        .args([
            "-C",
            &config.repo_root,
            "fetch",
            "origin",
            "main",
            "--quiet",
        ])
        .output()
        .await;

    let windows = list_windows(config).await;
    for (idx, name) in &windows {
        let Some(issue_num) = extract_issue_num(config, name) else {
            continue;
        };

        // Skip if no new merges and no stale conflict marker — nothing changed
        let has_stale_conflict = has_conflict_marker(issue_num);
        if !new_merges && !has_stale_conflict {
            continue;
        }

        let worktree = config.worktree_path(issue_num);
        if !std::path::Path::new(&worktree).exists() {
            continue;
        }

        if bottom_pane_active(config, name).await {
            continue;
        }

        let clean = test_rebase(&worktree, issue_num).await;
        let pane = capture_pane(config, *idx).await;

        if !clean {
            log(
                log_tx,
                format!("[rebase] ⚠️  Issue #{issue_num}: CONFLICT — spawning AI resolver"),
            );
            toast(
                log_tx,
                "WARNING",
                &format!("#{issue_num} has rebase conflicts!"),
            );
        } else {
            log(
                log_tx,
                format!("[rebase] Issue #{issue_num}: rebased cleanly — spawning AI pusher"),
            );
        }

        let pr_nums = github::list_prs_for_issue(&config.repo, issue_num)
            .await
            .unwrap_or_default();
        let fallback = config.branch_name(issue_num);
        let branch = worktree_branch(&worktree, &fallback).await;
        let prompt = build_monitor_prompt(
            config, issue_num, &worktree, &branch, &pane, &pr_nums, !clean,
        );
        send_print_pane(config, name, &worktree, &prompt, log_tx).await;

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

/// Check all open PRs. Merges are serialized: one CLEAN/UNSTABLE merge per call, one
/// BEHIND rebase+merge per call. DIRTY and BLOCKED get probes (no early exit).
pub async fn check_and_merge_open_prs(
    config: &Config,
    log_tx: &mpsc::UnboundedSender<String>,
    event_log: &EventLog,
) {
    let mut prs = github::list_open_prs(&config.repo)
        .await
        .unwrap_or_default();
    if prs.is_empty() {
        return;
    }

    // Oldest PR first — deterministic ordering reduces conflict surface
    prs.sort_by_key(|(n, _)| *n);

    log(
        log_tx,
        format!("[merge] Checking {} open PR(s) (serial mode)...", prs.len()),
    );

    let _ = tokio::process::Command::new("git")
        .args([
            "-C",
            &config.repo_root,
            "fetch",
            "origin",
            "main",
            "--quiet",
        ])
        .output()
        .await;

    // Collect PR states upfront (one pass)
    let mut pr_states: Vec<(u64, String, String)> = Vec::new();
    for (pr_num, head_branch) in &prs {
        match github::get_pr_info(&config.repo, *pr_num, head_branch).await {
            Ok(info) => pr_states.push((*pr_num, head_branch.clone(), info.merge_state)),
            Err(e) => log(
                log_tx,
                format!("[merge] PR #{pr_num}: failed to get state: {e}"),
            ),
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    let rt = runtime_config(config);

    // ── Step 0: Spawn reviewers for any PRs not yet reviewed ────────────────
    if rt.auto_review {
        for (pr_num, head_branch, _) in &pr_states {
            if !pr_reviewed(*pr_num) {
                if let Some(issue_num) = issue_num_from_branch(config, head_branch) {
                    spawn_pr_review(config, issue_num, *pr_num, log_tx, event_log).await;
                }
            }
        }
    }

    // ── Step 1: Merge the oldest CLEAN/UNSTABLE PR and stop ─────────────────
    // UNSTABLE = non-required CI checks failing but still mergeable.
    if rt.merge_policy != "manual" {
        for (pr_num, _, state) in &pr_states {
            if state == "CLEAN" || state == "UNSTABLE" {
                // review_then_merge: check review state before merging
                if rt.merge_policy == "review_then_merge" {
                    match github::get_latest_review_state(&config.repo, *pr_num).await {
                        Ok(Some(ref s)) if s == "APPROVED" => {
                            log(
                                log_tx,
                                format!("[merge] PR #{pr_num} APPROVED — proceeding"),
                            );
                        }
                        Ok(Some(ref s)) if s == "CHANGES_REQUESTED" => {
                            log(
                                log_tx,
                                format!("[merge] PR #{pr_num} has CHANGES_REQUESTED — skipping"),
                            );
                            continue;
                        }
                        _ => {
                            // No review yet — check if review was spawned and how long ago
                            let review_marker =
                                std::path::Path::new(REVIEW_DIR).join(pr_num.to_string());
                            let spawned_ago = std::fs::metadata(&review_marker)
                                .ok()
                                .and_then(|m| m.modified().ok())
                                .map(|t| t.elapsed().unwrap_or_default().as_secs())
                                .unwrap_or(u64::MAX);

                            if rt.review_timeout_secs > 0 && spawned_ago < rt.review_timeout_secs {
                                log(
                                    log_tx,
                                    format!(
                                        "[merge] PR #{pr_num} awaiting review ({spawned_ago}s / {}s timeout)",
                                        rt.review_timeout_secs
                                    ),
                                );
                                continue;
                            }
                            log(
                                log_tx,
                                format!("[merge] PR #{pr_num} review timed out — merging anyway"),
                            );
                        }
                    }
                }

                log(
                    log_tx,
                    format!("[merge] PR #{pr_num} is {state} — merging (oldest first)"),
                );
                toast(log_tx, "INFO", &format!("Auto-merging PR #{pr_num}"));
                match github::merge_pr(&config.repo, *pr_num).await {
                    Ok(()) => {
                        log(log_tx, format!("[merge] PR #{pr_num} merged"));
                        toast(log_tx, "SUCCESS", &format!("Merged PR #{pr_num}!"));
                        event_log.emit("pr_merged", &[("pr", serde_json::json!(*pr_num))]);
                        // Signal builder loop to rebase immediately and loop in 30s
                        let _ = std::fs::write(JUST_MERGED_FILE, pr_num.to_string());
                    }
                    Err(e) => {
                        log(log_tx, format!("[merge] PR #{pr_num} merge failed: {e}"));
                        toast(log_tx, "ERROR", &format!("PR #{pr_num} merge failed"));
                    }
                }
                return; // one merge per cycle — prevents cascade conflicts
            }
        }
    } else {
        // Manual mode: just log CLEAN PRs, don't merge
        for (pr_num, _, state) in &pr_states {
            if state == "CLEAN" || state == "UNSTABLE" {
                log(
                    log_tx,
                    format!("[merge] PR #{pr_num} is {state} (manual mode — not merging)"),
                );
            }
        }
    }

    // ── Step 2: Handle the oldest BEHIND PR (rebase+push+poll+merge) ─────────
    for (pr_num, head_branch, state) in &pr_states {
        if state != "BEHIND" {
            continue;
        }
        log(
            log_tx,
            format!("[merge] PR #{pr_num} is BEHIND — rebasing (oldest first)"),
        );
        let Some(n) = head_branch
            .strip_prefix(config.branch_prefix.as_str())
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let worktree = config.worktree_path(n);
        if !std::path::Path::new(&worktree).exists() {
            continue;
        }
        let clean = test_rebase(&worktree, n).await;
        if clean {
            let push = tokio::process::Command::new("git")
                .args([
                    "-C",
                    &worktree,
                    "push",
                    "--force-with-lease",
                    "origin",
                    "HEAD",
                ])
                .output()
                .await;
            match push {
                Ok(o) if o.status.success() => {
                    log(
                        log_tx,
                        format!("[merge] PR #{pr_num}: rebased+pushed — polling for CLEAN"),
                    );
                    toast(log_tx, "INFO", &format!("PR #{pr_num} rebased+pushed"));
                    let _ = std::fs::remove_file(format!("/tmp/cwo-issue-{n}-conflict.txt"));
                    'poll: for attempt in 0u8..3 {
                        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                        if let Ok(fresh) = github::get_pr_info(&config.repo, *pr_num, "").await {
                            match fresh.merge_state.as_str() {
                                "CLEAN" | "UNSTABLE" => {
                                    match github::merge_pr(&config.repo, *pr_num).await {
                                        Ok(()) => {
                                            log(
                                                log_tx,
                                                format!("[merge] PR #{pr_num} merged after rebase"),
                                            );
                                            toast(
                                                log_tx,
                                                "SUCCESS",
                                                &format!("Merged PR #{pr_num}!"),
                                            );
                                            event_log.emit(
                                                "pr_merged",
                                                &[("pr", serde_json::json!(*pr_num))],
                                            );
                                            let _ = std::fs::write(
                                                JUST_MERGED_FILE,
                                                pr_num.to_string(),
                                            );
                                        }
                                        Err(e) => {
                                            log(
                                                log_tx,
                                                format!("[merge] PR #{pr_num} merge failed: {e}"),
                                            );
                                            toast(
                                                log_tx,
                                                "ERROR",
                                                &format!("PR #{pr_num} merge failed"),
                                            );
                                        }
                                    }
                                    break 'poll;
                                }
                                s if attempt < 2 => {
                                    log(log_tx, format!("[merge] PR #{pr_num}: state={s} (attempt {}), retrying...", attempt + 1));
                                }
                                s => {
                                    log(
                                        log_tx,
                                        format!("[merge] PR #{pr_num}: not CLEAN ({s}) after 30s"),
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {
                    let windows = list_windows(config).await;
                    for (idx, name) in &windows {
                        if extract_issue_num(config, name) == Some(n) {
                            let pane = capture_pane(config, *idx).await;
                            let state = classify_pane(config, &pane);
                            let target = format!("{}:{}", config.session, idx);
                            if state == "shell" {
                                let cmd = format!(
                                    "cd '{}' && git push --force-with-lease origin HEAD",
                                    worktree
                                );
                                send_keys(config, &target, &cmd).await;
                            } else if state == "claude_repl" {
                                send_to_claude(
                                    config,
                                    &target,
                                    "Branch rebased — run: git push --force-with-lease origin HEAD",
                                )
                                .await;
                            }
                            break;
                        }
                    }
                }
            }
        }
        return; // one BEHIND handled per cycle
    }

    // ── Step 3: DIRTY probes + BLOCKED reviews (all, no early exit) ──────────
    for (pr_num, head_branch, state) in &pr_states {
        let issue_num: Option<u64> = head_branch
            .strip_prefix(config.branch_prefix.as_str())
            .and_then(|s| s.parse().ok());

        match state.as_str() {
            "DIRTY" => {
                log(
                    log_tx,
                    format!("[merge] PR #{pr_num} ({head_branch}) is DIRTY — spawning AI resolver"),
                );
                toast(
                    log_tx,
                    "WARNING",
                    &format!("PR #{pr_num} has merge conflicts"),
                );
                if let Some(n) = issue_num {
                    let worktree = config.worktree_path(n);
                    let name = config.window_name(n);
                    if std::path::Path::new(&worktree).exists()
                        && !bottom_pane_active(config, &name).await
                    {
                        let _ =
                            std::fs::write(format!("/tmp/cwo-issue-{n}-conflict.txt"), "conflict");
                        let pane = {
                            let windows = list_windows(config).await;
                            match windows
                                .iter()
                                .find(|(_, w)| extract_issue_num(config, w) == Some(n))
                            {
                                Some((idx, _)) => capture_pane(config, *idx).await,
                                None => String::new(),
                            }
                        };
                        let fallback = config.branch_name(n);
                        let branch = worktree_branch(&worktree, &fallback).await;
                        let prompt = build_monitor_prompt(
                            config,
                            n,
                            &worktree,
                            &branch,
                            &pane,
                            &[*pr_num],
                            true,
                        );
                        send_print_pane(config, &name, &worktree, &prompt, log_tx).await;
                    }
                }
            }
            "BLOCKED" => {
                log(
                    log_tx,
                    format!("[merge] PR #{pr_num} is BLOCKED — checking review context"),
                );
                if let Some(n) = issue_num {
                    // Avoid re-reviewing same PR within 20 minutes
                    let review_file = format!("/tmp/cwo-review-{n}.txt");
                    let reviewed_recently = std::fs::metadata(&review_file)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(|t| t.elapsed().unwrap_or_default().as_secs() < 1200)
                        .unwrap_or(false);
                    if reviewed_recently {
                        log(
                            log_tx,
                            format!("[review] PR #{pr_num} — reviewed recently, skipping"),
                        );
                    } else {
                        let worktree = config.worktree_path(n);
                        let name = config.window_name(n);
                        if std::path::Path::new(&worktree).exists()
                            && !bottom_pane_active(config, &name).await
                        {
                            match github::get_pr_review_context(&config.repo, *pr_num).await {
                                Ok(review_ctx) => {
                                    let _ = std::fs::write(&review_file, &review_ctx);
                                    let pane = {
                                        let windows = list_windows(config).await;
                                        match windows
                                            .iter()
                                            .find(|(_, w)| extract_issue_num(config, w) == Some(n))
                                        {
                                            Some((idx, _)) => capture_pane(config, *idx).await,
                                            None => String::new(),
                                        }
                                    };
                                    let prompt = build_review_prompt(
                                        config,
                                        n,
                                        *pr_num,
                                        &worktree,
                                        &pane,
                                        &review_ctx,
                                    );
                                    send_print_pane(config, &name, &worktree, &prompt, log_tx)
                                        .await;
                                    toast(log_tx, "INFO", &format!("Sent review notes to #{n}"));
                                }
                                Err(e) => {
                                    log(
                                        log_tx,
                                        format!(
                                            "[review] PR #{pr_num}: failed to get review context: {e}"
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            "CLEAN" | "UNSTABLE" | "BEHIND" => {} // handled above
            other => {
                log(
                    log_tx,
                    format!("[merge] PR #{pr_num}: merge state = {other}"),
                );
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

pub async fn cleanup_orphaned_worktrees(config: &Config, log_tx: &mpsc::UnboundedSender<String>) {
    if config.repo_root.is_empty() {
        return;
    }
    let issues = crate::poller::scan_worktrees(config);
    let windows = list_windows(config).await;
    let active_issues: std::collections::HashSet<u64> = windows
        .iter()
        .filter_map(|(_, n)| extract_issue_num(config, n))
        .collect();

    for issue_num in issues {
        if active_issues.contains(&issue_num) {
            continue;
        }
        let state = github::get_issue_state(&config.repo, issue_num)
            .await
            .unwrap_or_default();
        if state == "CLOSED" {
            let worktree = config.worktree_path(issue_num);
            log(
                log_tx,
                format!("[cleanup] Orphaned worktree issue-{issue_num} closed — removing"),
            );
            toast(log_tx, "INFO", &format!("Cleaned up closed #{issue_num}"));
            let _ = tokio::process::Command::new("git")
                .args([
                    "-C",
                    &config.repo_root,
                    "worktree",
                    "remove",
                    "--force",
                    &worktree,
                ])
                .output()
                .await;
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    }
}

pub async fn promote_orphaned_worktrees(config: &Config, log_tx: &mpsc::UnboundedSender<String>) {
    let active = count_active_workers(config).await;
    if active >= config.max_concurrent {
        return;
    }
    let slots = config.max_concurrent - active;

    let windows = list_windows(config).await;
    let window_names: std::collections::HashSet<String> =
        windows.iter().map(|(_, n)| n.clone()).collect();

    let worktrees = crate::poller::scan_worktrees(config);
    let mut launched = 0;

    for issue_num in worktrees {
        if launched >= slots {
            break;
        }
        let name = config.window_name(issue_num);
        if window_names.contains(&name) {
            continue;
        }

        let _ = tokio::process::Command::new(&config.tmux)
            .args(["new-session", "-d", "-s", &config.session])
            .output()
            .await;

        let _ = tokio::process::Command::new(&config.tmux)
            .args(["new-window", "-t", &config.session, "-n", &name])
            .output()
            .await;

        let worktree = config.worktree_path(issue_num);
        let fallback = config.branch_name(issue_num);
        let branch = worktree_branch(&worktree, &fallback).await;
        let claude_prompt = format!(
            "Continue implementing GitHub issue #{issue_num} in this repo. Check what has already been done (git log, git status, existing code), finish the implementation, commit, push branch {branch}, and open a PR to main referencing #{issue_num}. Work autonomously."
        );
        let script_path = format!("/tmp/cwo-worker-{issue_num}.sh");
        let script = format!(
            "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude --dangerously-skip-permissions '{}'\n",
            worktree,
            claude_prompt.replace('\'', "'\\''")
        );
        if std::fs::write(&script_path, &script).is_ok() {
            let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
        }

        let target = format!("{}:{name}", config.session);
        send_keys(config, &target, &script_path).await;

        log(
            log_tx,
            format!("[monitor] Promoted orphaned worktree → launched #{issue_num}"),
        );
        toast(log_tx, "INFO", &format!("Launched #{issue_num}"));
        launched += 1;

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

pub async fn resume_after_backoff(
    config: &Config,
    backoff: &Arc<Mutex<BackoffState>>,
    log_tx: &mpsc::UnboundedSender<String>,
) {
    if backoff.lock().await.in_backoff() {
        return;
    }
    if !std::path::Path::new(BACKOFF_RESUMED_FILE).exists() {
        return;
    }
    let _ = std::fs::remove_file(BACKOFF_RESUMED_FILE);
    backoff.lock().await.clear();

    log(
        log_tx,
        "[builder] Backoff cleared — sending 'continue' to idle Claude windows",
    );
    toast(log_tx, "INFO", "Rate limit cleared");

    let windows = list_windows(config).await;
    for (idx, _) in &windows {
        let pane = capture_pane(config, *idx).await;
        if pane.contains("bypass permissions on") {
            let target = format!("{}:{}", config.session, idx);
            send_to_claude(config, &target, "continue with the task").await;
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    }
}

// ─── PR Review ──────────────────────────────────────────────────────────────

const REVIEW_DIR: &str = "/tmp/cwo-reviews";

/// Check whether a PR has already been reviewed by CWO.
fn pr_reviewed(pr_num: u64) -> bool {
    std::path::Path::new(REVIEW_DIR)
        .join(pr_num.to_string())
        .exists()
}

/// Mark a PR as having a review spawned.
fn mark_pr_reviewed(pr_num: u64) {
    let _ = std::fs::create_dir_all(REVIEW_DIR);
    let _ = std::fs::write(
        std::path::Path::new(REVIEW_DIR).join(pr_num.to_string()),
        now_unix().to_string(),
    );
}

/// Spawn a dedicated review worker for a PR.
/// Creates a `review-{issue_num}` tmux window running a Claude reviewer.
pub async fn spawn_pr_review(
    config: &Config,
    issue_num: u64,
    pr_num: u64,
    log_tx: &mpsc::UnboundedSender<String>,
    event_log: &EventLog,
) {
    if pr_reviewed(pr_num) {
        return;
    }

    let window_name = format!("review-{issue_num}");

    // Don't spawn if review window already exists
    let windows = list_windows(config).await;
    if windows.iter().any(|(_, n)| n == &window_name) {
        return;
    }

    mark_pr_reviewed(pr_num);

    log(
        log_tx,
        format!("[review] Spawning reviewer for PR #{pr_num} (issue #{issue_num})"),
    );
    toast(log_tx, "INFO", &format!("Reviewing PR #{pr_num}"));
    event_log.emit(
        "review_spawned",
        &[
            ("issue", serde_json::json!(issue_num)),
            ("pr", serde_json::json!(pr_num)),
        ],
    );

    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-session", "-d", "-s", &config.session])
        .output()
        .await;

    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-window", "-t", &config.session, "-n", &window_name])
        .output()
        .await;

    let worktree = config.worktree_path(issue_num);
    let review_prompt = format!(
        r#"You are an expert code reviewer. Review PR #{pr_num} for GitHub issue #{issue_num} in repo {repo}.

Steps:
1. Run: gh pr diff {pr_num} --repo {repo}
2. Run: gh pr view {pr_num} --repo {repo} --json title,body,files
3. Read the changed files in the worktree at {worktree} to understand context
4. Review the code thoroughly. Check for:
   - Correctness: Does the code do what the issue asks?
   - Bugs: Off-by-one errors, null/None handling, edge cases
   - Security: Injection, auth issues, unsafe operations
   - Style: Naming, structure, idiomatic patterns for the language
   - Tests: Are there tests? Do they cover the changes?
5. Post your review:
   - If the code is good: gh pr review {pr_num} --repo {repo} --approve --body "LGTM. <brief summary>"
   - If changes needed: gh pr review {pr_num} --repo {repo} --request-changes --body "<specific feedback>"

Be concise but specific. Reference line numbers and file names."#,
        repo = config.repo,
        worktree = worktree,
    );

    let script_path = format!("/tmp/cwo-review-{issue_num}.sh");
    let script = format!(
        "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude --dangerously-skip-permissions '{}'\n",
        worktree,
        review_prompt.replace('\'', "'\\''")
    );
    if let Err(e) = std::fs::write(&script_path, &script) {
        log(log_tx, format!("[review] Failed to write script: {e}"));
        return;
    }
    let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));

    let target = format!("{}:{window_name}", config.session);
    send_keys(config, &target, &script_path).await;

    log(
        log_tx,
        format!("[review] Reviewer launched in {window_name} for PR #{pr_num}"),
    );
}

// ─── Worker Health & Self-Healing ────────────────────────────────────────────

fn relaunch_count(issue_num: u64) -> u32 {
    let path = format!("/tmp/cwo-relaunch-{issue_num}.txt");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn increment_relaunch(issue_num: u64) -> u32 {
    let count = relaunch_count(issue_num) + 1;
    let _ = std::fs::write(
        format!("/tmp/cwo-relaunch-{issue_num}.txt"),
        count.to_string(),
    );
    count
}

fn reset_relaunch(issue_num: u64) {
    let _ = std::fs::remove_file(format!("/tmp/cwo-relaunch-{issue_num}.txt"));
}

/// Check all workers for stale/shell state and auto-relaunch if configured.
/// Also kills stuck probe panes (no output for 120s).
pub async fn check_worker_health(
    config: &Config,
    log_tx: &mpsc::UnboundedSender<String>,
    event_log: &EventLog,
) {
    let rt = runtime_config(config);
    if !rt.auto_relaunch {
        return;
    }

    let windows = list_windows(config).await;

    for (idx, name) in &windows {
        let Some(issue_num) = extract_issue_num(config, name) else {
            continue;
        };

        let pane = capture_pane(config, *idx).await;
        let state = classify_pane(config, &pane);

        // Only act on shell or workers where the interactive Claude exited
        let needs_relaunch =
            state == "shell" && (pane.contains("claude") || pane.contains(&config.branch_prefix));

        if !needs_relaunch {
            // Reset relaunch counter when worker is healthy
            if state == "active" || state == "claude_repl" {
                reset_relaunch(issue_num);
            }
            continue;
        }

        let worktree = config.worktree_path(issue_num);
        if !std::path::Path::new(&worktree).exists() {
            continue;
        }

        if bottom_pane_active(config, name).await {
            continue;
        }

        let count = relaunch_count(issue_num);
        if count >= rt.max_relaunch_attempts {
            log(
                log_tx,
                format!(
                    "[health] #{issue_num}: reached max relaunch attempts ({}) — marking failed",
                    rt.max_relaunch_attempts
                ),
            );
            toast(
                log_tx,
                "ERROR",
                &format!("#{issue_num} failed after {count} relaunches"),
            );
            event_log.emit(
                "worker_failed",
                &[
                    ("issue", serde_json::json!(issue_num)),
                    (
                        "reason",
                        serde_json::json!(format!(
                            "{} relaunch failures",
                            rt.max_relaunch_attempts
                        )),
                    ),
                ],
            );
            // Write a marker so poller can show "failed" status
            let _ = std::fs::write(format!("/tmp/cwo-worker-{issue_num}-failed.txt"), "failed");
            continue;
        }

        let active = count_active_workers(config).await;
        if active >= config.max_concurrent {
            log(
                log_tx,
                format!("[health] #{issue_num}: at capacity, skipping relaunch"),
            );
            continue;
        }

        let new_count = increment_relaunch(issue_num);
        log(
            log_tx,
            format!(
                "[health] #{issue_num}: relaunching (attempt {new_count}/{})",
                rt.max_relaunch_attempts
            ),
        );
        toast(
            log_tx,
            "WARNING",
            &format!("Relaunching #{issue_num} ({new_count})"),
        );

        // Build a context-aware relaunch prompt
        let fallback = config.branch_name(issue_num);
        let branch = worktree_branch(&worktree, &fallback).await;

        // Get git context for the relaunch prompt
        let git_log = tokio::process::Command::new("git")
            .args(["-C", &worktree, "log", "--oneline", "-10"])
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let git_status = tokio::process::Command::new("git")
            .args(["-C", &worktree, "status", "--short"])
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        let claude_prompt = format!(
            "Continue implementing GitHub issue #{issue_num}. You are being relaunched after a crash.\n\n\
            Git log:\n{git_log}\n\
            Git status:\n{git_status}\n\n\
            Check what has been done, finish the implementation, commit, push branch {branch}, and open a PR to main referencing #{issue_num}. Work autonomously."
        );

        let script_path = format!("/tmp/cwo-worker-{issue_num}.sh");
        let script = format!(
            "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude --dangerously-skip-permissions '{}'\n",
            worktree,
            claude_prompt.replace('\'', r"'\''")
        );
        if std::fs::write(&script_path, &script).is_ok() {
            let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
            let target = format!("{}:{}", config.session, idx);
            send_keys(config, &target, &script_path).await;

            event_log.emit(
                "worker_relaunched",
                &[
                    ("issue", serde_json::json!(issue_num)),
                    ("attempt", serde_json::json!(new_count)),
                ],
            );
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }

    // Kill stuck probe panes (no output for 120s)
    for (_, name) in &windows {
        if let Some(probe_idx) = probe_pane_index(config, name).await {
            let target = format!("{}:{}.{probe_idx}", config.session, name);
            let current_cmd = tokio::process::Command::new(&config.tmux)
                .args([
                    "display-message",
                    "-t",
                    &target,
                    "-p",
                    "#{pane_current_command}",
                ])
                .output()
                .await
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();

            let probe_running =
                !matches!(current_cmd.as_str(), "zsh" | "bash" | "sh" | "fish" | "");
            if !probe_running {
                continue;
            }

            // Check probe pane start time via pane_start_command
            let start_time = tokio::process::Command::new(&config.tmux)
                .args(["display-message", "-t", &target, "-p", "#{pane_start_time}"])
                .output()
                .await
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .parse::<u64>()
                        .unwrap_or(0)
                })
                .unwrap_or(0);

            let elapsed = now_unix().saturating_sub(start_time);
            if elapsed > 120 {
                log(
                    log_tx,
                    format!("[health] Killing stuck probe pane in {name} (running {elapsed}s)"),
                );
                let _ = tokio::process::Command::new(&config.tmux)
                    .args(["kill-pane", "-t", &target])
                    .output()
                    .await;
            }
        }
    }
}

/// Check if a worker has been marked as failed.
pub fn is_worker_failed(issue_num: u64) -> bool {
    std::path::Path::new(&format!("/tmp/cwo-worker-{issue_num}-failed.txt")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_print_json_finds_last_json_line() {
        let output = "some preamble text\nmore output\n{\"action\":\"wait\",\"status\":\"done\"}\n";
        let v = parse_print_json(output).expect("should find JSON");
        assert_eq!(v["action"], "wait");
        assert_eq!(v["status"], "done");
    }

    #[test]
    fn parse_print_json_returns_none_for_no_json() {
        let output = "just plain text\nno json here";
        assert!(parse_print_json(output).is_none());
    }

    #[test]
    fn parse_print_json_picks_last_when_multiple() {
        let output = "{\"action\":\"first\"}\nsome text\n{\"action\":\"second\"}\n";
        let v = parse_print_json(output).expect("should find JSON");
        assert_eq!(v["action"], "second");
    }

    #[test]
    fn has_conflict_marker_false_when_no_file() {
        // Use a number unlikely to collide with real files
        assert!(!has_conflict_marker(999999999));
    }

    #[test]
    fn backoff_state_not_in_backoff_when_zero() {
        let b = BackoffState {
            until_unix: 0,
            needs_resume: false,
        };
        assert!(!b.in_backoff());
    }

    #[test]
    fn backoff_remaining_negative_when_expired() {
        let b = BackoffState {
            until_unix: 1,
            needs_resume: false,
        };
        assert!(b.remaining_secs() < 0);
    }
}
