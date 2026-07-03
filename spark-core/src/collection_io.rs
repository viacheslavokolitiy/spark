//! Import and export for saved request collections.

use crate::{
    http::{ApiKeyLocation, HttpMethod, QueryParam, RequestAuth, RequestScripts},
    saved::{DEFAULT_COLLECTION, SavedRequest, write_saved_requests},
};
use anyhow::{Context, anyhow};
use chrono::Utc;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, io::Write, path::Path};

/// Supported external collection document formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionFormat {
    /// Postman Collection v2.1 JSON.
    Postman,
    /// `OpenAPI` 3.x JSON or YAML.
    OpenApi,
}

/// Reads saved requests from `path`, detecting the format when `format` is `None`.
///
/// # Errors
/// Returns an error when the file cannot be read, parsed, or recognized.
pub fn import_collection(
    path: &Path,
    format: Option<CollectionFormat>,
) -> anyhow::Result<Vec<SavedRequest>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read collection file {}", path.display()))?;
    import_collection_str(&content, format)
}

/// Parses saved requests from an external collection document.
///
/// # Errors
/// Returns an error when the content cannot be parsed or recognized.
pub fn import_collection_str(
    content: &str,
    format: Option<CollectionFormat>,
) -> anyhow::Result<Vec<SavedRequest>> {
    let value = parse_document(content)?;
    let detected = format.or_else(|| detect_format(&value)).ok_or_else(|| {
        anyhow!("unrecognized collection format; choose Postman or OpenAPI explicitly")
    })?;

    match detected {
        CollectionFormat::Postman => import_postman(&value),
        CollectionFormat::OpenApi => import_openapi(&value),
    }
}

/// Writes `requests` to `path` in the selected external format.
///
/// # Errors
/// Returns an error when the output file cannot be written.
pub fn export_collection(
    path: &Path,
    requests: &[SavedRequest],
    format: CollectionFormat,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let value = match format {
        CollectionFormat::Postman => export_postman(requests),
        CollectionFormat::OpenApi => export_openapi(requests),
    };
    let json = serde_json::to_string_pretty(&value)?;
    let mut file = std::fs::File::create(path)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Imports `path` and merges the requests into the local saved request store.
///
/// # Errors
/// Returns an error when importing or rewriting the local store fails.
pub fn import_into_saved_requests(
    path: &Path,
    saved_path: &Path,
    requests: &mut Vec<SavedRequest>,
    format: Option<CollectionFormat>,
) -> anyhow::Result<usize> {
    let imported = import_collection(path, format)?;
    let imported_count = imported.len();
    for request in imported {
        if let Some(idx) = requests
            .iter()
            .position(|saved| same_saved_location(saved, &request))
        {
            requests[idx] = request;
        } else {
            requests.push(request);
        }
    }
    write_saved_requests(saved_path, requests)?;
    Ok(imported_count)
}

/// Returns whether two saved requests occupy the same external location.
fn same_saved_location(left: &SavedRequest, right: &SavedRequest) -> bool {
    left.name.eq_ignore_ascii_case(&right.name)
        && left.collection.eq_ignore_ascii_case(&right.collection)
        && match (left.folder.as_deref(), right.folder.as_deref()) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            (None, None) => true,
            _ => false,
        }
}

/// Parses JSON first, then YAML into a JSON value.
fn parse_document(content: &str) -> anyhow::Result<Value> {
    serde_json::from_str(content)
        .or_else(|_| serde_yaml::from_str(content))
        .context("failed to parse collection as JSON or YAML")
}

/// Detects the external collection format from top-level fields.
fn detect_format(value: &Value) -> Option<CollectionFormat> {
    if value.get("openapi").is_some() || value.get("swagger").is_some() {
        return Some(CollectionFormat::OpenApi);
    }
    if value.get("item").is_some() && value.get("info").is_some() {
        return Some(CollectionFormat::Postman);
    }
    None
}

/// Imports requests from a Postman collection value.
fn import_postman(value: &Value) -> anyhow::Result<Vec<SavedRequest>> {
    let collection = value
        .pointer("/info/name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(DEFAULT_COLLECTION);
    let items = value
        .get("item")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Postman collection is missing item array"))?;
    let mut requests = Vec::new();
    collect_postman_items(items, collection, None, &mut requests);
    Ok(requests)
}

/// Recursively collects Postman request items.
fn collect_postman_items(
    items: &[Value],
    collection: &str,
    folder: Option<&str>,
    requests: &mut Vec<SavedRequest>,
) {
    for item in items {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Imported Request");
        if let Some(children) = item.get("item").and_then(Value::as_array) {
            let child_folder =
                folder.map_or_else(|| name.to_string(), |parent| format!("{parent}/{name}"));
            collect_postman_items(children, collection, Some(&child_folder), requests);
            continue;
        }

        let Some(request_value) = item.get("request") else {
            continue;
        };
        let method = request_value
            .get("method")
            .and_then(Value::as_str)
            .and_then(parse_method)
            .unwrap_or(HttpMethod::Get);
        let url = postman_url(request_value.get("url"));
        if url.trim().is_empty() {
            continue;
        }
        let headers = request_value
            .get("header")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |headers| {
                headers
                    .iter()
                    .filter_map(postman_header)
                    .collect::<Vec<_>>()
            });
        let query_params = postman_query_params(request_value.get("url"));
        let auth = postman_auth(request_value.get("auth"));
        let scripts = postman_scripts(item.get("event"));
        let body = request_value
            .pointer("/body/raw")
            .and_then(Value::as_str)
            .filter(|body| !body.trim().is_empty())
            .map(ToString::to_string);

        requests.push(SavedRequest {
            name: name.to_string(),
            collection: collection.to_string(),
            folder: folder.map(ToString::to_string),
            method,
            url,
            query_params,
            auth,
            headers,
            body,
            scripts,
            updated_at: Utc::now(),
        });
    }
}

/// Extracts pre-request and test scripts from Postman item events.
fn postman_scripts(value: Option<&Value>) -> RequestScripts {
    let mut scripts = RequestScripts::default();
    let Some(events) = value.and_then(Value::as_array) else {
        return scripts;
    };

    for event in events {
        let Some(listen) = event.get("listen").and_then(Value::as_str) else {
            continue;
        };
        let Some(script) = postman_event_script(event) else {
            continue;
        };
        match listen {
            "prerequest" => scripts.pre_request = script,
            "test" => scripts.tests = script,
            _ => {}
        }
    }
    scripts
}

/// Extracts a Postman event script body.
fn postman_event_script(event: &Value) -> Option<String> {
    let exec = event.pointer("/script/exec")?;
    match exec {
        Value::String(script) => Some(script.clone()),
        Value::Array(lines) => Some(
            lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// Extracts a supported auth helper from a Postman auth object.
fn postman_auth(value: Option<&Value>) -> RequestAuth {
    let Some(auth) = value.and_then(Value::as_object) else {
        return RequestAuth::None;
    };
    match auth.get("type").and_then(Value::as_str) {
        Some("bearer") => postman_auth_attr(auth, "bearer", "token")
            .map_or(RequestAuth::None, |token| RequestAuth::Bearer { token }),
        Some("basic") => {
            let username = postman_auth_attr(auth, "basic", "username").unwrap_or_default();
            let password = postman_auth_attr(auth, "basic", "password").unwrap_or_default();
            if username.is_empty() {
                RequestAuth::None
            } else {
                RequestAuth::Basic { username, password }
            }
        }
        Some("apikey") => {
            let key = postman_auth_attr(auth, "apikey", "key").unwrap_or_default();
            let value = postman_auth_attr(auth, "apikey", "value").unwrap_or_default();
            let location = match postman_auth_attr(auth, "apikey", "in").as_deref() {
                Some("query") => ApiKeyLocation::Query,
                _ => ApiKeyLocation::Header,
            };
            if key.is_empty() {
                RequestAuth::None
            } else {
                RequestAuth::ApiKey {
                    key,
                    value,
                    location,
                }
            }
        }
        _ => RequestAuth::None,
    }
}

/// Reads one keyed Postman auth attribute from an auth array.
fn postman_auth_attr(auth: &Map<String, Value>, section: &str, key: &str) -> Option<String> {
    auth.get(section)
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| entry.get("key").and_then(Value::as_str) == Some(key))
        .and_then(|entry| entry.get("value"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// Parses a Postman header entry.
fn postman_header(value: &Value) -> Option<(String, String)> {
    let key = value.get("key")?.as_str()?.trim();
    let header_value = value.get("value")?.as_str()?.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), header_value.to_string()))
}

/// Extracts query parameters from a Postman request URL value.
fn postman_query_params(value: Option<&Value>) -> Vec<QueryParam> {
    let Some(query) = value
        .and_then(Value::as_object)
        .and_then(|map| map.get("query"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    query.iter().filter_map(postman_query_param).collect()
}

/// Parses a Postman query parameter entry.
fn postman_query_param(value: &Value) -> Option<QueryParam> {
    let key = value.get("key")?.as_str()?.trim();
    if key.is_empty() {
        return None;
    }

    let query_value = value.get("value").and_then(Value::as_str).unwrap_or("");
    Some(QueryParam {
        enabled: !value
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        key: key.to_string(),
        value: query_value.to_string(),
    })
}

/// Extracts a URL from a Postman request URL value.
fn postman_url(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(url)) => url.clone(),
        Some(Value::Object(map)) => map
            .get("raw")
            .and_then(Value::as_str)
            .map_or_else(String::new, ToString::to_string),
        _ => String::new(),
    }
}

/// Imports requests from an `OpenAPI` document.
fn import_openapi(value: &Value) -> anyhow::Result<Vec<SavedRequest>> {
    let collection = value
        .pointer("/info/title")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(DEFAULT_COLLECTION);
    let base_url = value
        .pointer("/servers/0/url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let paths = value
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("OpenAPI document is missing paths object"))?;
    let mut requests = Vec::new();

    for (path, path_value) in paths {
        let Some(operations) = path_value.as_object() else {
            continue;
        };
        for (method_name, operation) in operations {
            let Some(method) = parse_method(method_name) else {
                continue;
            };
            let name = operation
                .get("summary")
                .and_then(Value::as_str)
                .or_else(|| operation.get("operationId").and_then(Value::as_str))
                .map_or_else(
                    || format!("{} {path}", method.as_str()),
                    ToString::to_string,
                );
            let folder = operation
                .get("tags")
                .and_then(Value::as_array)
                .and_then(|tags| tags.first())
                .and_then(Value::as_str)
                .filter(|tag| !tag.trim().is_empty())
                .map(ToString::to_string);
            let headers = openapi_header_parameters(operation);
            let query_params = openapi_query_parameters(operation);
            let body = openapi_request_body(operation);

            requests.push(SavedRequest {
                name,
                collection: collection.to_string(),
                folder,
                method,
                url: format!("{base_url}{path}"),
                query_params,
                auth: RequestAuth::None,
                headers,
                body,
                scripts: RequestScripts::default(),
                updated_at: Utc::now(),
            });
        }
    }

    Ok(requests)
}

/// Extracts query parameters from an `OpenAPI` operation.
fn openapi_query_parameters(operation: &Value) -> Vec<QueryParam> {
    operation
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|parameter| parameter.get("in").and_then(Value::as_str) == Some("query"))
        .filter_map(|parameter| {
            let name = parameter.get("name")?.as_str()?;
            Some(QueryParam::enabled(name.to_string(), String::new()))
        })
        .collect()
}

/// Extracts header parameters from an `OpenAPI` operation.
fn openapi_header_parameters(operation: &Value) -> Vec<(String, String)> {
    operation
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|parameter| parameter.get("in").and_then(Value::as_str) == Some("header"))
        .filter_map(|parameter| {
            let name = parameter.get("name")?.as_str()?;
            Some((name.to_string(), String::new()))
        })
        .collect()
}

/// Extracts an example request body from an `OpenAPI` operation.
fn openapi_request_body(operation: &Value) -> Option<String> {
    let content = operation.pointer("/requestBody/content")?.as_object()?;
    let media = content.values().next()?;
    if let Some(example) = media.get("example") {
        return Some(example_to_body(example));
    }
    media
        .pointer("/examples/default/value")
        .map(example_to_body)
}

/// Converts an `OpenAPI` example value into a request body string.
fn example_to_body(value: &Value) -> String {
    value.as_str().map_or_else(
        || serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        ToString::to_string,
    )
}

/// Exports requests as a Postman Collection v2.1 value.
fn export_postman(requests: &[SavedRequest]) -> Value {
    let mut collections: BTreeMap<&str, BTreeMap<Option<&str>, Vec<&SavedRequest>>> =
        BTreeMap::new();
    for request in requests {
        collections
            .entry(&request.collection)
            .or_default()
            .entry(request.folder.as_deref())
            .or_default()
            .push(request);
    }

    let items = collections
        .into_iter()
        .map(|(collection, folders)| {
            let mut collection_items = Vec::new();
            for (folder, folder_requests) in folders {
                let request_items = folder_requests
                    .into_iter()
                    .map(postman_request_item)
                    .collect::<Vec<_>>();
                if let Some(folder) = folder {
                    collection_items.push(json!({
                        "name": folder,
                        "item": request_items,
                    }));
                } else {
                    collection_items.extend(request_items);
                }
            }
            json!({
                "name": collection,
                "item": collection_items,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "info": {
            "name": "Spark Collections",
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
        },
        "item": items
    })
}

/// Builds one Postman request item.
fn postman_request_item(request: &SavedRequest) -> Value {
    let headers = request
        .headers
        .iter()
        .map(|(key, value)| json!({ "key": key, "value": value }))
        .collect::<Vec<_>>();
    let mut request_value = json!({
        "method": request.method.as_str(),
        "header": headers,
        "url": {
            "raw": request.url,
        }
    });
    if let Some(body) = &request.body
        && !body.trim().is_empty()
    {
        request_value["body"] = json!({
            "mode": "raw",
            "raw": body,
        });
    }
    if !request.query_params.is_empty() {
        request_value["url"]["query"] = json!(
            request
                .query_params
                .iter()
                .map(|param| {
                    json!({
                        "key": param.key,
                        "value": param.value,
                        "disabled": !param.enabled,
                    })
                })
                .collect::<Vec<_>>()
        );
    }
    if let Some(auth) = postman_auth_value(&request.auth) {
        request_value["auth"] = auth;
    }

    let mut item = json!({
        "name": request.name,
        "request": request_value,
    });
    let events = postman_event_values(&request.scripts);
    if !events.is_empty() {
        item["event"] = Value::Array(events);
    }
    item
}

/// Converts Spark scripts into Postman event values.
fn postman_event_values(scripts: &RequestScripts) -> Vec<Value> {
    let mut events = Vec::new();
    if !scripts.pre_request.trim().is_empty() {
        events.push(postman_event_value("prerequest", &scripts.pre_request));
    }
    if !scripts.tests.trim().is_empty() {
        events.push(postman_event_value("test", &scripts.tests));
    }
    events
}

/// Builds one Postman event value.
fn postman_event_value(listen: &str, script: &str) -> Value {
    json!({
        "listen": listen,
        "script": {
            "type": "text/javascript",
            "exec": script.lines().collect::<Vec<_>>(),
        }
    })
}

/// Converts a Spark auth helper into a Postman auth object.
fn postman_auth_value(auth: &RequestAuth) -> Option<Value> {
    match auth {
        RequestAuth::None => None,
        RequestAuth::Bearer { token } => Some(json!({
            "type": "bearer",
            "bearer": [{"key": "token", "value": token, "type": "string"}],
        })),
        RequestAuth::Basic { username, password } => Some(json!({
            "type": "basic",
            "basic": [
                {"key": "username", "value": username, "type": "string"},
                {"key": "password", "value": password, "type": "string"}
            ],
        })),
        RequestAuth::ApiKey {
            key,
            value,
            location,
        } => Some(json!({
            "type": "apikey",
            "apikey": [
                {"key": "key", "value": key, "type": "string"},
                {"key": "value", "value": value, "type": "string"},
                {
                    "key": "in",
                    "value": match location {
                        ApiKeyLocation::Header => "header",
                        ApiKeyLocation::Query => "query",
                    },
                    "type": "string"
                }
            ],
        })),
    }
}

/// Exports requests as an `OpenAPI` 3.1 value.
fn export_openapi(requests: &[SavedRequest]) -> Value {
    let mut paths = Map::new();
    for request in requests {
        let path = openapi_path(&request.url);
        let method = request.method.as_str().to_lowercase();
        let operation = openapi_operation(request);
        paths
            .entry(path)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("inserted path value should be an object")
            .insert(method, operation);
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Spark Collections",
            "version": "1.0.0"
        },
        "paths": paths
    })
}

/// Builds one `OpenAPI` operation.
fn openapi_operation(request: &SavedRequest) -> Value {
    let mut operation = json!({
        "summary": request.name,
        "responses": {
            "default": {
                "description": "Imported from Spark"
            }
        }
    });
    if let Some(folder) = &request.folder {
        operation["tags"] = json!([folder]);
    } else if request.collection != DEFAULT_COLLECTION {
        operation["tags"] = json!([request.collection]);
    }
    if !request.headers.is_empty() {
        operation["parameters"] = json!(
            request
                .headers
                .iter()
                .map(|(key, _)| {
                    json!({
                        "name": key,
                        "in": "header",
                        "required": false,
                        "schema": { "type": "string" }
                    })
                })
                .collect::<Vec<_>>()
        );
    }
    if !request.query_params.is_empty() {
        if operation.get("parameters").is_none() {
            operation["parameters"] = json!([]);
        }
        let parameters = operation
            .get_mut("parameters")
            .and_then(Value::as_array_mut)
            .expect("parameters should be an array after initialization");
        parameters.extend(request.query_params.iter().map(|param| {
            json!({
                "name": param.key,
                "in": "query",
                "required": false,
                "schema": { "type": "string" }
            })
        }));
    }
    if let Some(body) = &request.body
        && !body.trim().is_empty()
    {
        operation["requestBody"] = json!({
            "content": {
                "application/json": {
                    "example": parse_json_or_string(body)
                }
            }
        });
    }
    operation
}

/// Parses a request body as JSON when possible, otherwise keeps it as a string.
fn parse_json_or_string(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
}

/// Converts an absolute URL or path-like URL into an `OpenAPI` path key.
fn openapi_path(url: &str) -> String {
    if url.starts_with('/') {
        return url.to_string();
    }
    if let Some(scheme_pos) = url.find("://") {
        let host_start = scheme_pos + "://".len();
        if let Some(path_start) = url[host_start..].find('/') {
            return url[host_start + path_start..].to_string();
        }
    }
    format!("/{url}")
}

/// Parses a string into an HTTP method.
fn parse_method(value: &str) -> Option<HttpMethod> {
    match value.to_ascii_uppercase().as_str() {
        "GET" => Some(HttpMethod::Get),
        "POST" => Some(HttpMethod::Post),
        "PUT" => Some(HttpMethod::Put),
        "PATCH" => Some(HttpMethod::Patch),
        "DELETE" => Some(HttpMethod::Delete),
        "HEAD" => Some(HttpMethod::Head),
        "OPTIONS" => Some(HttpMethod::Options),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! Tests for external collection import and export.

    use super::*;

    /// Builds a saved request for import/export tests.
    fn saved_request(name: &str, method: HttpMethod, url: &str) -> SavedRequest {
        SavedRequest {
            name: name.to_string(),
            collection: "Identity".to_string(),
            folder: Some("Users".to_string()),
            method,
            url: url.to_string(),
            query_params: Vec::new(),
            auth: RequestAuth::None,
            headers: vec![("Authorization".to_string(), "Bearer token".to_string())],
            body: Some("{\"name\":\"Ada\"}".to_string()),
            scripts: RequestScripts::default(),
            updated_at: Utc::now(),
        }
    }

    /// Imports a Postman collection with a nested folder.
    #[test]
    fn imports_postman_collection() {
        let content = r#"{
          "info": {"name": "Identity"},
          "item": [{
            "name": "Users",
            "item": [{
              "name": "Create user",
              "event": [
                {
                  "listen": "prerequest",
                  "script": {"exec": ["set trace={{trace_id}}", "header X-Trace: {{trace}}"]}
                },
                {
                  "listen": "test",
                  "script": {"exec": ["status 201", "body contains Ada"]}
                }
              ],
              "request": {
                "method": "POST",
                "header": [{"key":"Content-Type","value":"application/json"}],
                "auth": {
                  "type": "bearer",
                  "bearer": [{"key":"token","value":"{{token}}","type":"string"}]
                },
                "url": {
                  "raw":"https://api.example.com/users",
                  "query": [
                    {"key":"active","value":"true"},
                    {"key":"archived","value":"false","disabled":true}
                  ]
                },
                "body": {"mode":"raw","raw":"{\"name\":\"Ada\"}"}
              }
            }]
          }]
        }"#;

        let imported = import_collection_str(content, None).expect("Postman should import");

        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].collection, "Identity");
        assert_eq!(imported[0].folder.as_deref(), Some("Users"));
        assert_eq!(imported[0].method, HttpMethod::Post);
        assert_eq!(imported[0].headers[0].0, "Content-Type");
        assert_eq!(
            imported[0].query_params,
            vec![
                QueryParam::enabled("active".to_string(), "true".to_string()),
                QueryParam {
                    enabled: false,
                    key: "archived".to_string(),
                    value: "false".to_string(),
                },
            ]
        );
        assert_eq!(
            imported[0].auth,
            RequestAuth::Bearer {
                token: "{{token}}".to_string(),
            }
        );
        assert_eq!(
            imported[0].scripts.pre_request,
            "set trace={{trace_id}}\nheader X-Trace: {{trace}}"
        );
        assert_eq!(imported[0].scripts.tests, "status 201\nbody contains Ada");
    }

    /// Imports an `OpenAPI` operation with a server URL and tag.
    #[test]
    fn imports_openapi_document() {
        let content = r#"{
          "openapi": "3.0.3",
          "info": {"title": "Commerce"},
          "servers": [{"url": "https://api.example.com"}],
          "paths": {
            "/orders": {
              "post": {
                "summary": "Create order",
                "tags": ["Orders"],
                "parameters": [{"name":"X-Trace","in":"header"}],
                "requestBody": {
                  "content": {
                    "application/json": {
                      "example": {"sku": "ABC"}
                    }
                  }
                }
              }
            }
          }
        }"#;

        let imported = import_collection_str(content, None).expect("OpenAPI should import");

        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].collection, "Commerce");
        assert_eq!(imported[0].folder.as_deref(), Some("Orders"));
        assert_eq!(imported[0].url, "https://api.example.com/orders");
        assert_eq!(imported[0].headers[0].0, "X-Trace");
        assert_eq!(
            imported[0].body.as_deref(),
            Some("{\n  \"sku\": \"ABC\"\n}")
        );
    }

    /// Exports requests as Postman collection items.
    #[test]
    fn exports_postman_collection() {
        let mut request = saved_request(
            "Create user",
            HttpMethod::Post,
            "https://api.example.com/users",
        );
        request.query_params = vec![QueryParam::enabled(
            "active".to_string(),
            "true".to_string(),
        )];
        request.auth = RequestAuth::Basic {
            username: "ada".to_string(),
            password: "secret".to_string(),
        };
        request.scripts = RequestScripts {
            pre_request: "set trace=abc123".to_string(),
            tests: "status 2xx".to_string(),
        };
        let exported = export_postman(&[request]);

        assert_eq!(
            exported.pointer("/item/0/name").and_then(Value::as_str),
            Some("Identity")
        );
        assert_eq!(
            exported
                .pointer("/item/0/item/0/item/0/request/method")
                .and_then(Value::as_str),
            Some("POST")
        );
        assert_eq!(
            exported
                .pointer("/item/0/item/0/item/0/request/url/query/0/key")
                .and_then(Value::as_str),
            Some("active")
        );
        assert_eq!(
            exported
                .pointer("/item/0/item/0/item/0/request/auth/type")
                .and_then(Value::as_str),
            Some("basic")
        );
        assert_eq!(
            exported
                .pointer("/item/0/item/0/item/0/event/0/listen")
                .and_then(Value::as_str),
            Some("prerequest")
        );
        assert_eq!(
            exported
                .pointer("/item/0/item/0/item/0/event/1/script/exec/0")
                .and_then(Value::as_str),
            Some("status 2xx")
        );
    }

    /// Exports requests as `OpenAPI` paths.
    #[test]
    fn exports_openapi_document() {
        let exported = export_openapi(&[saved_request(
            "Create user",
            HttpMethod::Post,
            "https://api.example.com/users",
        )]);

        assert_eq!(
            exported.pointer("/openapi").and_then(Value::as_str),
            Some("3.1.0")
        );
        assert!(exported.pointer("/paths/~1users/post").is_some());
    }
}
