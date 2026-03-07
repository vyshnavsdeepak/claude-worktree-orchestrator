use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::config::{Config, RuntimeConfig};
use crate::github;
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
    let path = state_dir.autopilot_state();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(state_dir: &StateDir, state: &AutopilotState) {
    let path = state_dir.autopilot_state();
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, json);
    }
}

// ─── Main loop ───────────────────────────────────────────────────────────────

pub async fn run(
    config: Arc<Config>,
    worker_rx: watch::Receiver<Vec<WorkerState>>,
    log_tx: mpsc::UnboundedSender<String>,
    prompt_tx: mpsc::UnboundedSender<String>,
    state_dir: Arc<StateDir>,
    mut toggle_rx: watch::Receiver<bool>,
) {
    let mut state = load_state(&state_dir);

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

        let _ = log_tx.send("[autopilot] Starting batch cycle".to_string());

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
                let _ = log_tx.send(format!("[autopilot] Fetch error: {e}"));
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
            let _ = log_tx.send("[autopilot] No candidate issues found".to_string());
            send_status(&log_tx, "no issues, waiting...");
            save_state(&state_dir, &state);
            wait_or_toggle(&mut toggle_rx, rt.autopilot_batch_delay_secs).await;
            continue;
        }

        let _ = log_tx.send(format!(
            "[autopilot] {} candidate issues found",
            candidates.len()
        ));

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
                let _ = log_tx.send(format!("[autopilot] Analysis error: {e}"));
                send_status(&log_tx, "analysis error, retrying...");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };

        // Mark non-actionable as skipped
        for a in &analyses {
            if !a.actionable {
                state.skipped.insert(a.issue_num);
                let _ = log_tx.send(format!(
                    "[autopilot] Skipping #{} ({}): {}",
                    a.issue_num, a.title, a.reason
                ));
            }
        }

        // Phase 3: Select batch — conflict-minimizing
        let available_capacity = {
            let workers = worker_rx.borrow();
            let active = workers
                .iter()
                .filter(|w| !matches!(w.status.as_str(), "done" | "shell" | "failed" | "no-window"))
                .count();
            let max = RuntimeConfig::effective_max_concurrent(&config, &state_dir.runtime_config());
            max.saturating_sub(active)
        };

        if available_capacity == 0 {
            let _ =
                log_tx.send("[autopilot] No capacity, waiting for workers to finish".to_string());
            send_status(&log_tx, "at capacity, waiting...");
            save_state(&state_dir, &state);
            wait_or_toggle(&mut toggle_rx, 30).await;
            continue;
        }

        // Get file areas of currently running workers for conflict avoidance
        let running_areas: Vec<String> = state
            .current_batch
            .iter()
            .filter(|b| b.status == BatchItemStatus::Launched)
            .flat_map(|b| b.file_areas.clone())
            .collect();

        let actionable: Vec<&IssueAnalysis> = analyses.iter().filter(|a| a.actionable).collect();

        let batch = select_batch(&actionable, available_capacity, &running_areas);

        let _ = log_tx.send(format!(
            "[autopilot] Selected batch of {} issues (capacity: {})",
            batch.len(),
            available_capacity
        ));

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
                let _ = log_tx.send("[autopilot] Toggled off, pausing launches".to_string());
                break;
            }

            let msg = format!("__NEWJOB_{}__", item.issue_num);
            if prompt_tx.send(msg).is_ok() {
                item.status = BatchItemStatus::Launched;
                let _ = log_tx.send(format!(
                    "[autopilot] Launched #{} — {}",
                    item.issue_num, item.title
                ));
                send_status(&log_tx, &format!("launched #{}", item.issue_num));
            } else {
                let _ = log_tx.send(format!(
                    "[autopilot] Failed to queue #{} — channel closed",
                    item.issue_num
                ));
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

        // Merge completed PRs from this batch immediately
        merge_completed_prs(&config, &log_tx, &mut state).await;
        pull_latest_main(&config, &log_tx).await;

        let _ = log_tx.send("[autopilot] Batch complete, waiting before next cycle".to_string());
        send_status(&log_tx, "batch done, cooling down...");

        let delay = rt.autopilot_batch_delay_secs;
        wait_or_toggle(&mut toggle_rx, delay).await;
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn send_status(log_tx: &mpsc::UnboundedSender<String>, msg: &str) {
    let _ = log_tx.send(format!("__AUTOPILOT_STATUS_{msg}__"));
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

/// Select a conflict-minimizing batch of issues.
fn select_batch<'a>(
    candidates: &[&'a IssueAnalysis],
    capacity: usize,
    running_areas: &[String],
) -> Vec<&'a IssueAnalysis> {
    if candidates.is_empty() || capacity == 0 {
        return Vec::new();
    }

    // Sort by priority descending
    let mut sorted: Vec<&IssueAnalysis> = candidates.to_vec();
    sorted.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut selected: Vec<&IssueAnalysis> = Vec::new();
    let mut taken_areas: HashSet<String> = running_areas.iter().cloned().collect();

    // First pass: pick non-conflicting issues
    for candidate in &sorted {
        if selected.len() >= capacity {
            break;
        }
        let overlaps = candidate
            .file_areas
            .iter()
            .any(|area| area_conflicts(area, &taken_areas));
        if !overlaps {
            for area in &candidate.file_areas {
                taken_areas.insert(area.clone());
            }
            selected.push(candidate);
        }
    }

    // Second pass: fill remaining slots with best-effort (allow overlap)
    if selected.len() < capacity {
        let selected_nums: HashSet<u64> = selected.iter().map(|s| s.issue_num).collect();
        for candidate in &sorted {
            if selected.len() >= capacity {
                break;
            }
            if !selected_nums.contains(&candidate.issue_num) {
                selected.push(candidate);
            }
        }
    }

    selected
}

/// Check if an area conflicts with any in the taken set (prefix matching).
fn area_conflicts(area: &str, taken: &HashSet<String>) -> bool {
    for existing in taken {
        if area.starts_with(existing.as_str())
            || existing.starts_with(area)
            || (area.contains('/')
                && existing.contains('/')
                && common_prefix_depth(area, existing) >= 2)
        {
            return true;
        }
    }
    false
}

fn common_prefix_depth(a: &str, b: &str) -> usize {
    let a_parts: Vec<&str> = a.split('/').collect();
    let b_parts: Vec<&str> = b.split('/').collect();
    a_parts
        .iter()
        .zip(b_parts.iter())
        .take_while(|(x, y)| x == y)
        .count()
}

async fn monitor_batch(
    config: &Config,
    worker_rx: &watch::Receiver<Vec<WorkerState>>,
    log_tx: &mpsc::UnboundedSender<String>,
    state_dir: &StateDir,
    state: &mut AutopilotState,
    launched_nums: &HashSet<u64>,
    toggle_rx: &mut watch::Receiver<bool>,
) {
    let timeout = std::time::Duration::from_secs(3600); // 1 hour max per batch
    let start = std::time::Instant::now();

    loop {
        if !*toggle_rx.borrow() {
            let _ = log_tx.send("[autopilot] Toggled off during monitoring".to_string());
            break;
        }

        if start.elapsed() > timeout {
            let _ = log_tx.send("[autopilot] Batch monitoring timeout (1h)".to_string());
            break;
        }

        let workers = worker_rx.borrow().clone();
        let mut all_done = true;
        let mut newly_completed: Vec<BatchItem> = Vec::new();

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
                        let _ = log_tx.send(format!(
                            "[autopilot] #{} completed ({})",
                            item.issue_num, pr
                        ));
                        newly_completed.push(item.clone());
                    }
                    "shell" | "failed" => {
                        item.status = BatchItemStatus::Skipped;
                        state.completed.insert(item.issue_num, "failed".to_string());
                        let _ =
                            log_tx.send(format!("[autopilot] #{} failed/crashed", item.issue_num));
                    }
                    _ => {
                        all_done = false;
                    }
                }
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
    log_tx: &mpsc::UnboundedSender<String>,
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
                        let _ = log_tx.send(format!(
                            "[autopilot] Scope deviation detected for #{}, created follow-up #{}",
                            item.issue_num, new_num
                        ));
                        let _ = log_tx.send(format!(
                            "__TOAST_WARNING_Deviation: #{} → new #{}__",
                            item.issue_num, new_num
                        ));
                    }
                    Err(e) => {
                        let _ = log_tx
                            .send(format!("[autopilot] Failed to create deviation issue: {e}"));
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
    log_tx: &mpsc::UnboundedSender<String>,
    state: &mut AutopilotState,
) {
    let _ = log_tx.send(format!(
        "[autopilot] merge check: {} completed issues",
        state.completed.len()
    ));

    // Find open PRs matching completed issues by branch name
    let open_prs = match github::list_open_prs_with_titles(&config.repo).await {
        Ok(prs) => prs,
        Err(e) => {
            let _ = log_tx.send(format!("[autopilot] Failed to list PRs: {e}"));
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
        if let Some((pr_num, branch, title)) = open_prs
            .iter()
            .find(|(_, b, _)| b == &prefix || b.starts_with(&format!("{prefix}-")))
        {
            mergeable.push((*issue_num, *pr_num, branch.clone(), title.clone()));
        }
    }

    if mergeable.is_empty() {
        let _ = log_tx.send("[autopilot] No mergeable PRs found for completed issues".to_string());
        return;
    }

    let _ = log_tx.send(format!(
        "[autopilot] Found {} PRs to merge, determining order...",
        mergeable.len()
    ));

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
                let stat = String::from_utf8_lossy(&o.stdout).to_string();
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
    let mut remaining = pr_stats.clone();

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

    let _ = log_tx.send(format!(
        "[autopilot] Merge order: {}",
        ordered
            .iter()
            .map(|(pr, _, _)| format!("#{pr}"))
            .collect::<Vec<_>>()
            .join(" → ")
    ));

    // Merge sequentially, pulling main between each
    let mut merged_count = 0u32;
    for (pr_num, branch, title) in ordered {
        let _ = log_tx.send(format!("[autopilot] Checking PR #{pr_num}..."));

        match github::get_pr_info(&config.repo, pr_num, &branch).await {
            Ok(info) => match info.merge_state.as_str() {
                "CLEAN" => {
                    let _ = log_tx.send(format!("[autopilot] Merging PR #{pr_num}..."));
                    match github::merge_pr(&config.repo, pr_num).await {
                        Ok(()) => {
                            merged_count += 1;
                            let _ = log_tx.send(format!("[autopilot] ✓ Merged PR #{pr_num}"));
                            let _ = log_tx.send(format!("__TOAST_SUCCESS_Merged PR #{pr_num}__"));
                            let _ = log_tx.send(format!("__AUTOPILOT_MERGED_{pr_num}\t{title}__"));
                            pull_latest_main(config, log_tx).await;
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        Err(e) => {
                            let _ =
                                log_tx.send(format!("[autopilot] Merge failed PR #{pr_num}: {e}"));
                        }
                    }
                }
                "BEHIND" => {
                    let _ = log_tx.send(format!(
                        "[autopilot] PR #{pr_num} behind main, updating branch..."
                    ));
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
                    // GitHub may report UNKNOWN while recomputing states after a merge.
                    // Try merging anyway — gh will fail gracefully if not ready.
                    let _ = log_tx.send(format!(
                        "[autopilot] PR #{pr_num} state UNKNOWN, attempting merge..."
                    ));
                    match github::merge_pr(&config.repo, pr_num).await {
                        Ok(()) => {
                            merged_count += 1;
                            let _ = log_tx.send(format!("[autopilot] ✓ Merged PR #{pr_num}"));
                            let _ = log_tx.send(format!("__TOAST_SUCCESS_Merged PR #{pr_num}__"));
                            let _ = log_tx.send(format!("__AUTOPILOT_MERGED_{pr_num}\t{title}__"));
                            pull_latest_main(config, log_tx).await;
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        Err(e) => {
                            let _ = log_tx
                                .send(format!("[autopilot] PR #{pr_num} not mergeable yet: {e}"));
                        }
                    }
                }
                "DIRTY" => {
                    let _ = log_tx.send(format!(
                        "[autopilot] PR #{pr_num} has conflicts, attempting resolution..."
                    ));
                    if resolve_conflicts(config, log_tx, &branch).await {
                        let _ = log_tx.send(format!(
                            "[autopilot] PR #{pr_num} conflicts resolved, will merge next cycle"
                        ));
                    }
                }
                other => {
                    let _ =
                        log_tx.send(format!("[autopilot] PR #{pr_num} state: {other}, skipping"));
                }
            },
            Err(e) => {
                let _ = log_tx.send(format!("[autopilot] Failed to check PR #{pr_num}: {e}"));
            }
        }
    }

    if merged_count > 0 {
        let _ = log_tx.send(format!("[autopilot] Merged {merged_count} PRs this cycle"));
    }
}

async fn pull_latest_main(config: &Config, log_tx: &mpsc::UnboundedSender<String>) {
    let default_branch = config.default_branch();
    let out = tokio::process::Command::new("git")
        .args(["-C", &config.repo_root, "fetch", "origin", &default_branch])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let _ = log_tx.send(format!("[autopilot] Fetched latest {default_branch}"));
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let _ = log_tx.send(format!("[autopilot] Fetch warning: {stderr}"));
        }
        Err(e) => {
            let _ = log_tx.send(format!("[autopilot] Fetch error: {e}"));
        }
    }
}

/// Resolve merge conflicts on a branch by using `claude --continue` in the worktree
/// to rebase and fix conflicts in context. Falls back to `claude --print` if no
/// conversation exists. Returns true if conflicts were resolved and pushed.
async fn resolve_conflicts(
    config: &Config,
    log_tx: &mpsc::UnboundedSender<String>,
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
        let _ = log_tx
            .send("[autopilot] Could not determine issue num for conflict resolution".to_string());
        return false;
    }

    let wt_path = config.worktree_path(issue_num);

    // Check if worktree exists
    if !std::path::Path::new(&wt_path).exists() {
        let _ = log_tx.send(format!(
            "[autopilot] Worktree {wt_path} not found, cannot resolve conflicts"
        ));
        return false;
    }

    let prompt = format!(
        "The PR for this branch has merge conflicts with {default_branch}. \
         Please: 1) fetch origin, 2) rebase onto origin/{default_branch}, \
         3) resolve any conflicts keeping both our changes and upstream changes, \
         4) push with --force-with-lease. \
         If there are no conflicts after fetch+rebase, just push."
    );

    // Try --continue first (reuses existing conversation context)
    let _ = log_tx.send(format!(
        "[autopilot] Resolving conflicts for #{issue_num} via claude --continue..."
    ));

    let cont_out = tokio::process::Command::new("claude")
        .args([
            "--dangerously-skip-permissions",
            "--continue",
            "--print",
            &prompt,
        ])
        .current_dir(&wt_path)
        .env_remove("CLAUDECODE")
        .output()
        .await;

    let success = match cont_out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let _ = log_tx.send(format!(
                "[autopilot] Claude --continue finished for #{issue_num}"
            ));
            // Check if push succeeded by looking for force-with-lease in output
            stdout.contains("push") || stdout.contains("force-with-lease") || {
                // Verify by checking if branch is now ahead of remote
                let status = tokio::process::Command::new("git")
                    .args(["-C", &wt_path, "status", "--porcelain"])
                    .output()
                    .await;
                status
                    .map(|o| o.status.success() && o.stdout.is_empty())
                    .unwrap_or(false)
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // If --continue fails (no conversation), fall back to --print
            if stderr.contains("No previous conversation")
                || stderr.contains("no conversation")
                || stderr.contains("session")
            {
                let _ = log_tx.send(format!(
                    "[autopilot] No conversation for #{issue_num}, using fresh claude..."
                ));
                let fresh = tokio::process::Command::new("claude")
                    .args(["--dangerously-skip-permissions", "--print", &prompt])
                    .current_dir(&wt_path)
                    .env_remove("CLAUDECODE")
                    .output()
                    .await;
                fresh.map(|o| o.status.success()).unwrap_or(false)
            } else {
                let _ = log_tx.send(format!(
                    "[autopilot] Claude failed for #{issue_num}: {}",
                    stderr.chars().take(200).collect::<String>()
                ));
                false
            }
        }
        Err(e) => {
            let _ = log_tx.send(format!("[autopilot] Failed to spawn claude: {e}"));
            false
        }
    };

    if success {
        let _ = log_tx.send(format!(
            "[autopilot] ✓ Conflict resolution done for #{issue_num}"
        ));
    } else {
        let _ = log_tx.send(format!(
            "[autopilot] Conflict resolution failed for #{issue_num}"
        ));
    }

    success
}

/// Wait for delay seconds or until toggle changes.
async fn wait_or_toggle(toggle_rx: &mut watch::Receiver<bool>, delay_secs: u64) {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => {}
        _ = toggle_rx.changed() => {}
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_selection_respects_capacity() {
        let analyses = vec![
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

        // Capacity 2: should pick #1 (0.9) and #2 (0.5), skipping #3 (conflicts with #1)
        let batch = select_batch(&refs, 2, &[]);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].issue_num, 1);
        assert_eq!(batch[1].issue_num, 2);
    }

    #[test]
    fn batch_selection_avoids_running_areas() {
        let analyses = vec![
            IssueAnalysis {
                issue_num: 1,
                title: "Fix auth".into(),
                priority: 0.9,
                actionable: true,
                file_areas: vec!["src/auth/".into()],
                reason: "bug".into(),
                estimated_complexity: "small".into(),
            },
            IssueAnalysis {
                issue_num: 2,
                title: "Fix UI".into(),
                priority: 0.8,
                actionable: true,
                file_areas: vec!["src/ui/".into()],
                reason: "bug".into(),
                estimated_complexity: "small".into(),
            },
        ];
        let refs: Vec<&IssueAnalysis> = analyses.iter().collect();

        // src/auth/ is already running — should prefer #2 first
        let batch = select_batch(&refs, 1, &["src/auth/".into()]);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].issue_num, 2);
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
    fn area_conflicts_detects_prefix_overlap() {
        let mut taken = HashSet::new();
        taken.insert("src/auth/".to_string());
        assert!(area_conflicts("src/auth/login.rs", &taken));
        assert!(area_conflicts("src/auth/", &taken));
        assert!(!area_conflicts("src/ui/", &taken));
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
