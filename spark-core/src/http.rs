//! HTTP method/request/response types and curl-based request execution.

use serde::{Deserialize, Serialize};
use std::fmt::{self, Write as _};

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

/// One query string parameter attached to an outgoing request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryParam {
    /// Whether this parameter should be included in the outgoing URL.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Parameter name.
    pub key: String,
    /// Parameter value.
    pub value: String,
}

impl QueryParam {
    /// Creates an enabled query parameter.
    #[must_use]
    pub fn enabled(key: String, value: String) -> Self {
        Self {
            enabled: true,
            key,
            value,
        }
    }
}

/// Returns the default enabled state for query params.
const fn default_enabled() -> bool {
    true
}

/// An outgoing HTTP request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: HttpMethod,
    /// Target URL.
    pub url: String,
    /// Query string parameters appended to the target URL when enabled.
    pub query_params: Vec<QueryParam>,
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
            .arg(self.url_with_query_params());

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

    /// Returns the target URL with enabled query params appended.
    #[must_use]
    pub fn url_with_query_params(&self) -> String {
        append_query_params(&self.url, &self.query_params)
    }
}

/// Appends enabled query params to `url`, preserving existing query strings.
fn append_query_params(url: &str, params: &[QueryParam]) -> String {
    let mut pairs = params
        .iter()
        .filter(|param| param.enabled && !param.key.trim().is_empty())
        .peekable();

    if pairs.peek().is_none() {
        return url.to_string();
    }

    let mut output = url.to_string();
    let mut first = !url.contains('?');
    for param in pairs {
        output.push(if first { '?' } else { '&' });
        first = false;
        output.push_str(&percent_encode_component(param.key.trim()));
        output.push('=');
        output.push_str(&percent_encode_component(param.value.trim()));
    }
    output
}

/// Percent-encodes one query component.
fn percent_encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                encoded.push('%');
                let _ = write!(encoded, "{byte:02X}");
            }
        }
    }
    encoded
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

#[cfg(test)]
mod tests {
    //! Tests for HTTP request helpers.

    use super::*;

    /// Request URLs include enabled query parameters with encoding.
    #[test]
    fn url_with_query_params_appends_enabled_params() {
        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "https://example.com/search?existing=true".to_string(),
            query_params: vec![
                QueryParam::enabled("q".to_string(), "ada lovelace".to_string()),
                QueryParam {
                    enabled: false,
                    key: "archived".to_string(),
                    value: "true".to_string(),
                },
                QueryParam::enabled(String::new(), "skipped".to_string()),
            ],
            headers: Vec::new(),
            body: None,
        };

        assert_eq!(
            request.url_with_query_params(),
            "https://example.com/search?existing=true&q=ada%20lovelace"
        );
    }
}
