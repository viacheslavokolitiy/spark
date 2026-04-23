//! Request history persistence in JSONL (newline-delimited JSON) format.

use crate::http::{HttpMethod, HttpRequest};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::io::Write;

const SECONDS_IN_MINUTE: i64 = 60;
const SECONDS_IN_HOUR: i64 = 3_600;
const SECONDS_IN_DAY: i64 = 86_400;

/// A single entry stored in the history file.
#[derive(Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// HTTP method used.
    pub method: HttpMethod,
    /// Target URL.
    pub url: String,
    /// Headers that were sent.
    pub headers: Vec<(String, String)>,
    /// Request body, if any was sent.
    pub body: Option<String>,
    /// UTC timestamp of when the request was executed.
    pub timestamp: DateTime<Utc>,
}

impl HistoryEntry  {
    /// Creates a history entry from a completed [`HttpRequest`], timestamped now.
    #[must_use]
    pub fn from_request(req: &HttpRequest) -> Self {
        Self {
            method: req.method.clone(),
            url: req.url.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
            timestamp: Utc::now(),
        }
    }
}

/// Returns a human-readable relative time label for `timestamp`.
///
/// The label is stable per coarse time bucket (e.g. all entries from the same
/// minute share a label), making it suitable as a group-separator header in the
/// history sidebar.
///
/// # Examples
///
/// ```
/// # use chrono::{Duration, Utc};
/// # use spark_core::history::relative_time_label;
/// let recent = Utc::now() - Duration::seconds(30);
/// assert_eq!(relative_time_label(&recent), "Just now");
/// ```
#[must_use]
pub fn relative_time_label(timestamp: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(*timestamp);
    let secs = diff.num_seconds().max(0);

    if secs < SECONDS_IN_MINUTE {
        return "Just now".to_string();
    }
    if secs < SECONDS_IN_HOUR {
        let mins = diff.num_minutes();
        return if mins == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{mins} minutes ago")
        };
    }
    if secs < SECONDS_IN_DAY {
        let hours = diff.num_hours();
        return if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{hours} hours ago")
        };
    }

    let today = now.date_naive();
    let entry_day = timestamp.date_naive();
    let days = (today - entry_day).num_days();

    match days {
        0 | 1 => "Yesterday".to_string(),
        2..=6 => format!("{days} days ago"),
        7..=13 => "Last week".to_string(),
        _ => "Older".to_string(),
    }
}

/// Reads all history entries from a JSONL file.
///
/// Returns an empty [`Vec`] if the file does not exist, cannot be read, or
/// contains no valid entries. Malformed lines are silently skipped.
#[must_use]
pub fn load_history(path: &Path) -> Vec<HistoryEntry> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                None
            } else {
                serde_json::from_str(trimmed).ok()
            }
        })
        .collect()
}

/// Appends a single history entry to a JSONL file.
///
/// If the file does not yet exist it is created (along with any missing parent
/// directories). If it already exists it is opened directly without truncation.
///
/// # Errors
/// Returns an error if the file cannot be opened or written to.
pub fn append_history(path: &Path, entry: &HistoryEntry) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.append(true);

    if !path.exists() {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        options.create(true);
    }

    let mut file = options.open(path)?;
    let json = serde_json::to_string(entry)?;
    writeln!(file, "{json}")?;
    Ok(())
}
