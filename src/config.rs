use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TaskDef {
    /// Unique name for this task (used as window/branch identifier)
    pub name: String,

    /// The prompt to send to Claude
    pub prompt: String,

    /// Names of tasks that must complete before this one launches
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Tmux session where worker windows live
    pub session: String,

    /// GitHub repo (owner/name)
    pub repo: String,

    /// GitHub issue number used as the product discussion thread
    /// Only required when run_builder = true
    #[serde(default)]
    pub discussion_issue: Option<u64>,

    /// Git repo root (absolute path)
    pub repo_root: String,

    /// Tmux binary path
    #[serde(default = "default_tmux")]
    pub tmux: String,

    /// Worktree base dir, relative to repo_root
    #[serde(default = "default_worktree_dir")]
    pub worktree_dir: String,

    /// Feature branch prefix
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,

    /// Window name prefix for issue workers
    #[serde(default = "default_window_prefix")]
    pub window_prefix: String,

    /// Shell prompt prefixes to detect "idle shell" state
    #[serde(default = "default_shell_prompts")]
    pub shell_prompts: Vec<String>,

    /// How many Claude workers can run concurrently
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Seconds between builder discussion-scan cycles
    #[serde(default = "default_builder_sleep_secs")]
    pub builder_sleep_secs: u64,

    /// Seconds between poller ticks
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,

    /// Run the builder loop (set false for TUI-only mode)
    #[serde(default = "default_true")]
    pub run_builder: bool,

    /// Merge policy: "auto" | "review_then_merge" | "manual"
    #[serde(default = "default_merge_policy")]
    pub merge_policy: String,

    /// Whether to spawn AI reviewers for new PRs
    #[serde(default = "default_true")]
    pub auto_review: bool,

    /// Review timeout in seconds (merge anyway after this, 0 = wait forever)
    #[serde(default = "default_review_timeout_secs")]
    pub review_timeout_secs: u64,

    /// Auto-relaunch crashed workers
    #[serde(default = "default_true")]
    pub auto_relaunch: bool,

    /// Max relaunch attempts before marking worker as failed
    #[serde(default = "default_max_relaunch_attempts")]
    pub max_relaunch_attempts: u32,

    /// Mark worker stale if no output for this many seconds
    #[serde(default = "default_stale_timeout_secs")]
    pub stale_timeout_secs: u64,

    /// Extra flags passed to the `claude` CLI when launching workers
    /// e.g. ["--dangerously-skip-permissions"]
    #[serde(default = "default_claude_flags")]
    pub claude_flags: Vec<String>,

    /// Pre-defined task DAG with dependency ordering.
    /// Tasks launch automatically when their dependencies complete.
    #[serde(default)]
    pub tasks: Vec<TaskDef>,
}

fn default_tmux() -> String {
    "/opt/homebrew/bin/tmux".to_string()
}
fn default_worktree_dir() -> String {
    ".claude/worktrees".to_string()
}
fn default_branch_prefix() -> String {
    "feature/issue-".to_string()
}
fn default_window_prefix() -> String {
    "issue-".to_string()
}
fn default_shell_prompts() -> Vec<String> {
    vec!["$ ".to_string(), ">> ".to_string()]
}
fn default_max_concurrent() -> usize {
    3
}
fn default_builder_sleep_secs() -> u64 {
    300
}
fn default_poll_interval_secs() -> u64 {
    1
}
fn default_true() -> bool {
    true
}
fn default_merge_policy() -> String {
    "auto".to_string()
}
fn default_review_timeout_secs() -> u64 {
    600
}
fn default_max_relaunch_attempts() -> u32 {
    3
}
fn default_stale_timeout_secs() -> u64 {
    300
}
fn default_claude_flags() -> Vec<String> {
    vec!["--dangerously-skip-permissions".to_string()]
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read config file: {path}"))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {path}"))?;
        if !config.tasks.is_empty() {
            validate_dag(&config.tasks)?;
        }
        Ok(config)
    }

    /// Worktree path for a given issue number.
    pub fn worktree_path(&self, issue_num: u64) -> String {
        format!(
            "{}/{}/{}{}",
            self.repo_root, self.worktree_dir, self.window_prefix, issue_num
        )
    }

    /// Branch name for a given issue number (without title slug, for matching).
    pub fn branch_name(&self, issue_num: u64) -> String {
        format!("{}{}", self.branch_prefix, issue_num)
    }

    /// Branch name with a slugified title suffix for descriptive branches.
    /// e.g. "feature/issue-326-fix-permission-handling"
    pub fn branch_name_with_title(&self, issue_num: u64, title: &str) -> String {
        let slug = slugify_title(title);
        if slug.is_empty() {
            self.branch_name(issue_num)
        } else {
            format!("{}{}-{}", self.branch_prefix, issue_num, slug)
        }
    }

    /// Window name for a given issue number.
    pub fn window_name(&self, issue_num: u64) -> String {
        format!("{}{}", self.window_prefix, issue_num)
    }

    /// Window name for a DAG task.
    pub fn task_window_name(&self, task_name: &str) -> String {
        format!("t-{task_name}")
    }

    /// Branch name for a DAG task.
    pub fn task_branch_name(&self, task_name: &str) -> String {
        format!("task/{task_name}")
    }

    /// Worktree path for a DAG task.
    pub fn task_worktree_path(&self, task_name: &str) -> String {
        format!("{}/{}/t-{}", self.repo_root, self.worktree_dir, task_name)
    }

    /// Returns true if the given pane content ends with a shell prompt.
    pub fn is_shell_prompt(&self, pane: &str) -> bool {
        pane.lines().rev().take(5).any(|l| {
            let t = l.trim();
            self.shell_prompts
                .iter()
                .any(|p| t.starts_with(p.as_str()) || t == p.trim())
        })
    }
}

// ─── Runtime Config (hot-reloadable from TUI) ───────────────────────────────

const RUNTIME_CONFIG_FILE: &str = "/tmp/cwo-runtime.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeConfig {
    pub merge_policy: String,
    pub auto_review: bool,
    pub review_timeout_secs: u64,
    pub auto_relaunch: bool,
    pub max_relaunch_attempts: u32,
    pub stale_timeout_secs: u64,
}

impl RuntimeConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            merge_policy: config.merge_policy.clone(),
            auto_review: config.auto_review,
            review_timeout_secs: config.review_timeout_secs,
            auto_relaunch: config.auto_relaunch,
            max_relaunch_attempts: config.max_relaunch_attempts,
            stale_timeout_secs: config.stale_timeout_secs,
        }
    }

    pub fn load() -> Option<Self> {
        let content = std::fs::read_to_string(RUNTIME_CONFIG_FILE).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(RUNTIME_CONFIG_FILE, json);
        }
    }
}

/// Validate the task DAG: no duplicate names, all deps exist, no cycles.
fn validate_dag(tasks: &[TaskDef]) -> Result<()> {
    use std::collections::{HashMap, HashSet};

    let names: HashSet<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
    if names.len() != tasks.len() {
        // Find the duplicate
        let mut seen = HashSet::new();
        for t in tasks {
            if !seen.insert(t.name.as_str()) {
                anyhow::bail!("Duplicate task name: {}", t.name);
            }
        }
    }

    for t in tasks {
        for dep in &t.depends_on {
            if !names.contains(dep.as_str()) {
                anyhow::bail!(
                    "Task '{}' depends on '{}', which does not exist",
                    t.name,
                    dep
                );
            }
        }
    }

    // Cycle detection via DFS
    let adj: HashMap<&str, Vec<&str>> = tasks
        .iter()
        .map(|t| {
            (
                t.name.as_str(),
                t.depends_on.iter().map(|s| s.as_str()).collect(),
            )
        })
        .collect();

    #[derive(Clone, Copy, PartialEq)]
    enum DfsColor {
        White,
        Gray,
        Black,
    }
    let mut colors: HashMap<&str, DfsColor> = tasks
        .iter()
        .map(|t| (t.name.as_str(), DfsColor::White))
        .collect();

    fn dfs<'a>(
        node: &'a str,
        adj: &HashMap<&str, Vec<&'a str>>,
        colors: &mut HashMap<&'a str, DfsColor>,
    ) -> Option<String> {
        colors.insert(node, DfsColor::Gray);
        if let Some(deps) = adj.get(node) {
            for dep in deps {
                match colors.get(dep) {
                    Some(DfsColor::Gray) => {
                        return Some(format!("Cycle detected: {} -> {}", node, dep))
                    }
                    Some(DfsColor::Black) => {}
                    _ => {
                        if let Some(err) = dfs(dep, adj, colors) {
                            return Some(err);
                        }
                    }
                }
            }
        }
        colors.insert(node, DfsColor::Black);
        None
    }

    for t in tasks {
        if colors[t.name.as_str()] == DfsColor::White {
            if let Some(err) = dfs(&t.name, &adj, &mut colors) {
                anyhow::bail!(err);
            }
        }
    }

    Ok(())
}

/// Convert a title into a git-safe branch slug.
/// "Fix Permission Handling!!" → "fix-permission-handling"
fn slugify_title(title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    // Collapse runs of dashes and trim them
    let mut result = String::new();
    for c in slug.chars() {
        if c == '-' && result.ends_with('-') {
            continue;
        }
        result.push(c);
    }
    let result = result.trim_matches('-').to_string();
    // Cap at 50 chars to keep branch names reasonable, break at a dash boundary
    if result.len() <= 50 {
        result
    } else {
        match result[..50].rfind('-') {
            Some(i) => result[..i].to_string(),
            None => result[..50].to_string(),
        }
    }
}

pub const EXAMPLE_CONFIG: &str = r#"# Claude Worktree Orchestrator config

# Tmux session where worker windows live
session = "my-workers"

# GitHub repo (owner/name)
repo = "owner/repo"

# GitHub issue number used as the product discussion thread
# Only required when using the builder loop (not needed for direct prompts)
discussion_issue = 1

# Git repo root (absolute path)
repo_root = "/path/to/repo"

# Tmux binary
tmux = "/opt/homebrew/bin/tmux"

# Worktree base dir (relative to repo_root)
worktree_dir = ".claude/worktrees"

# Feature branch prefix
branch_prefix = "feature/issue-"

# Window name prefix for issue workers
window_prefix = "issue-"

# Shell prompt patterns to detect "idle shell" state
shell_prompts = ["$ ", ">> "]

# How many Claude workers can run concurrently
max_concurrent = 3

# Seconds between builder discussion-scan cycles
builder_sleep_secs = 300

# Seconds between poller ticks
poll_interval_secs = 1

# Merge policy: "auto" | "review_then_merge" | "manual"
# - auto: merge CLEAN PRs immediately
# - review_then_merge: wait for APPROVED review before merge
# - manual: never auto-merge
merge_policy = "auto"

# Spawn AI reviewers for new PRs
auto_review = true

# Review timeout in seconds (merge anyway after this, 0 = wait forever)
review_timeout_secs = 600

# Auto-relaunch crashed workers
auto_relaunch = true

# Max relaunch attempts before marking worker as failed
max_relaunch_attempts = 3

# Mark worker stale if no output for this many seconds
stale_timeout_secs = 300

# Extra flags passed to the claude CLI when launching workers
# Default: ["--dangerously-skip-permissions"]
# Set to [] to get interactive permission prompts
claude_flags = ["--dangerously-skip-permissions"]

# ─── Task DAG (optional) ─────────────────────────────────────────────
# Pre-defined tasks with dependency ordering.
# Tasks with no depends_on (or depends_on = []) start immediately.
# A task is "complete" when Claude finishes (idle/done/shell state).
#
# [[tasks]]
# name = "filing"
# prompt = "Implement the filing workflow..."
#
# [[tasks]]
# name = "scrutiny"
# prompt = "Implement the scrutiny workflow..."
# depends_on = ["filing"]
#
# [[tasks]]
# name = "edge-cases"
# prompt = "Test edge cases..."
# depends_on = []
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(prompts: &[&str]) -> Config {
        Config {
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
        }
    }

    #[test]
    fn worktree_path_uses_config() {
        let c = make_config(&["$ "]);
        assert_eq!(c.worktree_path(42), "/tmp/repo/.claude/worktrees/issue-42");
    }

    #[test]
    fn branch_name_uses_prefix() {
        let c = make_config(&["$ "]);
        assert_eq!(c.branch_name(7), "feature/issue-7");
    }

    #[test]
    fn branch_name_with_title_slugifies() {
        let c = make_config(&["$ "]);
        assert_eq!(
            c.branch_name_with_title(42, "Fix Permission Handling!!"),
            "feature/issue-42-fix-permission-handling"
        );
    }

    #[test]
    fn branch_name_with_title_truncates_long_titles() {
        let c = make_config(&["$ "]);
        let long = "implement the new user authentication system with oauth2 and jwt tokens for all endpoints";
        let branch = c.branch_name_with_title(1, long);
        // Should be prefix + number + slug capped at 50 chars
        assert!(branch.len() < 80);
        assert!(branch.starts_with("feature/issue-1-"));
        // Should not end with a dash
        assert!(!branch.ends_with('-'));
    }

    #[test]
    fn branch_name_with_empty_title_falls_back() {
        let c = make_config(&["$ "]);
        assert_eq!(c.branch_name_with_title(7, ""), "feature/issue-7");
    }

    #[test]
    fn window_name_uses_prefix() {
        let c = make_config(&["$ "]);
        assert_eq!(c.window_name(7), "issue-7");
    }

    #[test]
    fn is_shell_prompt_detects_configured_prompt() {
        let c = make_config(&["user@host", ">> "]);
        let pane = "some output\nuser@host:~$ ";
        assert!(c.is_shell_prompt(pane));
    }

    #[test]
    fn is_shell_prompt_misses_unconfigured_prompt() {
        let c = make_config(&["$ "]);
        let pane = "some output\nvyshnav@mac:~$ ";
        // starts_with("$ ") won't match "vyshnav@mac:~$ "
        assert!(!c.is_shell_prompt(pane));
    }

    #[test]
    fn example_config_parses() {
        let c: Config = toml::from_str(EXAMPLE_CONFIG).expect("example config should parse");
        assert_eq!(c.session, "my-workers");
        assert_eq!(c.repo, "owner/repo");
        assert_eq!(c.discussion_issue, Some(1));
        assert_eq!(c.max_concurrent, 3);
    }

    #[test]
    fn validate_dag_accepts_valid_dag() {
        let tasks = vec![
            TaskDef {
                name: "a".into(),
                prompt: "do a".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "b".into(),
                prompt: "do b".into(),
                depends_on: vec!["a".into()],
            },
            TaskDef {
                name: "c".into(),
                prompt: "do c".into(),
                depends_on: vec!["a".into()],
            },
            TaskDef {
                name: "d".into(),
                prompt: "do d".into(),
                depends_on: vec!["b".into(), "c".into()],
            },
        ];
        assert!(validate_dag(&tasks).is_ok());
    }

    #[test]
    fn validate_dag_rejects_cycle() {
        let tasks = vec![
            TaskDef {
                name: "a".into(),
                prompt: "".into(),
                depends_on: vec!["b".into()],
            },
            TaskDef {
                name: "b".into(),
                prompt: "".into(),
                depends_on: vec!["a".into()],
            },
        ];
        let err = validate_dag(&tasks).unwrap_err();
        assert!(err.to_string().contains("Cycle"));
    }

    #[test]
    fn validate_dag_rejects_missing_dep() {
        let tasks = vec![TaskDef {
            name: "a".into(),
            prompt: "".into(),
            depends_on: vec!["nonexistent".into()],
        }];
        let err = validate_dag(&tasks).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn validate_dag_rejects_duplicate_names() {
        let tasks = vec![
            TaskDef {
                name: "a".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
            TaskDef {
                name: "a".into(),
                prompt: "".into(),
                depends_on: vec![],
            },
        ];
        let err = validate_dag(&tasks).unwrap_err();
        assert!(err.to_string().contains("Duplicate"));
    }

    #[test]
    fn task_helpers_format_correctly() {
        let c = make_config(&["$ "]);
        assert_eq!(c.task_window_name("filing"), "t-filing");
        assert_eq!(c.task_branch_name("filing"), "task/filing");
        assert_eq!(
            c.task_worktree_path("filing"),
            "/tmp/repo/.claude/worktrees/t-filing"
        );
    }

    #[test]
    fn config_with_tasks_parses() {
        let toml_str = r#"
            session = "test"
            repo = "owner/repo"
            repo_root = "/tmp/repo"

            [[tasks]]
            name = "filing"
            prompt = "Implement filing"

            [[tasks]]
            name = "scrutiny"
            prompt = "Implement scrutiny"
            depends_on = ["filing"]
        "#;
        let c: Config = toml::from_str(toml_str).expect("should parse");
        assert_eq!(c.tasks.len(), 2);
        assert_eq!(c.tasks[0].name, "filing");
        assert_eq!(c.tasks[1].depends_on, vec!["filing"]);
    }
}
