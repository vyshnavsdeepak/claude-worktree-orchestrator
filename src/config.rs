use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Tmux session where worker windows live
    pub session: String,

    /// GitHub repo (owner/name)
    pub repo: String,

    /// GitHub issue number used as the product discussion thread
    pub discussion_issue: u64,

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

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read config file: {path}"))?;
        toml::from_str(&content).with_context(|| format!("Failed to parse config file: {path}"))
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
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(prompts: &[&str]) -> Config {
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
        assert_eq!(c.discussion_issue, 1);
        assert_eq!(c.max_concurrent, 3);
    }
}
