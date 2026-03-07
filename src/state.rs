use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub struct StateDir {
    pub path: PathBuf,
}

impl StateDir {
    pub fn new(config_path: &str) -> Self {
        let hash = session_hash(config_path);
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let path = PathBuf::from(home)
            .join(".local/share/cwo/sessions")
            .join(hash);
        Self { path }
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.path)?;
        write_session_info(&self.path)
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn runtime_config(&self) -> PathBuf {
        self.file("runtime.json")
    }

    pub fn dag_state(&self) -> PathBuf {
        self.file("dag-state.json")
    }

    pub fn builder_status(&self) -> PathBuf {
        self.file("builder-status.json")
    }

    pub fn backoff(&self) -> PathBuf {
        self.file("backoff-until.txt")
    }

    pub fn backoff_resumed(&self) -> PathBuf {
        self.file("resumed.txt")
    }

    pub fn rebase_check(&self) -> PathBuf {
        self.file("last-merge-check.txt")
    }

    pub fn just_merged(&self) -> PathBuf {
        self.file("just-merged.txt")
    }

    pub fn history(&self) -> PathBuf {
        self.file("history.json")
    }

    pub fn conflict(&self, issue_num: u64) -> PathBuf {
        self.file(&format!("issue-{issue_num}-conflict.txt"))
    }

    pub fn worker_failed(&self, issue_num: u64) -> PathBuf {
        self.file(&format!("worker-{issue_num}-failed.txt"))
    }

    pub fn relaunch_count(&self, issue_num: u64) -> PathBuf {
        self.file(&format!("relaunch-{issue_num}.txt"))
    }

    pub fn review_file(&self, issue_num: u64) -> PathBuf {
        self.file(&format!("review-{issue_num}.txt"))
    }

    pub fn review_dir(&self) -> PathBuf {
        self.path.join("reviews")
    }

    pub fn autopilot_state(&self) -> PathBuf {
        self.file("autopilot-state.json")
    }
}

fn session_hash(config_path: &str) -> String {
    let canonical = std::path::Path::new(config_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(config_path));
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn write_session_info(dir: &std::path::Path) -> std::io::Result<()> {
    let info = serde_json::json!({
        "cwd": std::env::current_dir().unwrap_or_default(),
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    });
    let path = dir.join("session-info.json");
    if !path.exists() {
        std::fs::write(
            path,
            serde_json::to_string_pretty(&info).unwrap_or_default(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_hash_is_deterministic() {
        let h1 = session_hash("/some/path/cwo.toml");
        let h2 = session_hash("/some/path/cwo.toml");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn session_hash_differs_for_different_paths() {
        let h1 = session_hash("/project-a/cwo.toml");
        let h2 = session_hash("/project-b/cwo.toml");
        assert_ne!(h1, h2);
    }

    #[test]
    fn state_dir_file_helpers() {
        let sd = StateDir {
            path: PathBuf::from("/tmp/test-state"),
        };
        assert_eq!(
            sd.runtime_config(),
            PathBuf::from("/tmp/test-state/runtime.json")
        );
        assert_eq!(
            sd.dag_state(),
            PathBuf::from("/tmp/test-state/dag-state.json")
        );
        assert_eq!(
            sd.backoff(),
            PathBuf::from("/tmp/test-state/backoff-until.txt")
        );
        assert_eq!(
            sd.conflict(42),
            PathBuf::from("/tmp/test-state/issue-42-conflict.txt")
        );
        assert_eq!(
            sd.worker_failed(7),
            PathBuf::from("/tmp/test-state/worker-7-failed.txt")
        );
        assert_eq!(
            sd.relaunch_count(3),
            PathBuf::from("/tmp/test-state/relaunch-3.txt")
        );
        assert_eq!(
            sd.review_file(10),
            PathBuf::from("/tmp/test-state/review-10.txt")
        );
        assert_eq!(sd.review_dir(), PathBuf::from("/tmp/test-state/reviews"));
        assert_eq!(sd.history(), PathBuf::from("/tmp/test-state/history.json"));
        assert_eq!(
            sd.autopilot_state(),
            PathBuf::from("/tmp/test-state/autopilot-state.json")
        );
    }
}
