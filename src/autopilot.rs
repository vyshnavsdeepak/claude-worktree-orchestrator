use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::config::{Config, RuntimeConfig};
use crate::github;
use crate::messages::{self, AppCommand, LogMessage, MergeQueueUpdate, ToastLevel, UpcomingUpdate};
use crate::poller::WorkerState;
use crate::state::StateDir;

// ─── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AutopilotState {
    pub current_batch: Vec<BatchItem>,
    pub completed: HashMap<u64, String>, // issue_num -> PR# or "skipped"
    pub skipped: HashSet<u64>,
    pub deviation_issues: Vec<u64>,
    pub last_fetch_ts: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchItem {
    pub issue_num: u64,
    pub title: String,
    pub priority: f32,
    pub file_areas: Vec<String>,
    pub status: BatchItemStatus,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BatchItemStatus {
    Queued,
    Launched,
    Done,
    Skipped,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct IssueAnalysis {
    pub issue_num: u64,
    pub title: String,
    pub priority: f32,
    pub actionable: bool,
    pub file_areas: Vec<String>,
    pub reason: String,
    pub estimated_complexity: String,
}

// ─── State persistence ───────────────────────────────────────────────────────

fn load_state(state_dir: &StateDir) -> AutopilotState {
    crate::util::load_json(&state_dir.autopilot_state()).unwrap_or_default()
}

fn save_state(state_dir: &StateDir, state: &AutopilotState) {
    crate::util::save_json(&state_dir.autopilot_state(), state);
}

// ─── Main loop ───────────────────────────────────────────────────────────────

pub async fn run(
    config: Arc<Config>,
    worker_rx: watch::Receiver<Vec<WorkerState>>,
    log_tx: mpsc::UnboundedSender<LogMessage>,
    prompt_tx: mpsc::UnboundedSender<AppCommand>,
    state_dir: Arc<StateDir>,
    mut toggle_rx: watch::Receiver<bool>,
) {
    let mut state = load_state(&state_dir);

    // Clean up "no-window" entries from completed — these are retryable, not truly completed
    state
        .completed
        .retain(|_, v| v != "no-window" && v != "failed");

    loop {
        // Wait until enabled
        loop {
            if *toggle_rx.borrow() {
                break;
            }
            if toggle_rx.changed().await.is_err() {
                return; // channel closed
            }
        }

        messages::log(&log_tx, "[autopilot] Starting batch cycle");

        // Fetch and publish repo issue counts
        if let Ok((open, closed)) = github::issue_counts(&config.repo).await {
            let _ = log_tx.send(LogMessage::RepoIssueCounts { open, closed });
        }

        // Load runtime config for current settings
        let rt = RuntimeConfig::load(&state_dir.runtime_config())
            .unwrap_or_else(|| RuntimeConfig::from_config(&config));

        // Phase 0: Merge any open PRs from completed issues
        send_status(&log_tx, "checking for mergeable PRs...");
        merge_completed_prs(&config, &log_tx, &mut state).await;
        save_state(&state_dir, &state);

        send_status(&log_tx, "fetching issues...");

        // Phase 1: Fetch open issues
        let issues = match fetch_issues(&config, &rt).await {
            Ok(issues) => issues,
            Err(e) => {
                messages::log(&log_tx, format!("[autopilot] Fetch error: {e}"));
                send_status(&log_tx, "fetch error, retrying...");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };

        state.last_fetch_ts = now_unix();

        // Filter out completed/skipped/active
        let active_issues = get_active_issue_nums(&worker_rx);
        let candidates: Vec<(u64, String)> = issues
            .into_iter()
            .filter(|(num, _, _)| {
                !state.completed.contains_key(num)
                    && !state.skipped.contains(num)
                    && !active_issues.contains(num)
            })
            .map(|(num, title, _)| (num, title))
            .collect();

        if candidates.is_empty() {
            messages::log(&log_tx, "[autopilot] No candidate issues found");
            send_status(&log_tx, "no issues, waiting...");
            save_state(&state_dir, &state);
            wait_or_toggle(&mut toggle_rx, rt.autopilot_batch_delay_secs).await;
            continue;
        }

        messages::log(
            &log_tx,
            format!("[autopilot] {} candidate issues found", candidates.len()),
        );

        // Show candidate issues in TUI immediately (before analysis)
        let _ = log_tx.send(LogMessage::AutopilotUpcoming(UpcomingUpdate::Clear));
        for (num, title) in &candidates {
            let _ = log_tx.send(LogMessage::AutopilotUpcoming(UpcomingUpdate::Set {
                issue_num: *num,
                title: title.clone(),
                priority: String::new(),
                complexity: String::new(),
                reason: String::new(),
            }));
        }

        // Phase 2: Analyze issues with Claude
        let batch_size = rt.autopilot_batch_size;
        let to_analyze: Vec<(u64, String)> = candidates.into_iter().take(batch_size * 2).collect();

        send_status(
            &log_tx,
            &format!("analyzing {} issues...", to_analyze.len()),
        );

        let analyses = match analyze_issues(&config.repo, &to_analyze).await {
            Ok(a) => a,
            Err(e) => {
                messages::log(&log_tx, format!("[autopilot] Analysis error: {e}"));
                send_status(&log_tx, "analysis error, retrying...");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };

        // Log analysis results — show Claude's thought process
        messages::log(
            &log_tx,
            format!("[autopilot] Analysis of {} issues:", analyses.len()),
        );
        for a in &analyses {
            let status = if a.actionable { "✓" } else { "✗" };
            let areas = if a.file_areas.is_empty() {
                String::new()
            } else {
                format!(" [{}]", a.file_areas.join(", "))
            };
            messages::log(
                &log_tx,
                format!(
                    "[autopilot]   {status} #{} p={:.1} {} {} — {}{}",
                    a.issue_num, a.priority, a.estimated_complexity, a.title, a.reason, areas
                ),
            );
        }

        // Mark non-actionable as skipped
        for a in &analyses {
            if !a.actionable {
                state.skipped.insert(a.issue_num);
            }
        }

        // Phase 2.5: Close idle done-worker tmux windows to free capacity
        {
            let to_close: Vec<usize> = {
                let workers = worker_rx.borrow();
                workers
                    .iter()
                    .filter(|w| {
                        w.pr.is_some()
                            && matches!(w.status.as_str(), "done" | "shell" | "posted")
                            && w.window_index > 0
                    })
                    .map(|w| w.window_index)
                    .collect()
            };
            if !to_close.is_empty() {
                for idx in &to_close {
                    let target = format!("{}:{}", config.session, idx);
                    let _ = tokio::process::Command::new(&config.tmux)
                        .args(["kill-window", "-t", &target])
                        .output()
                        .await;
                }
                messages::log(
                    &log_tx,
                    format!(
                        "[autopilot] Closed {} idle done-worker windows to free capacity",
                        to_close.len()
                    ),
                );
                // Give tmux a moment to settle
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }

        // Phase 3: Select batch — conflict-minimizing
        // Use the same capacity check as the builder (count tmux windows with active Claude)
        let available_capacity = {
            let active = crate::monitor::count_active_workers(&config).await;
            let max = RuntimeConfig::effective_max_concurrent(&config, &state_dir.runtime_config());
            messages::log(
                &log_tx,
                format!("[autopilot] Capacity: {active} active / {max} max"),
            );
            max.saturating_sub(active)
        };

        if available_capacity == 0 {
            messages::log(
                &log_tx,
                "[autopilot] No capacity, waiting for workers to finish",
            );
            send_status(&log_tx, "at capacity, waiting...");
            save_state(&state_dir, &state);
            wait_or_toggle(&mut toggle_rx, 30).await;
            continue;
        }

        let actionable: Vec<&IssueAnalysis> = analyses.iter().filter(|a| a.actionable).collect();

        let batch = select_batch(&actionable, available_capacity);

        messages::log(
            &log_tx,
            format!(
                "[autopilot] Selected batch of {} issues (capacity: {})",
                batch.len(),
                available_capacity
            ),
        );

        // Publish upcoming issues (actionable but not in this batch) to TUI
        let batch_nums: std::collections::HashSet<u64> =
            batch.iter().map(|b| b.issue_num).collect();
        let _ = log_tx.send(LogMessage::AutopilotUpcoming(UpcomingUpdate::Clear));
        for a in &actionable {
            if !batch_nums.contains(&a.issue_num) {
                // Format: num\ttitle\tpriority\tcomplexity\treason
                let _ = log_tx.send(LogMessage::AutopilotUpcoming(UpcomingUpdate::Set {
                    issue_num: a.issue_num,
                    title: a.title.clone(),
                    priority: format!("{:.1}", a.priority),
                    complexity: a.estimated_complexity.clone(),
                    reason: a.reason.clone(),
                }));
            }
        }

        // Phase 4: Launch workers
        state.current_batch.clear();
        for item in &batch {
            state.current_batch.push(BatchItem {
                issue_num: item.issue_num,
                title: item.title.clone(),
                priority: item.priority,
                file_areas: item.file_areas.clone(),
                status: BatchItemStatus::Queued,
            });
        }
        save_state(&state_dir, &state);

        for item in &mut state.current_batch {
            if !*toggle_rx.borrow() {
                messages::log(&log_tx, "[autopilot] Toggled off, pausing launches");
                break;
            }

            if prompt_tx
                .send(AppCommand::NewJob {
                    issue_num: item.issue_num,
                    branch_override: None,
                    base_branch: None,
                    plan_mode: false,
                })
                .is_ok()
            {
                item.status = BatchItemStatus::Launched;
                messages::log(
                    &log_tx,
                    format!("[autopilot] Launched #{} — {}", item.issue_num, item.title),
                );
                send_status(&log_tx, &format!("launched #{}", item.issue_num));
            } else {
                messages::log(
                    &log_tx,
                    format!(
                        "[autopilot] Failed to queue #{} — channel closed",
                        item.issue_num
                    ),
                );
                break;
            }

            // Stagger launches
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        save_state(&state_dir, &state);

        // Phase 5: Monitor — wait for batch workers to reach terminal states
        send_status(
            &log_tx,
            &format!(
                "monitoring {} workers...",
                state
                    .current_batch
                    .iter()
                    .filter(|b| b.status == BatchItemStatus::Launched)
                    .count()
            ),
        );

        let launched_nums: HashSet<u64> = state
            .current_batch
            .iter()
            .filter(|b| b.status == BatchItemStatus::Launched)
            .map(|b| b.issue_num)
            .collect();

        if !launched_nums.is_empty() {
            monitor_batch(
                &config,
                &worker_rx,
                &log_tx,
                &state_dir,
                &mut state,
                &launched_nums,
                &mut toggle_rx,
            )
            .await;
        }

        save_state(&state_dir, &state);

        // Merge drain loop: keep merging PRs until none are left or progress stalls.
        // After conflict resolution workers finish, their PRs become CLEAN and can be merged.
        let mut merge_rounds = 0u32;
        loop {
            merge_rounds += 1;
            if merge_rounds > 20 {
                messages::log(
                    &log_tx,
                    "[autopilot] Merge drain: max rounds reached, moving on",
                );
                break;
            }
            if !*toggle_rx.borrow() {
                break;
            }

            send_status(&log_tx, &format!("merge drain round {merge_rounds}..."));
            let before = state.completed.len();
            merge_completed_prs(&config, &log_tx, &mut state).await;
            pull_latest_main(&config, &log_tx).await;
            save_state(&state_dir, &state);

            // Check if any open PRs remain for our completed issues
            let remaining = match github::list_open_prs(&config.repo).await {
                Ok(prs) => prs
                    .iter()
                    .filter(|(_, branch)| {
                        state.completed.keys().any(|issue_num| {
                            let prefix = config.branch_name(*issue_num);
                            branch == &prefix || branch.starts_with(&format!("{prefix}-"))
                        })
                    })
                    .count(),
                Err(_) => 0,
            };

            if remaining == 0 {
                messages::log(&log_tx, "[autopilot] All completed PRs merged or closed");
                break;
            }

            messages::log(
                &log_tx,
                format!(
                "[autopilot] {remaining} PRs still open, waiting for conflict resolution workers..."
            ),
            );

            // Wait for conflict resolution workers to do their thing
            // Check every 30s if any PRs became mergeable
            let progress_made = state.completed.len() > before;
            let wait = if progress_made { 10 } else { 30 };
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        }

        messages::log(
            &log_tx,
            "[autopilot] Batch complete, waiting before next cycle",
        );
        send_status(&log_tx, "batch done, cooling down...");

        let delay = rt.autopilot_batch_delay_secs;
        wait_or_toggle(&mut toggle_rx, delay).await;
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn send_status(log_tx: &mpsc::UnboundedSender<LogMessage>, msg: &str) {
    let _ = log_tx.send(LogMessage::AutopilotStatus(msg.to_string()));
}

fn get_active_issue_nums(worker_rx: &watch::Receiver<Vec<WorkerState>>) -> HashSet<u64> {
    worker_rx
        .borrow()
        .iter()
        .filter_map(|w| {
            w.window_name
                .strip_prefix("issue-")
                .and_then(|s| s.parse::<u64>().ok())
        })
        .collect()
}

async fn fetch_issues(
    config: &Config,
    rt: &RuntimeConfig,
) -> anyhow::Result<Vec<(u64, String, String)>> {
    github::list_open_issues_with_labels(
        &config.repo,
        &rt.autopilot_labels,
        &rt.autopilot_exclude_labels,
    )
    .await
}

async fn analyze_issues(
    repo: &str,
    issues: &[(u64, String)],
) -> anyhow::Result<Vec<IssueAnalysis>> {
    if issues.is_empty() {
        return Ok(Vec::new());
    }

    // Build a summary of issues for Claude to analyze
    let issue_list: String = issues
        .iter()
        .map(|(num, title)| format!("- #{num}: {title}"))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        r#"You are analyzing GitHub issues for an autonomous coding system. For each issue, determine if it's actionable (implementable by an AI coder) and predict which file/directory areas it would touch.

Issues:
{issue_list}

Respond with a JSON array. Each element must have these fields:
- issue_num (number)
- title (string)
- priority (number 0.0-1.0, higher = more important/urgent)
- actionable (boolean — false for questions, discussions, meta-issues)
- file_areas (array of strings — predicted file/directory paths like "src/", "tests/", "src/config.rs")
- reason (string — brief explanation of priority/actionability decision)
- estimated_complexity (string — "small", "medium", or "large")

Respond ONLY with the JSON array, no markdown fences or explanation."#
    );

    // Also fetch issue bodies for better analysis
    let mut bodies = Vec::new();
    for (num, _title) in issues.iter().take(20) {
        match github::get_issue(repo, *num).await {
            Ok((_t, body)) => bodies.push((*num, body)),
            Err(_) => bodies.push((*num, String::new())),
        }
    }

    let body_context: String = bodies
        .iter()
        .filter(|(_, b)| !b.is_empty())
        .map(|(num, body)| {
            let preview: String = body.chars().take(500).collect();
            format!("### Issue #{num}\n{preview}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let full_prompt = if body_context.is_empty() {
        prompt
    } else {
        format!("{prompt}\n\nAdditional context (issue bodies):\n{body_context}")
    };

    let response = github::invoke_claude(&full_prompt).await?;

    // Parse JSON from response — find the array
    let json_str = extract_json_array(&response).unwrap_or(&response);

    let analyses: Vec<IssueAnalysis> = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse analysis JSON: {e}\nResponse: {response}"))?;

    Ok(analyses)
}

fn extract_json_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let mut depth = 0;
    for (i, ch) in text[start..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Select the top-priority subset of candidates up to `capacity`.
fn select_batch<'a>(candidates: &[&'a IssueAnalysis], capacity: usize) -> Vec<&'a IssueAnalysis> {
    let mut sorted: Vec<&IssueAnalysis> = candidates.to_vec();
    sorted.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sorted.truncate(capacity);
    sorted
}

async fn monitor_batch(
    config: &Config,
    worker_rx: &watch::Receiver<Vec<WorkerState>>,
    log_tx: &mpsc::UnboundedSender<LogMessage>,
    state_dir: &StateDir,
    state: &mut AutopilotState,
    launched_nums: &HashSet<u64>,
    toggle_rx: &mut watch::Receiver<bool>,
) {
    let timeout = std::time::Duration::from_secs(3600); // 1 hour max per batch
    let start = std::time::Instant::now();
    let mut resumed_issues: HashSet<u64> = HashSet::new();

    loop {
        if !*toggle_rx.borrow() {
            messages::log(log_tx, "[autopilot] Toggled off during monitoring");
            break;
        }

        if start.elapsed() > timeout {
            messages::log(log_tx, "[autopilot] Batch monitoring timeout (1h)");
            break;
        }

        let workers = worker_rx.borrow().clone();
        let mut all_done = true;
        let mut newly_completed: Vec<BatchItem> = Vec::new();
        let elapsed_secs = start.elapsed().as_secs();

        for item in state.current_batch.iter_mut() {
            if item.status != BatchItemStatus::Launched {
                continue;
            }
            if !launched_nums.contains(&item.issue_num) {
                continue;
            }

            let window_name = format!("issue-{}", item.issue_num);
            if let Some(w) = workers.iter().find(|w| w.window_name == window_name) {
                match w.status.as_str() {
                    "done" | "posted" => {
                        item.status = BatchItemStatus::Done;
                        let pr = w.pr.clone().unwrap_or_default();
                        state.completed.insert(item.issue_num, pr.clone());
                        messages::log(
                            log_tx,
                            format!("[autopilot] #{} completed ({})", item.issue_num, pr),
                        );
                        newly_completed.push(item.clone());
                    }
                    "shell" | "failed" => {
                        item.status = BatchItemStatus::Skipped;
                        state.completed.insert(item.issue_num, "failed".to_string());
                        messages::log(
                            log_tx,
                            format!("[autopilot] #{} failed/crashed", item.issue_num),
                        );
                    }
                    "idle" | "stale" => {
                        // Claude REPL is idle/stale — worker stopped without creating a PR.
                        // Send a continuation prompt (once per issue per batch).
                        if !resumed_issues.contains(&item.issue_num) {
                            resumed_issues.insert(item.issue_num);
                            let target = format!("{}:{}", config.session, w.window_name);
                            let resume = format!(
                                "Continue implementing issue #{}. If you hit errors, fix them. Push the branch and open a PR when done.",
                                item.issue_num
                            );
                            let _ = tokio::process::Command::new(&config.tmux)
                                .args(["send-keys", "-t", &target, "-l", &resume])
                                .output()
                                .await;
                            let _ = tokio::process::Command::new(&config.tmux)
                                .args(["send-keys", "-t", &target, "Enter"])
                                .output()
                                .await;
                            messages::log(
                                log_tx,
                                format!(
                                    "[autopilot] #{} idle — sent continue prompt",
                                    item.issue_num
                                ),
                            );
                        }
                        all_done = false;
                    }
                    _ => {
                        all_done = false;
                    }
                }
            } else if elapsed_secs > 120 {
                // Worker never got a tmux window after 2 minutes — likely queued but never launched
                // Don't mark completed — leave retryable for next batch cycle
                item.status = BatchItemStatus::Skipped;
                messages::log(
                    log_tx,
                    format!(
                        "[autopilot] #{} never launched (no tmux window after {}s), will retry",
                        item.issue_num, elapsed_secs
                    ),
                );
            } else {
                // Worker not found yet — might still be launching
                all_done = false;
            }
        }

        // Scope deviation checks for newly completed items
        for item in &newly_completed {
            check_scope_deviation(config, log_tx, state, item).await;
        }

        save_state(state_dir, state);

        if all_done {
            break;
        }

        let remaining = state
            .current_batch
            .iter()
            .filter(|b| b.status == BatchItemStatus::Launched)
            .count();
        send_status(log_tx, &format!("monitoring {remaining} workers..."));

        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

async fn check_scope_deviation(
    config: &Config,
    log_tx: &mpsc::UnboundedSender<LogMessage>,
    state: &mut AutopilotState,
    item: &BatchItem,
) {
    let branch = config.branch_name(item.issue_num);
    let default_branch = config.default_branch();

    // Get diff stat
    let diff_out = tokio::process::Command::new("git")
        .args([
            "-C",
            &config.repo_root,
            "diff",
            "--stat",
            &format!("{default_branch}...{branch}"),
        ])
        .output()
        .await;

    let diff_stat = match diff_out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return,
    };

    if diff_stat.trim().is_empty() {
        return;
    }

    let prompt = format!(
        r#"A worker was assigned this GitHub issue:
Title: {}
Issue #: {}

The git diff --stat of their branch vs main is:
{}

Did the implementation significantly deviate from the original issue scope?
If yes, describe what extra work was done that should be tracked separately.

Respond in JSON: {{"deviated": true/false, "new_issue_title": "...", "new_issue_body": "..."}}"#,
        item.title, item.issue_num, diff_stat
    );

    let response = match github::invoke_claude(&prompt).await {
        Ok(r) => r,
        Err(_) => return,
    };

    // Try to parse deviation result
    if let Some(json_str) = extract_json_object(&response) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
            if val
                .get("deviated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let title = val
                    .get("new_issue_title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Follow-up from autopilot scope deviation");
                let body = val
                    .get("new_issue_body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Autopilot detected scope deviation.");

                match github::create_issue(&config.repo, title, body).await {
                    Ok(new_num) => {
                        state.deviation_issues.push(new_num);
                        messages::log(
                            log_tx,
                            format!(
                            "[autopilot] Scope deviation detected for #{}, created follow-up #{}",
                            item.issue_num, new_num
                        ),
                        );
                        messages::toast(
                            log_tx,
                            ToastLevel::Warning,
                            format!("Deviation: #{} → new #{}", item.issue_num, new_num),
                        );
                    }
                    Err(e) => {
                        messages::log(
                            log_tx,
                            format!("[autopilot] Failed to create deviation issue: {e}"),
                        );
                    }
                }
            }
        }
    }
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0;
    for (i, ch) in text[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Smart merge: find all open PRs for completed issues, order them to minimize
/// conflicts (smallest diff first, non-overlapping files first), merge sequentially
/// pulling main between each merge.
async fn merge_completed_prs(
    config: &Config,
    log_tx: &mpsc::UnboundedSender<LogMessage>,
    state: &mut AutopilotState,
) {
    messages::log(
        log_tx,
        format!(
            "[autopilot] merge check: {} completed issues",
            state.completed.len()
        ),
    );

    // Find open PRs matching completed issues by branch name
    let open_prs = match github::list_open_prs_with_titles(&config.repo).await {
        Ok(prs) => prs,
        Err(e) => {
            messages::log(log_tx, format!("[autopilot] Failed to list PRs: {e}"));
            return;
        }
    };

    if open_prs.is_empty() {
        return;
    }

    // Collect PRs that belong to our completed issues (match by branch prefix)
    // (issue_num, pr_num, branch, title)
    let mut mergeable: Vec<(u64, u64, String, String)> = Vec::new();
    for issue_num in state.completed.keys() {
        let prefix = config.branch_name(*issue_num);
        let prefix_dash = format!("{prefix}-");
        if let Some((pr_num, branch, title)) = open_prs
            .iter()
            .find(|(_, b, _)| b == &prefix || b.starts_with(&prefix_dash))
        {
            mergeable.push((*issue_num, *pr_num, branch.clone(), title.clone()));
        }
    }

    if mergeable.is_empty() {
        messages::log(
            log_tx,
            "[autopilot] No mergeable PRs found for completed issues",
        );
        return;
    }

    messages::log(
        log_tx,
        format!(
            "[autopilot] Found {} PRs to merge, determining order...",
            mergeable.len()
        ),
    );

    // Get diff stats for each PR to determine merge order
    // (issue, pr, branch, title, lines_changed, files)
    let mut pr_stats: Vec<(u64, u64, String, String, usize, Vec<String>)> = Vec::new();
    let default_branch = config.default_branch();
    for (issue_num, pr_num, branch, title) in &mergeable {
        let diff_out = tokio::process::Command::new("git")
            .args([
                "-C",
                &config.repo_root,
                "diff",
                "--stat",
                &format!("{default_branch}...{branch}"),
            ])
            .output()
            .await;

        let (lines, files) = match diff_out {
            Ok(o) if o.status.success() => {
                let stat = String::from_utf8_lossy(&o.stdout);
                let file_list: Vec<String> = stat
                    .lines()
                    .filter(|l| l.contains('|'))
                    .filter_map(|l| l.split('|').next().map(|f| f.trim().to_string()))
                    .collect();
                // Last line has total: "N files changed, M insertions, K deletions"
                let total: usize = stat
                    .lines()
                    .last()
                    .map(|l| {
                        l.split(',')
                            .filter_map(|p| {
                                p.split_whitespace()
                                    .next()
                                    .and_then(|n| n.parse::<usize>().ok())
                            })
                            .sum()
                    })
                    .unwrap_or(0);
                (total, file_list)
            }
            _ => (0, Vec::new()),
        };

        pr_stats.push((
            *issue_num,
            *pr_num,
            branch.clone(),
            title.clone(),
            lines,
            files,
        ));
    }

    // Sort: smallest diffs first (merge easy ones first to reduce conflict chance),
    // break ties by putting non-overlapping PRs earlier
    pr_stats.sort_by(|a, b| a.4.cmp(&b.4));

    // Greedy reorder: pick PRs that don't overlap files with already-selected ones first
    let mut ordered: Vec<(u64, String, String)> = Vec::new(); // (pr, branch, title)
    let mut merged_files: HashSet<String> = HashSet::new();
    let mut remaining = pr_stats;

    // First pass: non-overlapping, smallest first
    remaining.retain(|(_issue, pr, branch, title, _lines, files)| {
        let overlaps = files.iter().any(|f| merged_files.contains(f));
        if !overlaps {
            for f in files {
                merged_files.insert(f.clone());
            }
            ordered.push((*pr, branch.clone(), title.clone()));
            false
        } else {
            true
        }
    });

    // Second pass: remaining (overlapping) PRs, still smallest first
    for (_issue, pr, branch, title, _lines, _files) in remaining {
        ordered.push((pr, branch, title));
    }

    messages::log(
        log_tx,
        format!(
            "[autopilot] Merge order: {}",
            ordered
                .iter()
                .map(|(pr, branch, _)| format!("PR#{pr}({branch})"))
                .collect::<Vec<_>>()
                .join(" → ")
        ),
    );

    // Populate merge queue in TUI
    let _ = log_tx.send(LogMessage::AutopilotMergeQueue(MergeQueueUpdate::Clear));
    for (pr_num, _branch, title) in &ordered {
        let _ = log_tx.send(LogMessage::AutopilotMergeQueue(MergeQueueUpdate::Set {
            pr_num: *pr_num,
            title: title.clone(),
            status: "queued".to_string(),
        }));
    }

    // Merge sequentially, pulling main between each
    let mut merged_count = 0u32;
    for (pr_num, branch, title) in ordered {
        let _ = log_tx.send(LogMessage::AutopilotMergeQueue(MergeQueueUpdate::Set {
            pr_num,
            title: title.clone(),
            status: "checking".to_string(),
        }));
        messages::log(log_tx, format!("[autopilot] Checking PR #{pr_num}..."));

        let update_queue = |status: &str| {
            let _ = log_tx.send(LogMessage::AutopilotMergeQueue(MergeQueueUpdate::Set {
                pr_num,
                title: title.clone(),
                status: status.to_string(),
            }));
        };

        match github::get_pr_info(&config.repo, pr_num, &branch).await {
            Ok(info) => match info.merge_state.as_str() {
                "CLEAN" => {
                    update_queue("merging");
                    messages::log(log_tx, format!("[autopilot] Merging PR #{pr_num}..."));
                    match github::merge_pr(&config.repo, pr_num).await {
                        Ok(()) => {
                            merged_count += 1;
                            messages::log(log_tx, format!("[autopilot] ✓ Merged PR #{pr_num}"));
                            messages::toast(
                                log_tx,
                                ToastLevel::Success,
                                format!("Merged PR #{pr_num}"),
                            );
                            let _ = log_tx.send(LogMessage::AutopilotMerged {
                                pr_num,
                                title: title.clone(),
                            });
                            pull_latest_main(config, log_tx).await;
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        Err(e) => {
                            update_queue("merge failed");
                            messages::log(
                                log_tx,
                                format!("[autopilot] Merge failed PR #{pr_num}: {e}"),
                            );
                        }
                    }
                }
                "BEHIND" => {
                    update_queue("behind → updating");
                    messages::log(
                        log_tx,
                        format!("[autopilot] PR #{pr_num} behind main, updating branch..."),
                    );
                    let _ = tokio::process::Command::new("gh")
                        .args([
                            "pr",
                            "update-branch",
                            &pr_num.to_string(),
                            "--repo",
                            &config.repo,
                            "--rebase",
                        ])
                        .output()
                        .await;
                }
                "UNKNOWN" => {
                    update_queue("unknown → trying");
                    messages::log(
                        log_tx,
                        format!("[autopilot] PR #{pr_num} state UNKNOWN, attempting merge..."),
                    );
                    match github::merge_pr(&config.repo, pr_num).await {
                        Ok(()) => {
                            merged_count += 1;
                            messages::log(log_tx, format!("[autopilot] ✓ Merged PR #{pr_num}"));
                            messages::toast(
                                log_tx,
                                ToastLevel::Success,
                                format!("Merged PR #{pr_num}"),
                            );
                            let _ = log_tx.send(LogMessage::AutopilotMerged {
                                pr_num,
                                title: title.clone(),
                            });
                            pull_latest_main(config, log_tx).await;
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        Err(e) => {
                            update_queue("not ready");
                            messages::log(
                                log_tx,
                                format!("[autopilot] PR #{pr_num} not mergeable yet: {e}"),
                            );
                        }
                    }
                }
                "DIRTY" => {
                    update_queue("conflicts → resolving");
                    messages::log(
                        log_tx,
                        format!(
                            "[autopilot] PR #{pr_num} has conflicts, dispatching resolution..."
                        ),
                    );
                    resolve_conflicts(config, log_tx, pr_num, &branch).await;
                }
                other => {
                    update_queue(other);
                    messages::log(
                        log_tx,
                        format!("[autopilot] PR #{pr_num} state: {other}, skipping"),
                    );
                }
            },
            Err(e) => {
                update_queue("error");
                messages::log(
                    log_tx,
                    format!("[autopilot] Failed to check PR #{pr_num}: {e}"),
                );
            }
        }
    }

    if merged_count > 0 {
        messages::log(
            log_tx,
            format!("[autopilot] Merged {merged_count} PRs this cycle"),
        );
    }
}

async fn pull_latest_main(config: &Config, log_tx: &mpsc::UnboundedSender<LogMessage>) {
    let default_branch = config.default_branch();
    let out = tokio::process::Command::new("git")
        .args(["-C", &config.repo_root, "fetch", "origin", &default_branch])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            messages::log(
                log_tx,
                format!("[autopilot] Fetched latest {default_branch}"),
            );
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            messages::log(log_tx, format!("[autopilot] Fetch warning: {stderr}"));
        }
        Err(e) => {
            messages::log(log_tx, format!("[autopilot] Fetch error: {e}"));
        }
    }
}

/// Resolve merge conflicts by sending the rebase instruction to Claude in a tmux window.
/// If the issue already has a tmux window (with idle Claude), sends the prompt there.
/// Otherwise, creates a new tmux window in the worktree and launches `claude --continue`.
/// Returns true if the instruction was dispatched (not necessarily completed — the caller
/// should monitor the worker and retry merge later).
async fn resolve_conflicts(
    config: &Config,
    log_tx: &mpsc::UnboundedSender<LogMessage>,
    pr_num: u64,
    branch: &str,
) -> bool {
    let default_branch = config.default_branch();

    // Extract issue number from branch to find worktree
    let issue_num: u64 = branch
        .strip_prefix(&config.branch_prefix)
        .and_then(|rest| rest.split('-').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);

    if issue_num == 0 {
        messages::log(
            log_tx,
            format!(
                "[autopilot] Could not determine issue num from branch {branch} for PR #{pr_num}"
            ),
        );
        return false;
    }

    let wt_path = config.worktree_path(issue_num);
    let window_name = config.window_name(issue_num);
    let target = format!("{}:{window_name}", config.session);

    let prompt = format!(
        "The PR for this branch has merge conflicts with {default_branch}. \
         Please: 1) fetch origin, 2) rebase onto origin/{default_branch}, \
         3) resolve any conflicts keeping both our changes and upstream changes, \
         4) push with --force-with-lease. \
         If there are no conflicts after fetch+rebase, just push."
    );

    // Check if tmux window exists for this issue
    let has_window = tokio::process::Command::new(&config.tmux)
        .args([
            "list-windows",
            "-t",
            &config.session,
            "-F",
            "#{window_name}",
        ])
        .output()
        .await
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.trim() == window_name)
        })
        .unwrap_or(false);

    if has_window {
        // Window exists — send the rebase prompt to the running Claude
        messages::log(
            log_tx,
            format!(
            "[autopilot] Sending rebase instruction to existing worker {window_name} (PR #{pr_num})"
        ),
        );
        let _ = tokio::process::Command::new(&config.tmux)
            .args(["send-keys", "-t", &target, "-l", &prompt])
            .output()
            .await;
        let _ = tokio::process::Command::new(&config.tmux)
            .args(["send-keys", "-t", &target, "Enter"])
            .output()
            .await;
        return true;
    }

    // No tmux window — check worktree exists, create window, launch claude --continue
    if !std::path::Path::new(&wt_path).exists() {
        // Worktree doesn't exist — recreate it from the remote branch
        messages::log(
            log_tx,
            format!("[autopilot] Recreating worktree for issue #{issue_num} (PR #{pr_num})..."),
        );
        let create_out = tokio::process::Command::new("git")
            .args(["-C", &config.repo_root, "worktree", "add", &wt_path, branch])
            .output()
            .await;
        match create_out {
            Ok(o) if o.status.success() => {
                messages::log(
                    log_tx,
                    format!("[autopilot] Worktree recreated at {wt_path} for #{issue_num}"),
                );
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                messages::log(
                    log_tx,
                    format!("[autopilot] Failed to recreate worktree for #{issue_num}: {stderr}"),
                );
                return false;
            }
            Err(e) => {
                messages::log(
                    log_tx,
                    format!("[autopilot] git worktree add failed for #{issue_num}: {e}"),
                );
                return false;
            }
        }
    }

    messages::log(log_tx, format!(
        "[autopilot] Creating tmux window for issue #{issue_num} (PR #{pr_num}) to resolve conflicts..."
    ));

    // Ensure session exists
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-session", "-d", "-s", &config.session])
        .output()
        .await;

    // Create new window
    let _ = tokio::process::Command::new(&config.tmux)
        .args(["new-window", "-t", &config.session, "-n", &window_name])
        .output()
        .await;

    // Launch claude --continue in the worktree with the rebase prompt
    let flags = config.claude_flags.join(" ");
    let script_path = format!("/tmp/cwo-conflict-{issue_num}.sh");
    let script = format!(
        "#!/bin/bash\nunset CLAUDECODE\ncd '{}'\nexec claude {flags} --continue '{}'\n",
        wt_path,
        prompt.replace('\'', "'\\''")
    );
    if let Err(e) = std::fs::write(&script_path, &script) {
        messages::log(
            log_tx,
            format!("[autopilot] Failed to write conflict script for #{issue_num}: {e}"),
        );
        return false;
    }
    let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));

    let _ = tokio::process::Command::new(&config.tmux)
        .args(["send-keys", "-t", &target, &script_path, "Enter"])
        .output()
        .await;

    messages::log(
        log_tx,
        format!(
            "[autopilot] Launched conflict resolution worker for issue #{issue_num} (PR #{pr_num})"
        ),
    );
    true
}

/// Wait for delay seconds or until toggle changes.
async fn wait_or_toggle(toggle_rx: &mut watch::Receiver<bool>, delay_secs: u64) {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => {}
        _ = toggle_rx.changed() => {}
    }
}

use crate::util::now_unix;

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_selection_respects_capacity() {
        let analyses = [
            IssueAnalysis {
                issue_num: 1,
                title: "Fix login".into(),
                priority: 0.9,
                actionable: true,
                file_areas: vec!["src/auth/".into()],
                reason: "bug".into(),
                estimated_complexity: "small".into(),
            },
            IssueAnalysis {
                issue_num: 2,
                title: "Add tests".into(),
                priority: 0.5,
                actionable: true,
                file_areas: vec!["tests/".into()],
                reason: "improvement".into(),
                estimated_complexity: "medium".into(),
            },
            IssueAnalysis {
                issue_num: 3,
                title: "Refactor auth".into(),
                priority: 0.7,
                actionable: true,
                file_areas: vec!["src/auth/".into()],
                reason: "refactor".into(),
                estimated_complexity: "large".into(),
            },
        ];
        let refs: Vec<&IssueAnalysis> = analyses.iter().collect();

        // Capacity 2: should pick by priority — #1 (0.9), then #3 (0.7)
        let batch = select_batch(&refs, 2);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].issue_num, 1);
        assert_eq!(batch[1].issue_num, 3);
    }

    #[test]
    fn extract_json_array_works() {
        let text = "Here is the analysis:\n[{\"issue_num\": 1}]\nDone.";
        let arr = extract_json_array(text);
        assert_eq!(arr, Some("[{\"issue_num\": 1}]"));
    }

    #[test]
    fn extract_json_object_works() {
        let text = "Result: {\"deviated\": false}\nEnd.";
        let obj = extract_json_object(text);
        assert_eq!(obj, Some("{\"deviated\": false}"));
    }

    #[test]
    fn state_roundtrips() {
        let state = AutopilotState {
            current_batch: vec![BatchItem {
                issue_num: 42,
                title: "Test".into(),
                priority: 0.5,
                file_areas: vec!["src/".into()],
                status: BatchItemStatus::Launched,
            }],
            completed: HashMap::from([(1, "#10".into())]),
            skipped: HashSet::from([99]),
            deviation_issues: vec![100],
            last_fetch_ts: 1234567890,
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: AutopilotState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.current_batch.len(), 1);
        assert_eq!(loaded.completed.len(), 1);
        assert_eq!(loaded.skipped.len(), 1);
    }
}
