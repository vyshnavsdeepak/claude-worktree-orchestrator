use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_to_iso8601(ts: u64) -> String {
    let time = ts % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;
    let s = time % 60;
    let mut days = ts / 86400;

    let mut year = 1970u32;
    loop {
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let days_in_year = if leap { 366u64 } else { 365u64 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let months = if leap {
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

#[derive(Clone)]
pub struct EventLog {
    inner: Arc<Mutex<EventLogInner>>,
}

struct EventLogInner {
    file: Option<File>,
    stats: EventStats,
}

#[derive(Default, Clone)]
pub struct EventStats {
    pub merged_count: u64,
    pub active_count: u64,
    pub failed_count: u64,
    pub total_merge_secs: u64,
    pub launch_timestamps: Vec<u64>, // unix ts of worker_launched events for avg calc
}

impl EventStats {
    pub fn avg_merge_secs(&self) -> Option<u64> {
        if self.merged_count > 0 {
            Some(self.total_merge_secs / self.merged_count)
        } else {
            None
        }
    }
}

impl EventLog {
    pub fn new(repo_root: &str) -> Self {
        let path = PathBuf::from(repo_root).join(".claude/cwo-events.jsonl");
        let file = if repo_root.is_empty() {
            None
        } else {
            // Ensure parent dir exists
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
        };

        // Scan existing events for stats
        let stats = if let Ok(content) = std::fs::read_to_string(&path) {
            Self::parse_stats(&content)
        } else {
            EventStats::default()
        };

        EventLog {
            inner: Arc::new(Mutex::new(EventLogInner { file, stats })),
        }
    }

    fn parse_stats(content: &str) -> EventStats {
        let mut stats = EventStats::default();
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match v.get("event").and_then(|e| e.as_str()) {
                Some("pr_merged") => {
                    stats.merged_count += 1;
                    if let Some(elapsed) = v.get("elapsed_secs").and_then(|e| e.as_u64()) {
                        stats.total_merge_secs += elapsed;
                    }
                }
                Some("worker_launched") => {
                    if let Some(ts) = v.get("ts_unix").and_then(|t| t.as_u64()) {
                        stats.launch_timestamps.push(ts);
                    }
                    stats.active_count += 1;
                }
                Some("worker_failed") => {
                    stats.failed_count += 1;
                }
                // worker_done decrements active
                Some("worker_done") => {
                    stats.active_count = stats.active_count.saturating_sub(1);
                }
                _ => {}
            }
        }
        stats
    }

    pub fn emit(&self, event: &str, fields: &[(&str, serde_json::Value)]) {
        let mut obj = serde_json::Map::new();
        let ts = now_unix();
        obj.insert(
            "ts".to_string(),
            serde_json::Value::String(unix_to_iso8601(ts)),
        );
        obj.insert("ts_unix".to_string(), serde_json::json!(ts));
        obj.insert(
            "event".to_string(),
            serde_json::Value::String(event.to_string()),
        );
        for (k, v) in fields {
            obj.insert(k.to_string(), v.clone());
        }

        let json = serde_json::Value::Object(obj);
        if let Ok(line) = serde_json::to_string(&json) {
            if let Ok(mut inner) = self.inner.lock() {
                if let Some(file) = &mut inner.file {
                    let _ = writeln!(file, "{line}");
                }
                // Update in-memory stats
                match event {
                    "pr_merged" => {
                        inner.stats.merged_count += 1;
                        for (k, v) in fields {
                            if *k == "elapsed_secs" {
                                if let Some(e) = v.as_u64() {
                                    inner.stats.total_merge_secs += e;
                                }
                            }
                        }
                    }
                    "worker_launched" => {
                        inner.stats.active_count += 1;
                        inner.stats.launch_timestamps.push(ts);
                    }
                    "worker_failed" => {
                        inner.stats.failed_count += 1;
                    }
                    "worker_done" => {
                        inner.stats.active_count = inner.stats.active_count.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn stats(&self) -> EventStats {
        self.inner
            .lock()
            .map(|i| i.stats.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_stats_empty() {
        let log = EventLog::new("");
        let stats = log.stats();
        assert_eq!(stats.merged_count, 0);
        assert_eq!(stats.failed_count, 0);
        assert!(stats.avg_merge_secs().is_none());
    }

    #[test]
    fn parse_stats_counts_events() {
        let content = r#"{"ts":"2026-01-01T00:00:00Z","event":"worker_launched","ts_unix":1000}
{"ts":"2026-01-01T00:10:00Z","event":"pr_merged","elapsed_secs":600}
{"ts":"2026-01-01T00:20:00Z","event":"pr_merged","elapsed_secs":1200}
{"ts":"2026-01-01T00:30:00Z","event":"worker_failed"}
"#;
        let stats = EventLog::parse_stats(content);
        assert_eq!(stats.merged_count, 2);
        assert_eq!(stats.failed_count, 1);
        assert_eq!(stats.avg_merge_secs(), Some(900));
    }

    #[test]
    fn unix_to_iso8601_epoch() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
    }
}
