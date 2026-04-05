use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// All known trackable features. Any with zero uses will appear in the "never used" section.
pub const ALL_FEATURES: &[&str] = &[
    "new_job",
    "plan_job",
    "send_prompt",
    "broadcast",
    "smart_prompt",
    "direct_prompt",
    "merge_all",
    "merge_pr",
    "interrupt_worker",
    "close_worker",
    "close_finished",
    "detail_view",
    "open_pr_browser",
    "switch_to_window",
    "toggle_log",
    "settings_panel",
    "autopilot_config",
    "run_custom_action",
    "self_update",
    "help_screen",
    "command_mode",
    "branch_rename_ai",
    "output_preview",
    "startup_confirm_launched",
    "startup_confirm_dismissed",
    "branch_conflict_reuse",
    "branch_conflict_reset",
    "dag_reset",
    "pr_check_github_state",
    "pr_check_merged",
];

use crate::util::now_unix;

#[derive(Clone)]
pub struct UsageLog {
    inner: Arc<Mutex<UsageLogInner>>,
}

struct UsageLogInner {
    file: Option<File>,
    counts: HashMap<String, u64>,
}

impl UsageLog {
    pub fn new(repo_root: &str) -> Self {
        let path = PathBuf::from(repo_root).join(".claude/cwo-usage.jsonl");

        let file = if repo_root.is_empty() {
            None
        } else {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
        };

        let counts = if let Ok(content) = std::fs::read_to_string(&path) {
            Self::parse_counts(&content)
        } else {
            HashMap::new()
        };

        UsageLog {
            inner: Arc::new(Mutex::new(UsageLogInner { file, counts })),
        }
    }

    fn parse_counts(content: &str) -> HashMap<String, u64> {
        let mut counts = HashMap::new();
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(feature) = v.get("feature").and_then(|f| f.as_str()) {
                *counts.entry(feature.to_string()).or_insert(0) += 1;
            }
        }
        counts
    }

    pub fn record(&self, feature: &str) {
        let ts = now_unix();
        let line = format!("{{\"feature\":\"{feature}\",\"ts_unix\":{ts}}}\n");
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(file) = &mut inner.file {
                let _ = file.write_all(line.as_bytes());
            }
            *inner.counts.entry(feature.to_string()).or_insert(0) += 1;
        }
    }

    /// Returns features sorted by count descending, followed by known features with 0 uses.
    pub fn summary(&self) -> Vec<(String, u64)> {
        let counts = self
            .inner
            .lock()
            .map(|i| i.counts.clone())
            .unwrap_or_default();

        let mut used: Vec<(String, u64)> = counts
            .iter()
            .filter(|(_, &v)| v > 0)
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        used.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut never: Vec<(String, u64)> = ALL_FEATURES
            .iter()
            .filter(|&&f| counts.get(f).copied().unwrap_or(0) == 0)
            .map(|&f| (f.to_string(), 0u64))
            .collect();
        never.sort_by(|a, b| a.0.cmp(&b.0));

        used.extend(never);
        used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_counts_empty() {
        let counts = UsageLog::parse_counts("");
        assert!(counts.is_empty());
    }

    #[test]
    fn parse_counts_accumulates() {
        let content = r#"{"feature":"new_job","ts_unix":1000}
{"feature":"new_job","ts_unix":1001}
{"feature":"detail_view","ts_unix":1002}
"#;
        let counts = UsageLog::parse_counts(content);
        assert_eq!(counts["new_job"], 2);
        assert_eq!(counts["detail_view"], 1);
    }

    #[test]
    fn summary_separates_used_and_never() {
        let log = UsageLog::new(""); // no file
        log.record("new_job");
        log.record("new_job");
        log.record("detail_view");
        let summary = log.summary();
        // Used items come first, sorted by count desc
        assert_eq!(summary[0], ("new_job".to_string(), 2));
        assert_eq!(summary[1], ("detail_view".to_string(), 1));
        // Never-used items at end have count 0
        let never: Vec<_> = summary.iter().filter(|(_, c)| *c == 0).collect();
        assert!(!never.is_empty());
        // "new_job" and "detail_view" should not appear in never section
        assert!(never
            .iter()
            .all(|(f, _)| f != "new_job" && f != "detail_view"));
    }
}
