use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::messages::{LogMessage, ToastLevel};
use crate::state::StateDir;

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct BuilderStatus {
    #[serde(default)]
    pub prs: HashMap<String, String>, // window_name -> PR number
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkerState {
    pub window_index: usize,
    pub window_name: String,
    /// Pane/Claude state: "active" | "idle" | "shell" | "done" | "queued" | "sleeping" | "posted" | "no-window" | "probing"
    pub status: String,
    pub pr: Option<String>,
    pub last_output: String,
    /// Whether the worktree directory exists on disk
    pub worktree_exists: bool,
    /// The feature branch name for this issue
    pub branch_name: String,
    /// Richer pipeline status for at-a-glance: WT→BR→PR
    pub pipeline: String,
    /// Last result from a --print probe in the bottom split pane
    pub probe: Option<String>,
    /// What process is running in the main pane: "claude" | "claude-print" | "bash" | "zsh" | etc.
    pub process: String,
    /// GitHub issue title (fetched once, cached)
    pub issue_title: Option<String>,
    /// Whether this worker's PR has been merged on GitHub
    pub pr_merged: bool,
    /// Merge state from GitHub: CLEAN, BEHIND, BLOCKED, UNSTABLE, UNKNOWN
    pub pr_merge_state: Option<String>,
}

impl WorkerState {
    /// Parse PR number from the pr field, stripping any '#' prefix.
    pub fn pr_num(&self) -> Option<u64> {
        self.pr
            .as_ref()
            .and_then(|s| s.strip_prefix('#').unwrap_or(s).parse::<u64>().ok())
    }
}

pub fn compute_pipeline(
    worktree_exists: bool,
    branch_name: &str,
    pr: &Option<String>,
    status: &str,
) -> String {
    let _ = branch_name;

    // Phase: what lifecycle stage this worker is in
    let phase = match (status, worktree_exists, pr.is_some()) {
        ("waiting", _, _) => "WAITING",
        ("queued", _, _) => "QUEUED",
        ("no-window", _, _) => "ORPHAN",
        ("failed", _, _) => "FAILED",
        ("stale", _, _) => "STALE",
        (_, false, _) => "INIT",
        (_, true, false) => match status {
            "active" => "CODING",
            "idle" | "sleeping" => "PAUSED",
            "shell" => "CRASHED",
            "conflict" => "CONFLICT",
            "probing" => "PROBING",
            _ => "WORKING",
        },
        (_, true, true) => match status {
            "active" => "PR FIXING",
            "done" | "posted" => "PR READY",
            "idle" => "PR IDLE",
            "conflict" => "PR CONFLICT",
            "probing" => "PR PROBING",
            _ => "PR'd",
        },
    };

    // Progress: WT → PR
    let wt = if worktree_exists { "●" } else { "○" };
    let pr_dot = if pr.is_some() { "●" } else { "○" };

    let pr_label = match pr {
        Some(p) => format!(" {p}"),
        None => String::new(),
    };

    format!("{wt}→{pr_dot} {phase}{pr_label}")
}

pub async fn run(
    config: Arc<Config>,
    tx: watch::Sender<Vec<WorkerState>>,
    log_tx: mpsc::UnboundedSender<LogMessage>,
    is_polling: Arc<AtomicBool>,
    state_dir: Arc<StateDir>,
) {
    let mut prev_states: HashMap<String, String> = HashMap::new();
    // Track pane content hashes for stale detection: window_name -> (hash, last_change_unix)
    let mut content_hashes: HashMap<String, (u64, u64)> = HashMap::new();
    // Cache GitHub issue titles: issue_num -> title
    let mut title_cache: HashMap<u64, String> = HashMap::new();
    let slow_every = config.poll_interval_secs.max(60);
    let mut slow_counter: u64 = 0;
    let mut first_run = true;

    loop {
        is_polling.store(true, Ordering::Relaxed);

        let do_slow = slow_counter == 0;
        slow_counter = (slow_counter + 1) % slow_every;

        let builder_status = load_builder_status(&state_dir.builder_status());

        let mut states = poll_tmux_windows(&config, &builder_status, &state_dir);

        // Stale detection: track content hash changes
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let rt_stale_timeout = crate::config::RuntimeConfig::load(&state_dir.runtime_config())
            .map(|r| r.stale_timeout_secs)
            .unwrap_or(config.stale_timeout_secs);
        for w in &mut states {
            let hash = simple_hash(&w.last_output);
            let entry = content_hashes
                .entry(w.window_name.clone())
                .or_insert((hash, now_ts));
            if entry.0 != hash {
                entry.0 = hash;
                entry.1 = now_ts;
            }
            let unchanged_secs = now_ts.saturating_sub(entry.1);
            let is_terminal = matches!(
                w.status.as_str(),
                // genuinely finished states
                "done" | "queued" | "no-window" | "posted" | "failed"
                // idle = Claude is at the bypass prompt or plan-mode approval screen,
                // deliberately waiting for input — not a stuck worker
                | "idle"
            );
            if !is_terminal && rt_stale_timeout > 0 && unchanged_secs >= rt_stale_timeout {
                w.status = "stale".to_string();
                w.pipeline = compute_pipeline(w.worktree_exists, &w.branch_name, &w.pr, &w.status);
            }
        }

        // Slow path: refresh PR status from GitHub (works with or without builder)
        if (do_slow || first_run) && !config.repo_root.is_empty() {
            crate::monitor::write_builder_status(&config, &log_tx, &state_dir.builder_status())
                .await;
            // Re-read the freshly written status so this poll cycle sees updated PRs
            let fresh_status = load_builder_status(&state_dir.builder_status());
            for w in &mut states {
                if let Some(pr) = fresh_status.prs.get(&w.window_name) {
                    w.pr = Some(pr.clone());
                    w.pipeline =
                        compute_pipeline(w.worktree_exists, &w.branch_name, &w.pr, &w.status);
                }
            }
        }

        // Slow path: check which PRs have been merged on GitHub
        if (do_slow || first_run) && !config.repo.is_empty() {
            if let Ok(merged_nums) = crate::github::list_merged_pr_numbers(&config.repo).await {
                let merged_set: std::collections::HashSet<u64> = merged_nums.into_iter().collect();
                for w in &mut states {
                    if let Some(pr_num) = w.pr_num() {
                        if merged_set.contains(&pr_num) {
                            w.pr_merged = true;
                        }
                    }
                }
            }
        }

        // Slow path: fetch merge state for unmerged PRs
        if (do_slow || first_run) && !config.repo.is_empty() {
            let mut set = tokio::task::JoinSet::new();
            for w in &states {
                if !w.pr_merged {
                    if let Some(pr_num) = w.pr_num() {
                        let repo = config.repo.clone();
                        let branch = w.branch_name.clone();
                        set.spawn(async move {
                            let info = crate::github::get_pr_info(&repo, pr_num, &branch).await;
                            (pr_num, info.map(|i| i.merge_state))
                        });
                    }
                }
            }
            let mut merge_states = std::collections::HashMap::new();
            while let Some(Ok((pr_num, Ok(state)))) = set.join_next().await {
                merge_states.insert(pr_num, state);
            }
            for w in &mut states {
                if let Some(pr_num) = w.pr_num() {
                    if let Some(ms) = merge_states.remove(&pr_num) {
                        w.pr_merge_state = Some(ms);
                    }
                }
            }
        }

        // Fetch missing issue titles (parallel on slow path, cache on fast path)
        {
            let mut missing: Vec<u64> = Vec::new();
            for w in &states {
                if let Some(n) = w
                    .window_name
                    .strip_prefix(config.window_prefix.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    if !title_cache.contains_key(&n) {
                        missing.push(n);
                    }
                }
            }
            if !missing.is_empty() && (first_run || do_slow) {
                let mut set = tokio::task::JoinSet::new();
                for &n in &missing {
                    let repo = config.repo.clone();
                    set.spawn(async move { (n, crate::github::get_issue(&repo, n).await) });
                }
                while let Some(Ok((n, result))) = set.join_next().await {
                    if let Ok((title, _)) = result {
                        if !title.is_empty() {
                            title_cache.insert(n, title);
                        }
                    }
                }
            }
            for w in &mut states {
                if let Some(n) = w
                    .window_name
                    .strip_prefix(config.window_prefix.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    if let Some(title) = title_cache.get(&n) {
                        w.issue_title = Some(title.clone());
                    }
                }
            }
        }

        // Slow path: merge orphaned worktrees
        if (do_slow || first_run) && !config.repo_root.is_empty() {
            let worktree_issues = scan_worktrees(&config);
            let tmux_names: Vec<String> = states.iter().map(|w| w.window_name.clone()).collect();

            let mut orphan_count = 0;
            for issue_num in worktree_issues {
                let name = config.window_name(issue_num);
                if !tmux_names.contains(&name) {
                    let pr = builder_status.prs.get(&name).cloned();
                    let worktree_path = config.worktree_path(issue_num);
                    let worktree_exists = std::path::Path::new(&worktree_path).exists();
                    let branch_name = if worktree_exists {
                        read_worktree_branch(&worktree_path)
                            .unwrap_or_else(|| config.branch_name(issue_num))
                    } else {
                        config.branch_name(issue_num)
                    };
                    let pipeline =
                        compute_pipeline(worktree_exists, &branch_name, &pr, "no-window");
                    states.push(WorkerState {
                        window_index: usize::MAX,
                        window_name: name,
                        status: "no-window".to_string(),
                        pr,
                        last_output: "(orphaned worktree)".to_string(),
                        worktree_exists,
                        branch_name,
                        pipeline,
                        probe: None,
                        process: String::new(),
                        issue_title: None,
                        pr_merged: false,
                        pr_merge_state: None,
                    });
                    orphan_count += 1;
                }
            }

            if first_run {
                let total = states.len();
                let msg = if orphan_count > 0 {
                    format!("Loaded {total} workers ({orphan_count} orphaned)")
                } else {
                    format!("Loaded {total} workers")
                };
                crate::messages::toast(&log_tx, ToastLevel::Info, msg);
                first_run = false;
            }
        }

        // Add pending DAG tasks as phantom workers
        if !config.tasks.is_empty() {
            let dag_state = load_dag_state(&state_dir.dag_state());
            let tmux_names: Vec<String> = states.iter().map(|w| w.window_name.clone()).collect();
            for task in &config.tasks {
                let wn = config.task_window_name(&task.name);
                if !tmux_names.contains(&wn) && !dag_state.completed.contains(&task.name) {
                    let waiting_on: Vec<&str> = task
                        .depends_on
                        .iter()
                        .filter(|d| !dag_state.completed.contains(d.as_str()))
                        .map(|s| s.as_str())
                        .collect();
                    let status = if waiting_on.is_empty() {
                        "queued".to_string()
                    } else {
                        "waiting".to_string()
                    };
                    let last_output = if waiting_on.is_empty() {
                        "(ready to launch)".to_string()
                    } else {
                        format!("waiting on: {}", waiting_on.join(", "))
                    };
                    let pipeline = compute_pipeline(false, "", &None, &status);
                    states.push(WorkerState {
                        window_index: usize::MAX,
                        window_name: wn,
                        status,
                        pr: None,
                        last_output,
                        worktree_exists: false,
                        branch_name: config.task_branch_name(&task.name),
                        pipeline,
                        probe: None,
                        process: String::new(),
                        issue_title: None,
                        pr_merged: false,
                        pr_merge_state: None,
                    });
                }
            }
        }

        // Detect state transitions
        for w in &states {
            if let Some(prev) = prev_states.get(&w.window_name) {
                if prev != &w.status {
                    let toast: Option<(ToastLevel, String)> =
                        match (prev.as_str(), w.status.as_str()) {
                            (p, "active") if p != "active" => Some((
                                ToastLevel::Info,
                                format!("{} started working", w.window_name),
                            )),
                            ("active", "done") => {
                                Some((ToastLevel::Success, format!("{} has a PR!", w.window_name)))
                            }
                            ("shell", "idle") => Some((
                                ToastLevel::Info,
                                format!("{} Claude relaunched", w.window_name),
                            )),
                            (_, "no-window") => Some((
                                ToastLevel::Warning,
                                format!("{} window lost", w.window_name),
                            )),
                            _ => None,
                        };
                    if let Some((level, msg)) = toast {
                        crate::messages::toast(&log_tx, level, msg);
                    }
                }
            }
        }

        prev_states.clear();
        for w in &states {
            prev_states.insert(w.window_name.clone(), w.status.clone());
        }

        let _ = tx.send(states);
        is_polling.store(false, Ordering::Relaxed);

        tokio::time::sleep(tokio::time::Duration::from_secs(config.poll_interval_secs)).await;
    }
}

/// Scan the worktree dir for `<window_prefix>N` directories and return sorted issue numbers.
pub fn scan_worktrees(config: &Config) -> Vec<u64> {
    let worktrees_dir = format!("{}/{}", config.repo_root, config.worktree_dir);
    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        return Vec::new();
    };

    let prefix = config.window_prefix.as_str();
    let mut issues: Vec<u64> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name_str = name.to_string_lossy().to_string();
            name_str
                .strip_prefix(prefix)
                .and_then(|rest| rest.parse::<u64>().ok())
        })
        .collect();

    issues.sort_unstable();
    issues
}

fn poll_tmux_windows(
    config: &Config,
    builder_status: &BuilderStatus,
    state_dir: &StateDir,
) -> Vec<WorkerState> {
    let Ok(out) = std::process::Command::new(&config.tmux)
        .args([
            "list-windows",
            "-t",
            &config.session,
            "-F",
            "#{window_index} #{pane_current_command} #{window_name}",
        ])
        .output()
    else {
        return Vec::new();
    };

    let windows_text = String::from_utf8_lossy(&out.stdout);
    let mut states = Vec::new();

    for line in windows_text.lines() {
        let mut parts = line.splitn(3, ' ');
        let Some(idx_str) = parts.next() else {
            continue;
        };
        let Some(pane_cmd) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else { continue };
        let Ok(idx) = idx_str.parse::<usize>() else {
            continue;
        };
        let process = pane_cmd.to_string();

        // Skip windows that are not issue workers or DAG tasks
        let is_issue_window = name.starts_with(config.window_prefix.as_str());
        let is_task_window = name.starts_with("t-");
        if !is_issue_window && !is_task_window {
            continue;
        }

        let pane_content = capture_pane(config, idx);
        let last_output = last_nonempty_line(&pane_content);
        let pr = builder_status.prs.get(name).cloned();
        let status = classify_state(config, &pane_content, pr.is_some());

        let issue_num_opt: Option<u64> = name
            .strip_prefix(config.window_prefix.as_str())
            .and_then(|s| s.parse().ok());

        // Check if this is a DAG task window (t-<name>)
        let task_name_opt: Option<&str> = name.strip_prefix("t-");

        let (worktree_exists, branch_name) = if let Some(n) = issue_num_opt {
            let wt = config.worktree_path(n);
            let exists = std::path::Path::new(&wt).exists();
            let br = if exists {
                read_worktree_branch(&wt).unwrap_or_else(|| config.branch_name(n))
            } else {
                config.branch_name(n)
            };
            (exists, br)
        } else if let Some(tn) = task_name_opt {
            let wt = config.task_worktree_path(tn);
            let exists = std::path::Path::new(&wt).exists();
            let br = if exists {
                read_worktree_branch(&wt).unwrap_or_else(|| config.task_branch_name(tn))
            } else {
                config.task_branch_name(tn)
            };
            (exists, br)
        } else {
            (false, String::new())
        };

        let (probe, status) = if issue_num_opt.is_some() {
            read_probe(config, name, status, issue_num_opt)
        } else {
            (None, status)
        };

        let status = match issue_num_opt {
            Some(n) if state_dir.worker_failed(n).exists() => "failed".to_string(),
            Some(n) if state_dir.conflict(n).exists() => "conflict".to_string(),
            _ => status,
        };
        let pipeline = compute_pipeline(worktree_exists, &branch_name, &pr, &status);

        states.push(WorkerState {
            window_index: idx,
            window_name: name.to_string(),
            status,
            pr,
            last_output,
            worktree_exists,
            branch_name,
            pipeline,
            probe,
            process,
            issue_title: None,
            pr_merged: false,
            pr_merge_state: None,
        });
    }

    states
}

/// Check the bottom split pane of a window for probe activity or finished JSON.
/// Uses pane count to detect probe pane (fixes pane-base-index=1 bug).
fn read_probe(
    config: &Config,
    window_name: &str,
    status: String,
    issue_num: Option<u64>,
) -> (Option<String>, String) {
    // List all pane indices. With pane-base-index=1, the main pane is index 1;
    // a probe split pane only exists when there are 2+ panes (highest index).
    let panes_out = std::process::Command::new(&config.tmux)
        .args([
            "list-panes",
            "-t",
            &format!("{}:{}", config.session, window_name),
            "-F",
            "#{pane_index}",
        ])
        .output()
        .ok();
    let indices: Vec<usize> = panes_out
        .as_ref()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<usize>().ok())
                .collect()
        })
        .unwrap_or_default();

    if indices.len() < 2 {
        return (None, status);
    }
    let probe_idx = *indices.iter().max().unwrap();
    let target = format!("{}:{}.{}", config.session, window_name, probe_idx);

    // Use pane_current_command — shell means probe is done, anything else = still running.
    let current_cmd = std::process::Command::new(&config.tmux)
        .args([
            "display-message",
            "-t",
            &target,
            "-p",
            "#{pane_current_command}",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let probe_running = !matches!(current_cmd.as_str(), "zsh" | "bash" | "sh" | "fish" | "");

    if probe_running {
        return (Some("running".to_string()), "probing".to_string());
    }

    // Probe finished — capture content to parse JSON result
    let content = std::process::Command::new(&config.tmux)
        .args(["capture-pane", "-t", &target, "-p", "-S", "-200"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let json_action = crate::monitor::parse_print_json(&content).and_then(|v| {
        v.get("action")
            .and_then(|a| a.as_str())
            .map(|s| s.to_string())
    });

    let _ = issue_num;
    (json_action.clone(), status)
}

pub fn capture_pane(config: &Config, window_index: usize) -> String {
    let target = format!("{}:{}", config.session, window_index);
    let Ok(out) = std::process::Command::new(&config.tmux)
        .args(["capture-pane", "-t", &target, "-p", "-S", "-500"])
        .output()
    else {
        return String::new();
    };
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Read the branch name from a git worktree without spawning a subprocess.
/// Worktrees have a `.git` *file* (not dir) pointing to the gitdir, whose HEAD has the ref.
fn read_worktree_branch(worktree: &str) -> Option<String> {
    let git_path = std::path::Path::new(worktree).join(".git");
    let content = std::fs::read_to_string(&git_path).ok()?;

    // Regular repo: .git is a directory; worktree: .git is a file with "gitdir: ..."
    let head_path = if git_path.is_dir() {
        git_path.join("HEAD")
    } else {
        let gitdir = content.strip_prefix("gitdir: ")?.trim();
        std::path::PathBuf::from(gitdir).join("HEAD")
    };

    let head = std::fs::read_to_string(head_path).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(|s| s.to_string())
}

fn pane_line_is_noise(t: &str) -> bool {
    t.is_empty()
        || t == "❯"
        || t == ">>"
        || t == "$"
        || t.starts_with("bypass permissions on")
        || t.starts_with("⏵⏵")
        || t.starts_with("ctrl-")
        || t.starts_with("shift+")
        || t.starts_with("───")
        || t.starts_with("---")
        || t.starts_with("══")
        || t.starts_with("  ⏵")
        || (t.starts_with("──") && t.ends_with("──"))
        // Claude 4 idle footer tip line: "⎿  Tip: Use ctrl+v to paste a file path"
        || t.contains("Tip: Use ctrl")
}

/// Returns the last `n` non-noise lines from a raw pane capture, in order (oldest first).
pub fn clean_pane_lines(content: &str, n: usize) -> Vec<String> {
    let mut lines: Vec<String> = content
        .lines()
        .filter(|l| !pane_line_is_noise(l.trim()))
        .rev()
        .take(n)
        .map(|l| l.to_string())
        .collect();
    lines.reverse();
    lines
}

fn last_nonempty_line(content: &str) -> String {
    // Skip Claude TUI chrome and shell prompt lines to surface actual output
    content
        .lines()
        .rev()
        .find(|l| !pane_line_is_noise(l.trim()))
        .unwrap_or("")
        .trim()
        .chars()
        .take(80)
        .collect()
}

pub fn classify_state(config: &Config, pane: &str, has_pr: bool) -> String {
    let spinner_words = [
        // Claude 3 spinner words
        "Crunching",
        "Brewing",
        "Baking",
        "Cogitating",
        "Thinking",
        "Analyzing",
        // Claude 4 thinking / extended-thinking states
        "Propagating",
        "Skedaddling",
        "Compacting",
        "Undulating",
        "Marinating",
    ];

    // Only scan the last 30 lines for live-state signals (active spinner, idle
    // footer) to avoid false positives from historical scrollback content.
    let recent: String = pane
        .lines()
        .rev()
        .take(30)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    // Also catch any unknown Claude 4 spinner via the "Word… (Xm Ys ·" pattern
    let is_active = spinner_words.iter().any(|w| recent.contains(w)) || recent.contains("… (");

    let has_bypass = recent.contains("bypass permissions on")
        // Claude 4 idle footer: "⎿  Tip: Use ctrl+v to paste a file path"
        || recent.contains("Tip: Use ctrl");
    let has_claude_prompt = recent.contains("> ") && (has_bypass || recent.contains("claude"));
    let in_plan_mode =
        recent.contains("plan mode on") || recent.contains("Would you like to proceed?");

    let is_shell = config.is_shell_prompt(pane);
    let is_sleeping = pane.contains("Sleeping ");
    let has_posted = pane.contains("posted a comment");
    let pr_url_in_pane = pane.contains("/pull/")
        && (pane.contains("github.com/") || pane.contains("Created pull request"));

    if in_plan_mode {
        // Plan mode takes priority — worker is paused waiting for user approval
        "idle".to_string()
    } else if is_active {
        "active".to_string()
    } else if has_posted {
        "posted".to_string()
    } else if is_sleeping {
        "sleeping".to_string()
    } else if (has_bypass || has_claude_prompt) && (has_pr || pr_url_in_pane) {
        "done".to_string()
    } else if has_bypass || has_claude_prompt {
        "idle".to_string()
    } else if is_shell && (has_pr || pr_url_in_pane) {
        "done".to_string()
    } else if is_shell {
        let had_claude = pane.contains("claude") || pane.contains("Implement");
        if had_claude {
            "shell".to_string()
        } else {
            "queued".to_string()
        }
    } else {
        "unknown".to_string()
    }
}

fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 5381;
    for b in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u64);
    }
    hash
}

/// DAG task scheduler state, persisted to /tmp/cwo-dag-state.json.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DagState {
    pub launched: std::collections::HashSet<String>,
    pub completed: std::collections::HashSet<String>,
}

pub fn load_dag_state(path: &std::path::Path) -> DagState {
    crate::util::load_json(path).unwrap_or_default()
}

pub fn save_dag_state(state: &DagState, path: &std::path::Path) {
    crate::util::save_json(path, state);
}

fn load_builder_status(path: &std::path::Path) -> BuilderStatus {
    crate::util::load_json(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg(prompts: &[&str]) -> Config {
        Config {
            config_path: String::new(),
            session: "test".to_string(),
            repo: "owner/repo".to_string(),
            discussion_issue: Some(1),
            repo_root: "/tmp/repo".to_string(),
            tmux: "/usr/bin/tmux".to_string(),
            worktree_dir: ".claude/worktrees".to_string(),
            branch_prefix: "feature/issue-".to_string(),
            window_prefix: "issue-".to_string(),
            shell_prompts: prompts.iter().map(|s| s.to_string()).collect(),
            max_concurrent: 3,
            builder_sleep_secs: 300,
            poll_interval_secs: 1,
            run_builder: true,
            merge_policy: "auto".to_string(),
            auto_review: true,
            review_timeout_secs: 600,
            auto_relaunch: true,
            max_relaunch_attempts: 3,
            stale_timeout_secs: 300,
            claude_flags: vec!["--dangerously-skip-permissions".to_string()],
            tasks: Vec::new(),
            issues: Vec::new(),
            actions: Vec::new(),
            dashboard_port: None,
            post_worktree_create: Vec::new(),
            autopilot: false,
            autopilot_batch_size: 10,
            autopilot_batch_delay_secs: 60,
            autopilot_labels: Vec::new(),
            autopilot_exclude_labels: Vec::new(),
        }
    }

    #[test]
    fn classify_active_when_spinner_present() {
        let c = cfg(&["$ "]);
        let pane = "Crunching the data...";
        assert_eq!(classify_state(&c, pane, false), "active");
    }

    #[test]
    fn classify_idle_when_bypass_present() {
        let c = cfg(&["$ "]);
        let pane = "bypass permissions on\n> ";
        assert_eq!(classify_state(&c, pane, false), "idle");
    }

    #[test]
    fn classify_done_when_idle_plus_pr() {
        let c = cfg(&["$ "]);
        let pane = "bypass permissions on\n> ";
        assert_eq!(classify_state(&c, pane, true), "done");
    }

    #[test]
    fn classify_shell_when_had_claude() {
        let c = cfg(&["user@host "]);
        let pane = "exec claude --dangerously-skip-permissions\nuser@host my-machine$ ";
        assert_eq!(classify_state(&c, pane, false), "shell");
    }

    #[test]
    fn classify_queued_when_fresh_shell() {
        let c = cfg(&["user@host "]);
        let pane = "user@host my-machine$ ";
        assert_eq!(classify_state(&c, pane, false), "queued");
    }

    #[test]
    fn compute_pipeline_formats_correctly() {
        let pipeline =
            compute_pipeline(true, "feature/issue-1", &Some("#42".to_string()), "active");
        assert!(pipeline.contains("●→●"));
        assert!(pipeline.contains("#42"));
        assert!(pipeline.contains("PR FIXING"));

        let coding = compute_pipeline(true, "feature/issue-1", &None, "active");
        assert!(coding.contains("●→○"));
        assert!(coding.contains("CODING"));

        let queued = compute_pipeline(false, "", &None, "queued");
        assert!(queued.contains("QUEUED"));
    }

    #[test]
    fn scan_worktrees_returns_empty_for_missing_dir() {
        let c = cfg(&["$ "]);
        let c2 = Config {
            repo_root: "/nonexistent/path/xyz".to_string(),
            ..c
        };
        let result = scan_worktrees(&c2);
        assert!(result.is_empty());
    }
}
