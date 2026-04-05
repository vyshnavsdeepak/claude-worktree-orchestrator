//! Shared utility functions used across modules.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

/// Format a Unix timestamp (seconds since epoch) as ISO 8601 UTC.
pub fn unix_to_iso8601(ts: u64) -> String {
    let time = ts % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;
    let s = time % 60;
    let mut days = ts / 86400;

    let mut year = 1970u32;
    loop {
        let days_in_year = if is_leap(year) { 366u64 } else { 365u64 };
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

/// Current Unix timestamp in seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_leap(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Read and deserialize JSON from a file. Returns `None` on any error.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Serialize and write JSON to a file. Errors are silently ignored.
pub fn save_json<T: Serialize>(path: &Path, value: &T) {
    if let Ok(json) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(path, json);
    }
}
