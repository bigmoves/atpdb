use crate::app::AppState;
use crate::config::{matches_any_filter, IndexDirection, Mode};
use crate::crawler::start_crawler;
use crate::firehose::{Event, FirehoseClient, Operation};
use crate::query::{self, Query};
use crate::repos::{RepoState, RepoStatus};
use crate::storage::{Record, ScanDirection};
use crate::sync;
use crate::sync_worker::{start_sync_worker, SyncHandle};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::PrometheusHandle;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};

type AppStateHandle = (Arc<AppState>, Arc<SyncHandle>);

type MetricsState = (PrometheusHandle, Arc<AppState>);

async fn metrics_handler(State((handle, _app)): State<MetricsState>) -> String {
    // Gauges are updated by a background task every 30 seconds,
    // not here, to avoid lock contention during Prometheus scrapes.
    handle.render()
}

async fn http_metrics_middleware(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = normalize_path(req.uri().path());
    let start = Instant::now();

    let response = next.run(req).await;

    let status = response.status().as_u16().to_string();
    let duration = start.elapsed().as_secs_f64();

    counter!("http_requests_total", "method" => method.clone(), "path" => path.clone(), "status" => status).increment(1);
    histogram!("http_request_duration_seconds", "method" => method, "path" => path)
        .record(duration);

    response
}

/// Normalize paths to avoid label cardinality explosion
/// e.g., /repos/did:plc:abc123 -> /repos/{did}
fn normalize_path(path: &str) -> String {
    if path.starts_with("/repos/") && path.len() > 7 && !path.ends_with("/errors") {
        return "/repos/{did}".to_string();
    }
    path.to_string()
}

const DEFAULT_QUERY_LIMIT: usize = 100;
const MAX_QUERY_LIMIT: usize = 1000;

/// Cursor for sorted pagination - contains sort value and URI for keyset pagination
#[derive(Debug, Serialize, Deserialize)]
struct SortCursor {
    #[serde(rename = "s")]
    sort_value: String,
    #[serde(rename = "u")]
    uri: String,
}

fn encode_cursor(sort_value: &str, uri: &str) -> String {
    let cursor = SortCursor {
        sort_value: sort_value.to_string(),
        uri: uri.to_string(),
    };
    let json = serde_json::to_string(&cursor).unwrap();
    URL_SAFE_NO_PAD.encode(json.as_bytes())
}

fn decode_cursor(cursor: &str) -> Option<SortCursor> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    let json = std::str::from_utf8(&bytes).ok()?;
    serde_json::from_str(json).ok()
}

/// Extract field value as string for cursor
fn extract_field_value(value: &serde_json::Value, field_path: &str) -> Option<String> {
    let mut current = value;
    for part in field_path.split('.') {
        current = current.get(part)?;
    }
    current
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| current.as_i64().map(|n| n.to_string()))
}

/// Extract DID from an AT-URI string
fn extract_did_from_uri(uri: &str) -> Option<String> {
    // at://did:plc:xyz/collection/rkey -> did:plc:xyz
    uri.strip_prefix("at://")
        .and_then(|s| s.split('/').next())
        .map(|s| s.to_string())
}

/// Parse an AT URI that may contain query string params
/// e.g. "at://*/collection/*?search.trackName=value&sort=field:type:desc"
/// Returns (base_uri, params)
fn parse_query_uri(uri: &str) -> (String, HashMap<String, String>) {
    let mut params = HashMap::new();

    // Split on '?' to separate base URI from query string
    let (base, query_string) = match uri.split_once('?') {
        Some((base, qs)) => (base.to_string(), Some(qs)),
        None => (uri.to_string(), None),
    };

    // Parse query string params
    if let Some(qs) = query_string {
        for pair in qs.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                // URL decode the value
                let decoded = urlencoding::decode(value).unwrap_or_else(|_| value.into());
                params.insert(key.to_string(), decoded.into_owned());
            }
        }
    }

    (base, params)
}

/// Normalize a field path - adds "value." prefix unless it's a known top-level field
fn normalize_field_path(field: &str) -> String {
    const TOP_LEVEL_FIELDS: &[&str] = &["indexedAt", "uri", "cid", "did", "handle"];

    // Don't add prefix if it's a top-level field or already has value. prefix
    if TOP_LEVEL_FIELDS.contains(&field.split(':').next().unwrap_or(field))
        || field.starts_with("value.")
    {
        field.to_string()
    } else {
        format!("value.{}", field)
    }
}

/// Parse hydration pattern like "at://$.did/app.bsky.actor.profile/self"
/// Returns (collection, rkey) if pattern uses $.did
fn parse_hydration_pattern(pattern: &str) -> Option<(String, String)> {
    // Only support $.did patterns for v1
    let pattern = pattern.strip_prefix("at://$.did/")?;
    let parts: Vec<&str> = pattern.splitn(2, '/').collect();
    if parts.len() == 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

/// Transform a blob ref to include CDN URL
/// Blob refs look like: {"$type": "blob", "ref": {"$link": "bafkrei..."}, ...}
fn transform_blob(blob: &mut serde_json::Value, did: &str, preset: &str) {
    if let Some(cid) = blob
        .get("ref")
        .and_then(|r| r.get("$link"))
        .and_then(|l| l.as_str())
    {
        let url = format!(
            "https://cdn.bsky.app/img/{}/plain/{}/{}@jpeg",
            preset, did, cid
        );
        blob["url"] = serde_json::Value::String(url);
    }
}

/// Build a record JSON object with did and handle
fn build_record_json(
    uri: &str,
    cid: &str,
    value: serde_json::Value,
    indexed_at: u64,
    app: &AppState,
) -> serde_json::Value {
    let did = extract_did_from_uri(uri).unwrap_or_default();
    let handle = app.get_handle(&did);

    let mut record = serde_json::json!({
        "uri": uri,
        "did": did,
        "cid": cid,
        "value": value,
        "indexedAt": indexed_at
    });

    if let Some(h) = handle {
        record["handle"] = serde_json::Value::String(h);
    }

    record
}

/// Walk a JSON value at a dot-separated path and transform blob if found
fn transform_blob_at_path(value: &mut serde_json::Value, path: &str, did: &str, preset: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Last part - this should be the blob field
            if let Some(blob) = current.get_mut(*part) {
                if blob.get("$type").and_then(|t| t.as_str()) == Some("blob") {
                    transform_blob(blob, did, preset);
                }
            }
        } else {
            // Navigate deeper
            match current.get_mut(*part) {
                Some(next) => current = next,
                None => return,
            }
        }
    }
}

/// Hydrate records with related data
/// Supports two pattern types:
/// 1. Forward hydration: "at://$.did/collection/rkey" - fetch a specific record by DID
/// 2. Reverse hydration: "at://*/collection/*?field=$.uri" - fetch records that reference this record
fn hydrate_records(
    records: &mut [serde_json::Value],
    hydrate: &HashMap<String, String>,
    blobs: &HashMap<String, String>,
    app: &AppState,
) {
    let store = &app.store;

    // Separate forward and reverse hydration patterns
    let mut forward_patterns: Vec<(&str, &str, String, String)> = Vec::new();
    let mut reverse_patterns: Vec<(&str, ReverseLookup)> = Vec::new();

    for (key, pattern) in hydrate {
        if pattern.contains('*') && pattern.contains('?') {
            // Reverse lookup pattern
            if let Some(lookup) = parse_reverse_lookup(pattern) {
                reverse_patterns.push((key.as_str(), lookup));
            }
        } else if let Some((collection, rkey)) = parse_hydration_pattern(pattern) {
            // Forward hydration pattern
            forward_patterns.push((key.as_str(), pattern.as_str(), collection, rkey));
        }
    }

    // Handle forward hydration (existing logic)
    if !forward_patterns.is_empty() {
        // Collect all DIDs and their hydration targets using references
        let mut hydration_lookups: HashMap<String, Vec<(usize, &str)>> = HashMap::new();

        for (idx, record) in records.iter().enumerate() {
            if let Some(uri) = record.get("uri").and_then(|u| u.as_str()) {
                if let Some(did) = extract_did_from_uri(uri) {
                    for (key, _, collection, rkey) in &forward_patterns {
                        let target_uri = format!("at://{}/{}/{}", did, collection, rkey);
                        hydration_lookups
                            .entry(target_uri)
                            .or_default()
                            .push((idx, *key));
                    }
                }
            }
        }

        // Parallel lookup all hydration targets
        let hydrated_records: HashMap<String, serde_json::Value> = hydration_lookups
            .keys()
            .par_bridge()
            .filter_map(|target_uri| {
                store
                    .get_by_uri_string(target_uri)
                    .ok()
                    .flatten()
                    .map(|record| {
                        let mut record_json = build_record_json(
                            &record.uri,
                            &record.cid,
                            record.value,
                            record.indexed_at,
                            app,
                        );

                        // Transform blobs in hydrated record
                        let did = extract_did_from_uri(target_uri).unwrap_or_default();
                        for (path, preset) in blobs {
                            for (key, _, _, _) in &forward_patterns {
                                if let Some(sub_path) = path.strip_prefix(&format!("{}.", key)) {
                                    // Normalize: add value. prefix if not already present
                                    let normalized = if sub_path.starts_with("value.") {
                                        sub_path.to_string()
                                    } else {
                                        format!("value.{}", sub_path)
                                    };
                                    transform_blob_at_path(
                                        &mut record_json,
                                        &normalized,
                                        &did,
                                        preset,
                                    );
                                }
                            }
                        }

                        (target_uri.clone(), record_json)
                    })
            })
            .collect();

        // Attach hydrated records to main records
        for (target_uri, indices) in hydration_lookups {
            if let Some(hydrated) = hydrated_records.get(&target_uri) {
                for (idx, key) in indices {
                    if let Some(record) = records.get_mut(idx) {
                        record[key] = hydrated.clone();
                    }
                }
            }
        }
    }

    // Handle reverse hydration (parallel)
    if !reverse_patterns.is_empty() {
        // Collect all lookups: (record_index, key, collection, field, match_value, limit)
        let mut reverse_lookups: Vec<(usize, &str, String, String, String, usize)> = Vec::new();

        for (idx, record) in records.iter().enumerate() {
            for (key, lookup) in &reverse_patterns {
                let match_value = if lookup.match_field == "$.uri" {
                    record.get("uri").and_then(|v| v.as_str())
                } else {
                    let field_path = lookup.match_field.strip_prefix("$.").unwrap_or("");
                    get_nested_str(record, field_path)
                };

                if let Some(value) = match_value {
                    reverse_lookups.push((
                        idx,
                        *key,
                        lookup.collection.clone(),
                        lookup.field.clone(),
                        value.to_string(),
                        lookup.limit.unwrap_or(10),
                    ));
                }
            }
        }

        // Parallel lookup
        let results: Vec<(usize, &str, Vec<serde_json::Value>)> = reverse_lookups
            .par_iter()
            .filter_map(|(idx, key, collection, field, value, limit)| {
                store
                    .get_by_field_value(collection, field, value, *limit, None)
                    .ok()
                    .map(|related| {
                        let related_json: Vec<serde_json::Value> = related
                            .into_iter()
                            .map(|r| build_record_json(&r.uri, &r.cid, r.value, r.indexed_at, app))
                            .collect();
                        (*idx, *key, related_json)
                    })
            })
            .collect();

        // Merge results back
        for (idx, key, related_json) in results {
            if let Some(record) = records.get_mut(idx) {
                record[key] = serde_json::Value::Array(related_json);
            }
        }
    }
}

#[derive(Deserialize)]
pub struct QueryRequest {
    #[serde(default)]
    q: Option<String>, // Made optional - can use collection shorthand instead
    #[serde(default)]
    collection: Option<String>, // Shorthand: expands to at://*/collection/*
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    sort: Option<String>, // e.g. "indexedAt:desc", "indexedAt:asc"
    #[serde(default)]
    hydrate: HashMap<String, String>, // key -> "at://$.did/collection/rkey"
    #[serde(default)]
    blobs: HashMap<String, String>, // path -> preset (avatar, banner, feed_thumbnail)
    #[serde(default)]
    include_count: bool,
    #[serde(flatten)]
    search_fields: HashMap<String, serde_json::Value>, // Captures search.* fields
}

#[derive(Serialize)]
pub struct QueryResponse {
    records: Vec<serde_json::Value>,
    cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    error: String,
}

#[derive(Serialize)]
pub struct StatsResponse {
    records: usize,
    tracked_repos: usize,
    mode: String,
    cursors: Vec<CursorInfo>,
}

#[derive(Serialize)]
pub struct CursorInfo {
    key: String,
    cursor: String,
}

#[derive(Deserialize)]
pub struct SyncRequest {
    did: String,
}

#[derive(Serialize)]
pub struct SyncResponse {
    synced: usize,
}
// Repos types
#[derive(Deserialize)]
pub struct ReposAddRequest {
    dids: Vec<String>,
}

#[derive(Serialize)]
pub struct ReposAddResponse {
    queued: usize,
}

#[derive(Deserialize)]
pub struct ReposRemoveRequest {
    dids: Vec<String>,
}

#[derive(Serialize)]
pub struct ReposRemoveResponse {
    removed: usize,
}

#[derive(Serialize)]
pub struct ReposListResponse {
    repos: Vec<RepoStateJson>,
}

#[derive(Serialize)]
pub struct RepoStateJson {
    did: String,
    status: String,
    rev: Option<String>,
    record_count: u64,
    last_sync: Option<u64>,
    error: Option<String>,
    retry_count: u32,
}

impl From<RepoState> for RepoStateJson {
    fn from(state: RepoState) -> Self {
        RepoStateJson {
            did: state.did,
            status: state.status.to_string(),
            rev: state.rev,
            record_count: state.record_count,
            last_sync: state.last_sync,
            error: state.error,
            retry_count: state.retry_count,
        }
    }
}

#[derive(Deserialize)]
pub struct ReposResyncRequest {
    dids: Vec<String>,
}

#[derive(Serialize)]
pub struct ReposResyncResponse {
    queued: usize,
}

// Config types
#[derive(Serialize)]
pub struct ConfigResponse {
    mode: String,
    signal_collection: Option<String>,
    collections: Vec<String>,
    relay: String,
    sync_parallelism: u32,
    search_fields: Vec<String>,
}

#[derive(Deserialize)]
pub struct ConfigUpdateRequest {
    mode: Option<String>,
    signal_collection: Option<String>,
    collections: Option<Vec<String>>,
    search_fields: Option<Vec<String>>,
    sync_parallelism: Option<u32>,
}

async fn health(State((app, _)): State<AppStateHandle>) -> Result<&'static str, StatusCode> {
    // Verify database is accessible by doing a simple count
    app.store
        .count()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok("ok")
}

fn update_app_gauges(app: &AppState) {
    // Called by background task every 30 seconds and on /stats requests.
    // Not called during /metrics to avoid lock contention with queries.

    // Total collections and records (excludes index entries)
    if let Ok(collections) = app.store.unique_collections() {
        gauge!("storage_collections_total").set(collections.len() as f64);
        let total: usize = collections
            .iter()
            .filter_map(|c| app.store.count_collection(c).ok())
            .sum();
        gauge!("storage_records_total").set(total as f64);
    }

    // Repos by status
    if let Ok(repos) = app.repos.list() {
        let mut synced = 0;
        let mut pending = 0;
        let mut syncing = 0;
        let mut error = 0;
        let mut desync = 0;
        for repo in repos {
            match repo.status {
                RepoStatus::Synced => synced += 1,
                RepoStatus::Pending => pending += 1,
                RepoStatus::Syncing => syncing += 1,
                RepoStatus::Error => error += 1,
                RepoStatus::Desync => desync += 1,
            }
        }
        gauge!("storage_repos_total", "status" => "synced").set(synced as f64);
        gauge!("storage_repos_total", "status" => "pending").set(pending as f64);
        gauge!("storage_repos_total", "status" => "syncing").set(syncing as f64);
        gauge!("storage_repos_total", "status" => "error").set(error as f64);
        gauge!("storage_repos_total", "status" => "desync").set(desync as f64);
    }

    // Database disk space
    if let Ok(bytes) = app.disk_space() {
        gauge!("storage_disk_bytes").set(bytes as f64);
    }

    // Search index disk space
    if let Some(ref search) = app.search {
        gauge!("search_disk_bytes").set(search.disk_space() as f64);
    }
}

async fn stats(State((app, _)): State<AppStateHandle>) -> Result<Json<StatsResponse>, StatusCode> {
    update_app_gauges(&app);
    let records = app
        .store
        .count()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let tracked_repos = app.repos.list().map(|r| r.len()).unwrap_or(0);
    let config = app.config();
    let cursors = app
        .list_cursors()
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| CursorInfo { key: k, cursor: v })
        .collect();

    Ok(Json(StatsResponse {
        records,
        tracked_repos,
        mode: config.mode.to_string(),
        cursors,
    }))
}

/// Extract search.* fields from the flattened map
fn extract_search_fields(fields: &HashMap<String, serde_json::Value>) -> HashMap<String, String> {
    fields
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("search.").and_then(|field| {
                v.as_str()
                    .map(|query| (field.to_string(), query.to_string()))
            })
        })
        .collect()
}

/// Reverse lookup configuration parsed from a pattern
#[derive(Debug, Clone)]
struct ReverseLookup {
    collection: String,
    field: String,
    match_field: String, // e.g., "$.uri"
    limit: Option<usize>,
}

/// Parse a reverse lookup pattern like "at://*/app.bsky.feed.like/*?subject.uri=$.uri&limit=5"
fn parse_reverse_lookup(pattern: &str) -> Option<ReverseLookup> {
    // Must have wildcards and query params
    if !pattern.contains('*') || !pattern.contains('?') {
        return None;
    }

    // Parse: at://*/collection/*?field=$.value&limit=N
    let parts: Vec<&str> = pattern.splitn(2, '?').collect();
    if parts.len() != 2 {
        return None;
    }

    let uri_part = parts[0];
    let query_part = parts[1];

    // Extract collection from at://*/collection/*
    let uri_parts: Vec<&str> = uri_part.split('/').collect();
    if uri_parts.len() < 4 {
        return None;
    }
    let collection = uri_parts[3].trim_end_matches('*').trim_end_matches('/');
    if collection.is_empty() {
        return None;
    }

    // Parse query params
    let mut field = None;
    let mut match_field = None;
    let mut limit = None;

    for param in query_part.split('&') {
        let kv: Vec<&str> = param.splitn(2, '=').collect();
        if kv.len() == 2 {
            if kv[1].starts_with("$.") {
                field = Some(kv[0].to_string());
                match_field = Some(kv[1].to_string());
            } else if kv[0] == "limit" {
                limit = kv[1].parse().ok();
            }
        }
    }

    Some(ReverseLookup {
        collection: collection.to_string(),
        field: field?,
        match_field: match_field?,
        limit,
    })
}

/// Extract count.* fields from the flattened map
fn extract_count_fields(fields: &HashMap<String, serde_json::Value>) -> HashMap<String, String> {
    fields
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("count.").and_then(|field| {
                v.as_str()
                    .map(|pattern| (field.to_string(), pattern.to_string()))
            })
        })
        .collect()
}

/// Get a nested string value from a JSON object using dot notation
fn get_nested_str<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a str> {
    let mut current = value;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    current.as_str()
}

/// Execute count.* reverse lookups on records (parallel)
fn execute_count_lookups(
    records: &mut [serde_json::Value],
    count_fields: &HashMap<String, String>,
    app: &AppState,
) {
    if count_fields.is_empty() {
        return;
    }

    // Parse patterns once
    let parsed_patterns: Vec<(&str, ReverseLookup)> = count_fields
        .iter()
        .filter_map(|(key, pattern)| parse_reverse_lookup(pattern).map(|l| (key.as_str(), l)))
        .collect();

    if parsed_patterns.is_empty() {
        return;
    }

    // Collect all count lookups: (record_index, key, collection, field, match_value)
    let mut lookups: Vec<(usize, &str, &str, &str, String)> = Vec::new();

    for (idx, record) in records.iter().enumerate() {
        for (key, lookup) in &parsed_patterns {
            let match_value = if lookup.match_field == "$.uri" {
                record.get("uri").and_then(|v| v.as_str())
            } else {
                let field_path = lookup.match_field.strip_prefix("$.").unwrap_or("");
                get_nested_str(record, field_path)
            };

            if let Some(value) = match_value {
                lookups.push((
                    idx,
                    *key,
                    lookup.collection.as_str(),
                    lookup.field.as_str(),
                    value.to_string(),
                ));
            }
        }
    }

    // Execute lookups in parallel
    let results: Vec<(usize, &str, usize)> = lookups
        .par_iter()
        .map(|(idx, key, collection, field, value)| {
            let count = app
                .store
                .count_by_field_value(collection, field, value)
                .unwrap_or(0);
            (*idx, *key, count)
        })
        .collect();

    // Merge results back
    for (idx, key, count) in results {
        if let Some(record) = records.get_mut(idx) {
            record[key] = serde_json::Value::Number(count.into());
        }
    }
}

async fn query_handler(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Resolve query: use q if provided, otherwise expand collection shorthand
    let raw_query = match (&payload.q, &payload.collection) {
        (Some(q), _) => q.clone(),
        (None, Some(collection)) => format!("at://*/{}/*", collection),
        (None, None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Either 'q' or 'collection' is required".to_string(),
                }),
            ));
        }
    };

    // Parse query string params from the URI (e.g., at://...?search.field=value&sort=...)
    let (base_uri, uri_params) = parse_query_uri(&raw_query);

    let parsed = Query::parse(&base_uri).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    // Merge sort: JSON field takes precedence over URI param
    // Normalize field path (adds value. prefix unless it's a top-level field)
    let sort = payload
        .sort
        .clone()
        .or_else(|| uri_params.get("sort").cloned())
        .map(|s| {
            // Parse sort spec: field:type:direction
            let parts: Vec<&str> = s.split(':').collect();
            if parts.is_empty() {
                return s;
            }
            let normalized_field = normalize_field_path(parts[0]);
            let rest: Vec<&str> = parts.into_iter().skip(1).collect();
            if rest.is_empty() {
                normalized_field
            } else {
                format!("{}:{}", normalized_field, rest.join(":"))
            }
        });

    // Merge hydrate: JSON field takes precedence, then check URI params for hydrate.*
    let mut hydrate = payload.hydrate.clone();
    for (key, value) in &uri_params {
        if let Some(field) = key.strip_prefix("hydrate.") {
            hydrate
                .entry(field.to_string())
                .or_insert_with(|| value.clone());
        }
    }

    let limit = payload
        .limit
        .unwrap_or(DEFAULT_QUERY_LIMIT)
        .min(MAX_QUERY_LIMIT);
    let config = app.config();

    // Extract search fields from JSON body
    let mut search_fields = extract_search_fields(&payload.search_fields);

    // Merge search fields from URI params (JSON takes precedence)
    for (key, value) in &uri_params {
        if let Some(field) = key.strip_prefix("search.") {
            search_fields
                .entry(field.to_string())
                .or_insert_with(|| value.clone());
        }
    }

    // Extract count.* fields for reverse lookups
    let mut count_fields = extract_count_fields(&payload.search_fields);

    // Merge count fields from URI params
    for (key, value) in &uri_params {
        if let Some(field) = key.strip_prefix("count.") {
            count_fields
                .entry(field.to_string())
                .or_insert_with(|| value.clone());
        }
    }

    // If search fields present, use Tantivy search
    if !search_fields.is_empty() {
        return execute_search_query(
            &app,
            &parsed,
            &search_fields,
            limit,
            sort.as_deref(),
            &hydrate,
            &payload.blobs,
            &count_fields,
        )
        .await;
    }

    // Handle sorted queries using indexes
    if let Some(ref sort) = sort {
        let parts: Vec<&str> = sort.split(':').collect();
        let field = parts.first().unwrap_or(&"indexedAt");
        // field_type is declared in sort spec but not used for index lookup
        // (indexes are declared with types in config)
        let order = parts.get(2).unwrap_or(&"desc");
        let (direction, index_direction) = if *order == "asc" {
            (ScanDirection::Ascending, IndexDirection::Asc)
        } else {
            (ScanDirection::Descending, IndexDirection::Desc)
        };

        // Get collection from query
        let collection = match &parsed {
            Query::AllCollection { collection } => collection.as_str(),
            Query::Collection { collection, .. } => collection.as_str(),
            Query::Exact(_) | Query::AllForDid { .. } => {
                // Exact and AllForDid queries don't support sorting, use unsorted path
                return execute_unsorted_query(
                    &app,
                    &parsed,
                    limit,
                    payload.cursor.as_deref(),
                    &hydrate,
                    &payload.blobs,
                    payload.include_count,
                    &count_fields,
                )
                .await;
            }
        };

        // Decode cursor if present
        let cursor = payload.cursor.as_ref().and_then(|c| decode_cursor(c));

        // Check if using indexedAt or a configured index
        let records = if *field == "indexedAt" {
            app.store
                .scan_indexed_at(
                    collection,
                    cursor.as_ref().and_then(|c| c.sort_value.parse().ok()),
                    cursor.as_ref().map(|c| c.uri.as_str()),
                    limit + 1,
                    direction,
                )
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ErrorResponse {
                            error: e.to_string(),
                        }),
                    )
                })?
        } else {
            // Check if field is indexed for this direction
            let field_name = field.strip_prefix("value.").unwrap_or(field);
            if !config.is_field_indexed(collection, field_name, index_direction) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!(
                            "No {} index for '{}' on collection '{}'. Add ATPDB_INDEXES={}:{}:<type>:{} or use 'indexedAt'.",
                            order, field_name, collection, collection, field_name, order
                        ),
                    }),
                ));
            }

            app.store
                .scan_index(
                    collection,
                    field_name,
                    cursor.as_ref().map(|c| c.sort_value.as_str()),
                    cursor.as_ref().map(|c| c.uri.as_str()),
                    limit + 1,
                    direction,
                )
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ErrorResponse {
                            error: e.to_string(),
                        }),
                    )
                })?
        };

        let has_more = records.len() > limit;
        let records: Vec<_> = records.into_iter().take(limit).collect();

        let next_cursor = if has_more {
            records.last().map(|r| {
                let sort_value = if *field == "indexedAt" {
                    r.indexed_at.to_string()
                } else {
                    // Extract sort value from record
                    let field_name = field.strip_prefix("value.").unwrap_or(field);
                    extract_field_value(&r.value, field_name).unwrap_or_default()
                };
                encode_cursor(&sort_value, &r.uri)
            })
        } else {
            None
        };

        let mut values: Vec<serde_json::Value> = records
            .into_iter()
            .map(|r| build_record_json(&r.uri, &r.cid, r.value, r.indexed_at, &app))
            .collect();

        // Apply hydration if requested
        if !hydrate.is_empty() || !payload.blobs.is_empty() {
            hydrate_records(&mut values, &hydrate, &payload.blobs, &app);
        }

        // Execute count.* reverse lookups
        execute_count_lookups(&mut values, &count_fields, &app);

        // Get count if requested
        let total = if payload.include_count {
            app.store.count_collection(collection).ok()
        } else {
            None
        };

        return Ok(Json(QueryResponse {
            records: values,
            cursor: next_cursor,
            total,
        }));
    }

    // Unsorted query - use existing pagination
    execute_unsorted_query(
        &app,
        &parsed,
        limit,
        payload.cursor.as_deref(),
        &hydrate,
        &payload.blobs,
        payload.include_count,
        &count_fields,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_unsorted_query(
    app: &AppState,
    parsed: &Query,
    limit: usize,
    cursor: Option<&str>,
    hydrate: &HashMap<String, String>,
    blobs: &HashMap<String, String>,
    include_count: bool,
    count_fields: &HashMap<String, String>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let records = query::execute_paginated(parsed, &app.store, cursor, limit + 1).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    let has_more = records.len() > limit;
    let records: Vec<_> = records.into_iter().take(limit).collect();

    let next_cursor = if has_more {
        records.last().map(|r| r.uri.clone())
    } else {
        None
    };

    let mut values: Vec<serde_json::Value> = records
        .into_iter()
        .map(|r| build_record_json(&r.uri, &r.cid, r.value, r.indexed_at, app))
        .collect();

    // Apply hydration if requested
    if !hydrate.is_empty() || !blobs.is_empty() {
        hydrate_records(&mut values, hydrate, blobs, app);
    }

    // Execute count.* reverse lookups
    execute_count_lookups(&mut values, count_fields, app);

    // Get count if requested
    let total = if include_count {
        match parsed {
            Query::AllCollection { collection } => {
                app.store.count_collection(collection.as_str()).ok()
            }
            Query::Collection { collection, .. } => {
                app.store.count_collection(collection.as_str()).ok()
            }
            Query::Exact(_) | Query::AllForDid { .. } => Some(values.len()), // Count is just result count
        }
    } else {
        None
    };

    Ok(Json(QueryResponse {
        records: values,
        cursor: next_cursor,
        total,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn execute_search_query(
    app: &AppState,
    parsed: &Query,
    search_fields: &HashMap<String, String>,
    limit: usize,
    sort: Option<&str>,
    hydrate: &HashMap<String, String>,
    blobs: &HashMap<String, String>,
    count_fields: &HashMap<String, String>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let search = app.search.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Search is not enabled. Set ATPDB_SEARCH_FIELDS to enable search indexing."
                    .to_string(),
            }),
        )
    })?;

    let config = app.config();

    // Get collection from query
    let collection = match parsed {
        Query::AllCollection { collection } => collection.as_str(),
        Query::Collection { collection, .. } => collection.as_str(),
        Query::Exact(uri) => uri.collection.as_str(),
        Query::AllForDid { .. } => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "search not supported for DID-only queries".to_string(),
                }),
            ));
        }
    };

    // Overfetch when sorting to ensure enough records after re-ranking
    let fetch_limit = if sort.is_some() {
        (limit * 3).min(MAX_QUERY_LIMIT)
    } else {
        limit
    };

    // Search each field and collect URIs
    let mut uris: Vec<String> = Vec::new();
    for (field, query) in search_fields {
        // Verify field is configured
        if !config.is_field_searchable(collection, field) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!(
                        "Field '{}' is not searchable for collection '{}'. Configure ATPDB_SEARCH_FIELDS={}:{}",
                        field, collection, collection, field
                    ),
                }),
            ));
        }

        let results = search
            .search(collection, field, query, fetch_limit, 0)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: e.to_string(),
                    }),
                )
            })?;

        uris.extend(results);
    }

    // Deduplicate URIs while preserving relevance order
    let mut seen = HashSet::new();
    uris.retain(|uri| seen.insert(uri.clone()));
    if sort.is_none() {
        uris.truncate(limit);
    }

    // Hydrate records from Fjall
    let mut records: Vec<serde_json::Value> = Vec::with_capacity(uris.len());
    for uri in &uris {
        if let Ok(Some(record)) = app.store.get_by_uri_string(uri) {
            let mut record_json = build_record_json(
                &record.uri,
                &record.cid,
                record.value,
                record.indexed_at,
                app,
            );

            // Transform blobs on main record (single-segment paths only)
            // Multi-segment paths like "author.avatar" are handled by hydrate_records
            let did = extract_did_from_uri(uri).unwrap_or_default();
            for (path, preset) in blobs {
                // Only process single-segment paths here (no dots)
                // Paths with dots are hydration paths handled later
                if !path.contains('.') {
                    let normalized = format!("value.{}", path);
                    transform_blob_at_path(&mut record_json, &normalized, &did, preset);
                }
            }

            records.push(record_json);
        }
    }

    // Sort records if requested
    if let Some(sort_spec) = sort {
        let parts: Vec<&str> = sort_spec.split(':').collect();
        let field_path = parts.first().unwrap_or(&"indexedAt");
        let field_type = parts.get(1).unwrap_or(&"string");
        let order = parts.get(2).unwrap_or(&"desc");
        let descending = *order != "asc";

        records.sort_by(|a, b| {
            let val_a = get_json_field(a, field_path);
            let val_b = get_json_field(b, field_path);

            let cmp = match *field_type {
                "datetime" => {
                    let time_a = val_a
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                    let time_b = val_b
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                    time_a.cmp(&time_b)
                }
                "number" => {
                    let num_a = val_a.and_then(|v| v.as_f64());
                    let num_b = val_b.and_then(|v| v.as_f64());
                    num_a
                        .partial_cmp(&num_b)
                        .unwrap_or(std::cmp::Ordering::Equal)
                }
                _ => {
                    let str_a = val_a.and_then(|v| v.as_str()).unwrap_or("");
                    let str_b = val_b.and_then(|v| v.as_str()).unwrap_or("");
                    str_a.cmp(str_b)
                }
            };

            if descending {
                cmp.reverse()
            } else {
                cmp
            }
        });

        records.truncate(limit);
    }

    // Apply hydration if requested (also handles multi-segment blob paths)
    if !hydrate.is_empty() || !blobs.is_empty() {
        hydrate_records(&mut records, hydrate, blobs, app);
    }

    // Execute count.* reverse lookups
    execute_count_lookups(&mut records, count_fields, app);

    let total = records.len();

    Ok(Json(QueryResponse {
        records,
        // Note: Search results don't support cursor pagination.
        // Use limit/offset pattern or implement cursor support for search.
        cursor: None,
        total: Some(total),
    }))
}

async fn sync_handler(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, (StatusCode, Json<ErrorResponse>)> {
    let did = payload.did.clone();
    let store = Arc::new(app.store.clone());
    let config = app.config();
    let collections = config.collections.clone();
    let indexes = config.indexes.clone();
    let search = app.search.clone();
    let search_fields = config.search_fields.clone();
    let result = tokio::task::spawn_blocking(move || {
        sync::sync_repo(
            &payload.did,
            &store,
            &collections,
            &indexes,
            search.as_ref(),
            &search_fields,
        )
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    match result {
        Ok(sync_result) => {
            // Store handle if resolved
            if let Some(handle) = &sync_result.handle {
                let _ = app.set_handle(&did, handle);
            }
            Ok(Json(SyncResponse {
                synced: sync_result.record_count,
            }))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

/// Get a nested field from a JSON value using dot notation (e.g., "value.playedTime")
fn get_json_field<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

// Repos handlers
async fn repos_add(
    State((app, sync_handle)): State<AppStateHandle>,
    Json(payload): Json<ReposAddRequest>,
) -> Result<Json<ReposAddResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut queued = 0;

    for did in payload.dids {
        let state = RepoState::new(did.clone());
        app.repos.put(&state).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;

        sync_handle.queue(did).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;

        queued += 1;
    }

    Ok(Json(ReposAddResponse { queued }))
}

async fn repos_remove(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<ReposRemoveRequest>,
) -> Result<Json<ReposRemoveResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut removed = 0;

    for did in payload.dids {
        if app.repos.contains(&did).unwrap_or(false) {
            app.repos.delete(&did).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: e.to_string(),
                    }),
                )
            })?;
            removed += 1;
        }
    }

    Ok(Json(ReposRemoveResponse { removed }))
}

async fn repos_list(
    State((app, _)): State<AppStateHandle>,
) -> Result<Json<ReposListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let repos = app.repos.list().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    let repos: Vec<RepoStateJson> = repos.into_iter().map(Into::into).collect();
    Ok(Json(ReposListResponse { repos }))
}

async fn repos_get(
    State((app, _)): State<AppStateHandle>,
    axum::extract::Path(did): axum::extract::Path<String>,
) -> Result<Json<RepoStateJson>, (StatusCode, Json<ErrorResponse>)> {
    let state = app.repos.get(&did).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    match state {
        Some(s) => Ok(Json(s.into())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "repo not found".to_string(),
            }),
        )),
    }
}

async fn repos_errors(
    State((app, _)): State<AppStateHandle>,
) -> Result<Json<ReposListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let repos = app.repos.list_by_status(RepoStatus::Error).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    let repos: Vec<RepoStateJson> = repos.into_iter().map(Into::into).collect();
    Ok(Json(ReposListResponse { repos }))
}

async fn repos_resync(
    State((app, sync_handle)): State<AppStateHandle>,
    Json(payload): Json<ReposResyncRequest>,
) -> Result<Json<ReposResyncResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut queued = 0;

    for did in payload.dids {
        if let Ok(Some(mut state)) = app.repos.get(&did) {
            state.status = RepoStatus::Pending;
            state.error = None;
            app.repos.put(&state).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: e.to_string(),
                    }),
                )
            })?;

            sync_handle.queue(did).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: e.to_string(),
                    }),
                )
            })?;

            queued += 1;
        }
    }

    Ok(Json(ReposResyncResponse { queued }))
}

// Config handlers
async fn config_get(
    State((app, _)): State<AppStateHandle>,
) -> Result<Json<ConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
    let config = app.config();
    let search_fields: Vec<String> = config
        .search_fields
        .iter()
        .map(|sf| sf.to_config_string())
        .collect();
    Ok(Json(ConfigResponse {
        mode: config.mode.to_string(),
        signal_collection: config.signal_collection,
        collections: config.collections,
        relay: config.relay,
        sync_parallelism: config.sync_parallelism,
        search_fields,
    }))
}

async fn config_update(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<ConfigUpdateRequest>,
) -> Result<Json<ConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
    app.update_config(|config| {
        if let Some(mode_str) = &payload.mode {
            config.mode = match mode_str.as_str() {
                "manual" => Mode::Manual,
                "signal" => Mode::Signal,
                "full-network" => Mode::FullNetwork,
                _ => return,
            };
        }
        if let Some(signal) = payload.signal_collection.clone() {
            config.signal_collection = Some(signal);
        }
        if let Some(collections) = payload.collections.clone() {
            config.collections = collections;
        }
        if let Some(search_fields_strs) = payload.search_fields.clone() {
            config.search_fields = search_fields_strs
                .iter()
                .filter_map(|s| crate::config::SearchFieldConfig::parse(s))
                .collect();
        }
        if let Some(parallelism) = payload.sync_parallelism {
            config.sync_parallelism = parallelism;
        }
    })
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    let config = app.config();
    let search_fields: Vec<String> = config
        .search_fields
        .iter()
        .map(|sf| sf.to_config_string())
        .collect();
    Ok(Json(ConfigResponse {
        mode: config.mode.to_string(),
        signal_collection: config.signal_collection,
        collections: config.collections,
        relay: config.relay,
        sync_parallelism: config.sync_parallelism,
        search_fields,
    }))
}

fn start_firehose(
    relay: String,
    app: Arc<AppState>,
    sync_handle: Arc<SyncHandle>,
    running: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut reconnect_delay = Duration::from_secs(1);
        let max_delay = Duration::from_secs(60);

        while running.load(Ordering::Relaxed) {
            // Load cursor from previous session (allows 72hr recovery)
            let cursor = app.get_firehose_cursor(&relay);
            if let Some(seq) = cursor {
                println!("Resuming firehose from cursor: {}", seq);
            }

            match FirehoseClient::connect(&relay, cursor) {
                Ok(mut client) => {
                    gauge!("firehose_connected").set(1.0);
                    println!("Connected to firehose: {}", relay);
                    let mut event_count: u64 = 0;
                    let mut last_seq: i64 = cursor.unwrap_or(0);

                    while running.load(Ordering::Relaxed) {
                        match client.next_event() {
                            Ok(Some(Event::Commit {
                                seq,
                                did,
                                rev,
                                operations,
                            })) => {
                                last_seq = seq;
                                event_count += 1;
                                gauge!("firehose_last_seq").set(seq as f64);

                                // Save cursor every 1000 events
                                if event_count.is_multiple_of(1000) {
                                    let _ = app.set_firehose_cursor(&relay, seq);
                                }
                                let config = app.config();

                                // Check if we should process this repo based on mode
                                let should_process = match config.mode {
                                    Mode::Manual => {
                                        // Only process tracked repos
                                        app.repos.contains(did.as_str()).unwrap_or(false)
                                    }
                                    Mode::Signal => {
                                        // Check if this commit contains a signal collection record
                                        let has_signal = config
                                            .signal_collection
                                            .as_ref()
                                            .is_some_and(|signal| {
                                                operations.iter().any(|op| match op {
                                                    Operation::Create { uri, .. } => {
                                                        uri.collection.as_str() == signal
                                                    }
                                                    _ => false,
                                                })
                                            });

                                        if has_signal {
                                            // Auto-add repo if not tracked
                                            if !app.repos.contains(did.as_str()).unwrap_or(false) {
                                                let state = RepoState::new(did.to_string());
                                                let _ = app.repos.put(&state);
                                                let _ = sync_handle.queue_blocking(did.to_string());
                                            }
                                        }

                                        // Process if tracked
                                        app.repos.contains(did.as_str()).unwrap_or(false)
                                    }
                                    Mode::FullNetwork => {
                                        // Auto-sync new DIDs when we see them post to configured collections
                                        let has_matching_collection =
                                            operations.iter().any(|op| match op {
                                                Operation::Create { uri, .. }
                                                | Operation::Update { uri, .. } => {
                                                    matches_any_filter(
                                                        uri.collection.as_str(),
                                                        &config.collections,
                                                    )
                                                }
                                                Operation::Delete { uri } => matches_any_filter(
                                                    uri.collection.as_str(),
                                                    &config.collections,
                                                ),
                                            });

                                        if has_matching_collection
                                            && !app.repos.contains(did.as_str()).unwrap_or(false)
                                        {
                                            let state = RepoState::new(did.to_string());
                                            let _ = app.repos.put(&state);
                                            let _ = sync_handle.queue_blocking(did.to_string());
                                            eprintln!("Auto-syncing new DID: {}", did);
                                        }

                                        true
                                    }
                                };

                                if !should_process {
                                    continue;
                                }

                                // Update repo rev if tracked
                                if let Ok(Some(mut state)) = app.repos.get(did.as_str()) {
                                    state.rev = Some(rev.clone());
                                    let _ = app.repos.put(&state);
                                }

                                for op in operations {
                                    match op {
                                        Operation::Create { uri, cid, value } => {
                                            // Apply collection filter
                                            if !matches_any_filter(
                                                uri.collection.as_str(),
                                                &config.collections,
                                            ) {
                                                continue;
                                            }

                                            let record = Record {
                                                uri: uri.to_string(),
                                                cid,
                                                value,
                                                indexed_at: SystemTime::now()
                                                    .duration_since(UNIX_EPOCH)
                                                    .unwrap()
                                                    .as_secs(),
                                            };
                                            if let Err(e) = app.store.put_with_indexes(
                                                &uri,
                                                &record,
                                                &config.indexes,
                                            ) {
                                                eprintln!("Storage error: {}", e);
                                            }
                                            // Index for search
                                            if let Some(ref search) = app.search {
                                                if let Err(e) = search.index_record(
                                                    &uri.to_string(),
                                                    uri.collection.as_str(),
                                                    &record.value,
                                                    &config.search_fields,
                                                ) {
                                                    eprintln!("Search index error: {}", e);
                                                }
                                            }
                                        }
                                        Operation::Update { uri, cid, value } => {
                                            // Apply collection filter
                                            if !matches_any_filter(
                                                uri.collection.as_str(),
                                                &config.collections,
                                            ) {
                                                continue;
                                            }

                                            let record = Record {
                                                uri: uri.to_string(),
                                                cid,
                                                value,
                                                indexed_at: SystemTime::now()
                                                    .duration_since(UNIX_EPOCH)
                                                    .unwrap()
                                                    .as_secs(),
                                            };
                                            if let Err(e) = app.store.put_with_indexes(
                                                &uri,
                                                &record,
                                                &config.indexes,
                                            ) {
                                                eprintln!("Storage error: {}", e);
                                            }
                                            // Index for search
                                            if let Some(ref search) = app.search {
                                                if let Err(e) = search.index_record(
                                                    &uri.to_string(),
                                                    uri.collection.as_str(),
                                                    &record.value,
                                                    &config.search_fields,
                                                ) {
                                                    eprintln!("Search index error: {}", e);
                                                }
                                            }
                                        }
                                        Operation::Delete { uri } => {
                                            if !matches_any_filter(
                                                uri.collection.as_str(),
                                                &config.collections,
                                            ) {
                                                continue;
                                            }
                                            if let Err(e) =
                                                app.store.delete_with_indexes(&uri, &config.indexes)
                                            {
                                                eprintln!("Storage error: {}", e);
                                            }
                                            // Remove from search index
                                            if let Some(ref search) = app.search {
                                                if let Err(e) =
                                                    search.delete_record(&uri.to_string())
                                                {
                                                    eprintln!("Search delete error: {}", e);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(Some(Event::Identity { seq, did, handle })) => {
                                last_seq = seq;
                                if let Some(h) = handle {
                                    if let Err(e) = app.set_handle(did.as_str(), &h) {
                                        eprintln!("Handle update error: {}", e);
                                    }
                                }
                            }
                            Ok(Some(Event::Unknown { seq })) => {
                                if let Some(s) = seq {
                                    last_seq = s;
                                }
                            }
                            Ok(None) => {
                                // Timeout or connection closed - continue to check running flag
                                continue;
                            }
                            Err(e) => {
                                gauge!("firehose_connected").set(0.0);
                                counter!("firehose_errors_total", "error" => "connection")
                                    .increment(1);
                                eprintln!("Firehose error: {}", e);
                                // Save cursor before reconnecting
                                if last_seq > 0 {
                                    let _ = app.set_firehose_cursor(&relay, last_seq);
                                }
                                break; // Break inner loop to reconnect
                            }
                        }

                        // Reset reconnect delay on successful message processing
                        reconnect_delay = Duration::from_secs(1);
                    }

                    // Save final cursor on disconnect
                    if last_seq > 0 {
                        let _ = app.set_firehose_cursor(&relay, last_seq);
                        println!("Saved firehose cursor: {}", last_seq);
                    }
                }
                Err(e) => {
                    gauge!("firehose_connected").set(0.0);
                    counter!("firehose_errors_total", "error" => "connect").increment(1);
                    eprintln!("Failed to connect to firehose: {}", e);
                }
            }

            // Reconnect with exponential backoff
            if running.load(Ordering::Relaxed) {
                eprintln!("Reconnecting to firehose in {:?}...", reconnect_delay);
                std::thread::sleep(reconnect_delay);
                reconnect_delay = std::cmp::min(reconnect_delay * 2, max_delay);
            }
        }
    });
}

pub async fn run(app: Arc<AppState>, port: u16, relay: Option<String>) {
    // Initialize Prometheus metrics exporter with histogram buckets
    let prometheus_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .set_buckets(&[
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
        ])
        .expect("failed to set histogram buckets")
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    let running = Arc::new(AtomicBool::new(true));

    // Start sync worker
    let sync_handle = Arc::new(start_sync_worker(Arc::clone(&app)));

    // Start crawler
    start_crawler(
        Arc::clone(&app),
        Arc::clone(&sync_handle),
        Arc::clone(&running),
    );

    // Start firehose if relay specified
    if let Some(relay) = relay {
        start_firehose(
            relay,
            Arc::clone(&app),
            Arc::clone(&sync_handle),
            Arc::clone(&running),
        );
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app_for_shutdown = Arc::clone(&app);
    let app_for_gauges = Arc::clone(&app);
    let metrics_state: MetricsState = (prometheus_handle, Arc::clone(&app));
    let state: AppStateHandle = (app, sync_handle);

    // Background task to update gauges every 30 seconds (on blocking thread pool)
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            let app = Arc::clone(&app_for_gauges);
            let _ = tokio::task::spawn_blocking(move || update_app_gauges(&app)).await;
        }
    });
    let metrics_router = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics_state);

    let router = Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/query", post(query_handler))
        .route("/sync", post(sync_handler))
        .route("/repos", get(repos_list))
        .route("/repos/add", post(repos_add))
        .route("/repos/remove", post(repos_remove))
        .route("/repos/errors", get(repos_errors))
        .route("/repos/resync", post(repos_resync))
        .route("/repos/{did}", get(repos_get))
        .route("/config", get(config_get))
        .route("/config", post(config_update))
        .merge(metrics_router)
        .layer(middleware::from_fn(http_metrics_middleware))
        .layer(CompressionLayer::new())
        .layer(cors)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    println!("Starting server on http://localhost:{}", port);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    // Graceful shutdown on SIGTERM or SIGINT
    let shutdown_signal = async {
        let ctrl_c = async {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("Failed to install SIGTERM handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }

        println!("\nShutdown signal received, draining connections...");
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .unwrap();

    running.store(false, Ordering::Relaxed);

    // Flush database to ensure all writes are persisted
    if let Err(e) = app_for_shutdown.flush() {
        eprintln!("Warning: failed to flush database: {}", e);
    } else {
        println!("Database flushed");
    }

    println!("Server shutdown complete");
}
