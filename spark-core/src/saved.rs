//! Saved request persistence in JSON format.

use crate::http::{HttpMethod, HttpRequest};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// A reusable request stored outside transient history.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedRequest {
    /// Human-readable saved request name.
    pub name: String,
    /// HTTP method used.
    pub method: HttpMethod,
    /// Target URL.
    pub url: String,
    /// Headers that should be sent.
    pub headers: Vec<(String, String)>,
    /// Request body, if any should be sent.
    pub body: Option<String>,
    /// UTC timestamp of when the saved request was last updated.
    pub updated_at: DateTime<Utc>,
}

impl SavedRequest {
    /// Creates a saved request from a composed [`HttpRequest`].
    #[must_use]
    pub fn from_request(req: &HttpRequest) -> Self {
        Self {
            name: format!("{} {}", req.method, req.url),
            method: req.method.clone(),
            url: req.url.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
            updated_at: Utc::now(),
        }
    }
}

/// Reads all saved requests from a JSON file.
///
/// Returns an empty [`Vec`] if the file does not exist, cannot be read, or
/// cannot be parsed.
#[must_use]
pub fn load_saved_requests(path: &Path) -> Vec<SavedRequest> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    serde_json::from_str(&content).unwrap_or_default()
}

/// Rewrites the saved request file with the provided collection.
///
/// If the file does not yet exist it is created along with any missing parent
/// directories.
///
/// # Errors
/// Returns an error if the file cannot be opened or written to.
pub fn write_saved_requests(path: &Path, requests: &[SavedRequest]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(requests)?;
    let mut file = std::fs::File::create(path)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Inserts or replaces a saved request by name.
///
/// # Errors
/// Returns an error if rewriting the saved request file fails.
pub fn upsert_saved_request(
    path: &Path,
    requests: &mut Vec<SavedRequest>,
    request: SavedRequest,
) -> anyhow::Result<usize> {
    let idx = if let Some(idx) = requests
        .iter()
        .position(|saved| saved.name.eq_ignore_ascii_case(&request.name))
    {
        requests[idx] = request;
        idx
    } else {
        requests.push(request);
        requests.len() - 1
    };

    write_saved_requests(path, requests)?;
    Ok(idx)
}

/// Removes a saved request by index.
///
/// # Errors
/// Returns an error if rewriting the saved request file fails.
pub fn remove_saved_request(
    path: &Path,
    requests: &mut Vec<SavedRequest>,
    index: usize,
) -> anyhow::Result<Option<SavedRequest>> {
    if index >= requests.len() {
        return Ok(None);
    }

    let removed = requests.remove(index);
    write_saved_requests(path, requests)?;
    Ok(Some(removed))
}

#[cfg(test)]
mod tests {
    //! Tests for saved request persistence.

    use super::*;

    /// Builds a temporary saved request path for tests.
    fn saved_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("spark-saved-{name}-{}.json", std::process::id()))
    }

    /// Creates a saved request for tests.
    fn saved_request(name: &str, url: &str) -> SavedRequest {
        SavedRequest {
            name: name.to_string(),
            method: HttpMethod::Get,
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
            updated_at: Utc::now(),
        }
    }

    /// Saved requests round-trip through JSON storage.
    #[test]
    fn saved_requests_round_trip() {
        let path = saved_path("round-trip");
        let requests = vec![saved_request("Users", "https://example.com/users")];

        write_saved_requests(&path, &requests).expect("saved requests should write");
        let loaded = load_saved_requests(&path);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Users");
        assert_eq!(loaded[0].url, "https://example.com/users");
        let _ = std::fs::remove_file(path);
    }

    /// Upserting with the same name replaces the existing saved request.
    #[test]
    fn upsert_saved_request_replaces_existing_name() {
        let path = saved_path("upsert");
        let mut requests = vec![saved_request("Users", "https://example.com/users")];

        let idx = upsert_saved_request(
            &path,
            &mut requests,
            saved_request("users", "https://example.com/v2/users"),
        )
        .expect("saved request should upsert");

        assert_eq!(idx, 0);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://example.com/v2/users");
        let _ = std::fs::remove_file(path);
    }

    /// Removing an existing request rewrites the collection.
    #[test]
    fn remove_saved_request_deletes_by_index() {
        let path = saved_path("remove");
        let mut requests = vec![
            saved_request("Users", "https://example.com/users"),
            saved_request("Orders", "https://example.com/orders"),
        ];

        let removed =
            remove_saved_request(&path, &mut requests, 0).expect("saved request should remove");

        assert_eq!(removed.expect("request should be removed").name, "Users");
        assert_eq!(requests.len(), 1);
        assert_eq!(load_saved_requests(&path).len(), 1);
        let _ = std::fs::remove_file(path);
    }
}
