//! Spark core library — configuration, HTTP types, request execution, and request storage.

/// Application configuration loading.
pub mod config;
/// Environment storage and variable substitution.
pub mod environment;
/// Request history storage and retrieval in JSONL format.
pub mod history;
/// HTTP request/response types and curl-based execution.
pub mod http;
/// Saved request storage and retrieval in JSON format.
pub mod saved;
