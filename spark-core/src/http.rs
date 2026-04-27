//! HTTP method/request/response types and curl-based request execution.

use serde::{Deserialize, Serialize};
use std::fmt;

/// CRLF blank-line separator between HTTP headers and body.
const CRLF_SEP: &str = "\r\n\r\n";
/// LF blank-line separator between HTTP headers and body.
const LF_SEP: &str = "\n\n";

/// Supported HTTP methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    /// HTTP GET.
    Get,
    /// HTTP POST.
    Post,
    /// HTTP PUT.
    Put,
    /// HTTP PATCH.
    Patch,
    /// HTTP DELETE.
    Delete,
    /// HTTP HEAD.
    Head,
    /// HTTP OPTIONS.
    Options,
}

impl HttpMethod {
    /// Returns the method as an uppercase string literal.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }

    /// Returns all method variants in their canonical display order.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Get,
            Self::Post,
            Self::Put,
            Self::Patch,
            Self::Delete,
            Self::Head,
            Self::Options,
        ]
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An outgoing HTTP request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: HttpMethod,
    /// Target URL.
    pub url: String,
    /// Request headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Optional request body.
    pub body: Option<String>,
}

/// A parsed HTTP response returned by curl.
#[derive(Debug)]
pub struct HttpResponse {
    /// Numeric HTTP status code (e.g. 200).
    pub status_code: u16,
    /// HTTP reason phrase (e.g. `"OK"`).
    pub status_text: String,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Raw response body text.
    pub body: String,
    /// Round-trip time in milliseconds (from sending the request to receiving the full response).
    pub duration_ms: u128,
}

impl HttpRequest {
    /// Executes the request by invoking the system `curl` binary.
    ///
    /// Blocks until the response is received.
    ///
    /// # Errors
    /// Returns an error if `curl` cannot be spawned, exits with a non-zero code,
    /// or the response cannot be parsed.
    pub fn execute(&self) -> Result<HttpResponse, Box<dyn std::error::Error>> {
        let mut cmd = std::process::Command::new("curl");
        cmd.arg("-s")
            .arg("-i")
            .arg("-X")
            .arg(self.method.as_str())
            .arg(&self.url);

        for (key, value) in &self.headers {
            cmd.arg("-H").arg(format!("{key}: {value}"));
        }

        if let Some(body) = &self.body
            && !body.is_empty()
        {
            cmd.arg("-d").arg(body);
        }

        let start = std::time::Instant::now();
        let output = cmd.output()?;
        let duration_ms = start.elapsed().as_millis();

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("curl error: {stderr}").into());
        }

        parse_response(&stdout, duration_ms)
    }
}

/// Parses the raw output of `curl -i` into an [`HttpResponse`].
fn parse_response(
    raw: &str,
    duration_ms: u128,
) -> Result<HttpResponse, Box<dyn std::error::Error>> {
    let (sep_pos, sep_len) = if let Some(p) = raw.find(CRLF_SEP) {
        (p, CRLF_SEP.len())
    } else if let Some(p) = raw.find(LF_SEP) {
        (p, LF_SEP.len())
    } else {
        // No body — treat everything as headers
        (raw.len(), 0)
    };

    let header_section = &raw[..sep_pos];
    let body = if sep_pos < raw.len() {
        &raw[sep_pos + sep_len..]
    } else {
        ""
    };

    let mut lines = header_section.lines();
    let status_line = lines.next().unwrap_or("");
    let (status_code, status_text) = parse_status_line(status_line)?;

    let headers = lines
        .filter_map(|line| {
            let colon = line.find(':')?;
            let key = line[..colon].trim().to_string();
            let value = line[colon + 1..].trim().to_string();
            Some((key, value))
        })
        .collect();

    Ok(HttpResponse {
        status_code,
        status_text,
        headers,
        body: body.to_string(),
        duration_ms,
    })
}

/// Parses an HTTP status line (e.g. `HTTP/1.1 200 OK`) into a numeric code and reason phrase.
fn parse_status_line(line: &str) -> Result<(u16, String), Box<dyn std::error::Error>> {
    let mut parts = line.splitn(3, ' ');
    parts.next(); // skip "HTTP/x.x"
    let code_str = parts.next().unwrap_or("0");
    let text = parts.next().unwrap_or("").to_string();
    let code = code_str
        .parse::<u16>()
        .map_err(|_| format!("invalid status code: {code_str}"))?;
    Ok((code, text))
}
