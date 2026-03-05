use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

use crate::config::Config;

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct BuilderStatus {
    #[serde(default)]
    pub prs: HashMap<String, String>, // window_name -> PR number
}

#[derive(Debug, Clone)]
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
}

pub fn compute_pipeline(
    worktree_exists: bool,
    branch_name: &str,
    pr: &Option<String>,
    status: &str,
) -> String {
    let wt = if worktree_exists { "🌳" } else { "·" };
    let br = if worktree_exists { "🌿" } else { "·" };
    let pr_part = match pr {
        Some(p) => p.clone(),
        None => "·".to_string(),
    };
    let state = match status {
        "active" => "⚡",
        "idle" => "⏸",
        "shell" => "🐚",
        "done" => "✅",
        "queued" => "⏳",
        "sleeping" => "💤",
        "posted" => "📮",
        "no-window" => "👻",
        "conflict" => "⚠️",
        "probing" => "🔍",
        _ => "?",
    };
    let _ = branch_name;
    format!("{wt}{br}{pr_part} {state}")
}

pub async fn run(
    config: Arc<Config>,
    tx: watch::Sender<Vec<WorkerState>>,
    log_tx: mpsc::UnboundedSender<String>,
    is_polling: Arc<AtomicBool>,
) {
    let mut prev_states: HashMap<String, String> = HashMap::new();
    let slow_every = config.poll_interval_secs.max(60);
    let mut slow_counter: u64 = 0;
    let mut first_run = true;

    loop {
        is_polling.store(true, Ordering::Relaxed);

        let do_slow = slow_counter == 0;
        slow_counter = (slow_counter + 1) % slow_every;

        let builder_status = load_builder_status();

        let mut states = poll_tmux_windows(&config, &builder_status);

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
                    let branch_name = config.branch_name(issue_num);
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
                    });
                    orphan_count += 1;
                }
            }

            if first_run {
                let total = states.len();
                let msg = if orphan_count > 0 {
                    format!("__TOAST_INFO_Loaded {total} workers ({orphan_count} orphaned)__")
                } else {
                    format!("__TOAST_INFO_Loaded {total} workers__")
                };
                let _ = log_tx.send(msg);
                first_run = false;
            }
        }

        // Detect state transitions
        for w in &states {
            if let Some(prev) = prev_states.get(&w.window_name) {
                if prev != &w.status {
                    let toast = match (prev.as_str(), w.status.as_str()) {
                        (p, "active") if p != "active" => {
                            Some(format!("__TOAST_INFO_{} started working__", w.window_name))
                        }
                        ("active", "done") => {
                            Some(format!("__TOAST_SUCCESS_{} has a PR!__", w.window_name))
                        }
                        ("shell", "idle") => Some(format!(
                            "__TOAST_INFO_{} Claude relaunched__",
                            w.window_name
                        )),
                        (_, "no-window") => {
                            Some(format!("__TOAST_WARNING_{} window lost__", w.window_name))
                        }
                        _ => None,
                    };
                    if let Some(msg) = toast {
                        let _ = log_tx.send(msg);
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

fn poll_tmux_windows(config: &Config, builder_status: &BuilderStatus) -> Vec<WorkerState> {
    let Ok(out) = std::process::Command::new(&config.tmux)
        .args([
            "list-windows",
            "-t",
            &config.session,
            "-F",
            "#{window_index} #{window_name}",
        ])
        .output()
    else {
        return Vec::new();
    };

    let windows_text = String::from_utf8_lossy(&out.stdout);
    let mut states = Vec::new();

    for line in windows_text.lines() {
        let mut parts = line.splitn(2, ' ');
        let Some(idx_str) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else { continue };
        let Ok(idx) = idx_str.parse::<usize>() else {
            continue;
        };

        let pane_content = capture_pane(config, idx);
        let last_output = last_nonempty_line(&pane_content);
        let pr = builder_status.prs.get(name).cloned();
        let status = classify_state(config, &pane_content, pr.is_some());

        let issue_num_opt: Option<u64> = name
            .strip_prefix(config.window_prefix.as_str())
            .and_then(|s| s.parse().ok());

        let (worktree_exists, branch_name) = if let Some(n) = issue_num_opt {
            let wt = config.worktree_path(n);
            let br = config.branch_name(n);
            (std::path::Path::new(&wt).exists(), br)
        } else {
            (false, String::new())
        };

        let (probe, status) = if issue_num_opt.is_some() {
            read_probe(config, name, status, issue_num_opt)
        } else {
            (None, status)
        };

        let status = match issue_num_opt {
            Some(n) if crate::monitor::has_conflict_marker(n) => "conflict".to_string(),
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
        });
    }

    states
}

/// Check pane 1 (bottom split) of a window for probe activity or finished JSON.
fn read_probe(
    config: &Config,
    window_name: &str,
    status: String,
    issue_num: Option<u64>,
) -> (Option<String>, String) {
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
    let has_bottom = panes_out
        .as_ref()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == "1")
        })
        .unwrap_or(false);

    if !has_bottom {
        return (None, status);
    }

    let target = format!("{}:{}.1", config.session, window_name);
    let content = std::process::Command::new(&config.tmux)
        .args(["capture-pane", "-t", &target, "-p"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let finished = config.is_shell_prompt(&content);

    if !finished {
        return (Some("running".to_string()), "probing".to_string());
    }

    let json_action = crate::monitor::parse_print_json(&content).and_then(|v| {
        v.get("action")
            .and_then(|a| a.as_str())
            .map(|s| s.to_string())
    });

    let _ = issue_num;
    (json_action, status)
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

fn last_nonempty_line(content: &str) -> String {
    content
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(80)
        .collect()
}

pub fn classify_state(config: &Config, pane: &str, has_pr: bool) -> String {
    let spinner_words = [
        "Crunching",
        "Brewing",
        "Baking",
        "Cogitating",
        "Thinking",
        "Analyzing",
    ];
    let is_active = spinner_words.iter().any(|w| pane.contains(w));

    let has_bypass = pane.contains("bypass permissions on");
    let has_claude_prompt = pane.contains("> ") && (has_bypass || pane.contains("claude"));

    let is_shell = config.is_shell_prompt(pane);
    let is_sleeping = pane.contains("Sleeping ");
    let has_posted = pane.contains("posted a comment");
    let pr_url_in_pane = pane.contains("/pull/")
        && (pane.contains("github.com/") || pane.contains("Created pull request"));

    if is_active {
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

fn load_builder_status() -> BuilderStatus {
    let path = "/tmp/cwo-status.json";
    let Ok(content) = std::fs::read_to_string(path) else {
        return BuilderStatus::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg(prompts: &[&str]) -> Config {
        Config {
            session: "test".to_string(),
            repo: "owner/repo".to_string(),
            discussion_issue: 1,
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
        assert!(pipeline.contains("🌳"));
        assert!(pipeline.contains("#42"));
        assert!(pipeline.contains("⚡"));
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
