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
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};

type AppStateHandle = (Arc<AppState>, Arc<SyncHandle>);

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
fn hydrate_records(
    records: &mut [serde_json::Value],
    hydrate: &std::collections::HashMap<String, String>,
    blobs: &std::collections::HashMap<String, String>,
    store: &crate::storage::Store,
) {
    // Pre-parse hydration patterns to avoid repeated parsing
    let parsed_patterns: Vec<(&str, &str, String, String)> = hydrate
        .iter()
        .filter_map(|(key, pattern)| {
            parse_hydration_pattern(pattern).map(|(collection, rkey)| {
                (key.as_str(), pattern.as_str(), collection, rkey)
            })
        })
        .collect();

    if parsed_patterns.is_empty() {
        return;
    }

    // Collect all DIDs and their hydration targets using references
    let mut hydration_lookups: std::collections::HashMap<String, Vec<(usize, &str)>> =
        std::collections::HashMap::new();

    for (idx, record) in records.iter().enumerate() {
        if let Some(uri) = record.get("uri").and_then(|u| u.as_str()) {
            if let Some(did) = extract_did_from_uri(uri) {
                for (key, _, collection, rkey) in &parsed_patterns {
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
    let hydrated_records: std::collections::HashMap<String, serde_json::Value> = hydration_lookups
        .keys()
        .par_bridge()
        .filter_map(|target_uri| {
            store.get_by_uri_string(target_uri).ok().flatten().map(|record| {
                let did = extract_did_from_uri(target_uri).unwrap_or_default();
                let mut record_json = serde_json::json!({
                    "uri": record.uri,
                    "cid": record.cid,
                    "value": record.value,
                    "indexedAt": record.indexed_at
                });

                // Transform blobs in hydrated record
                for (path, preset) in blobs {
                    for (key, _, _, _) in &parsed_patterns {
                        if let Some(sub_path) = path.strip_prefix(&format!("{}.", key)) {
                            transform_blob_at_path(&mut record_json, sub_path, &did, preset);
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

#[derive(Deserialize)]
pub struct QueryRequest {
    q: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    sort: Option<String>, // e.g. "indexedAt:desc", "indexedAt:asc"
    #[serde(default)]
    hydrate: std::collections::HashMap<String, String>, // key -> "at://$.did/collection/rkey"
    #[serde(default)]
    blobs: std::collections::HashMap<String, String>, // path -> preset (avatar, banner, feed_thumbnail)
    #[serde(default)]
    include_count: bool,
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
}

#[derive(Deserialize)]
pub struct ConfigUpdateRequest {
    mode: Option<String>,
    signal_collection: Option<String>,
    collections: Option<Vec<String>>,
}

async fn health(
    State((app, _)): State<AppStateHandle>,
) -> Result<&'static str, StatusCode> {
    // Verify database is accessible by doing a simple count
    app.store.count().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok("ok")
}

async fn stats(State((app, _)): State<AppStateHandle>) -> Result<Json<StatsResponse>, StatusCode> {
    let records = app.store.count().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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

async fn query_handler(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let parsed = Query::parse(&payload.q).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    let limit = payload
        .limit
        .unwrap_or(DEFAULT_QUERY_LIMIT)
        .min(MAX_QUERY_LIMIT);
    let config = app.config();

    // Handle sorted queries using indexes
    if let Some(ref sort) = payload.sort {
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
            Query::Exact(_) => {
                // Exact queries don't need sorting, just return the record
                return execute_unsorted_query(
                    &app,
                    &parsed,
                    limit,
                    payload.cursor.as_deref(),
                    &payload.hydrate,
                    &payload.blobs,
                    payload.include_count,
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
            .map(|r| {
                serde_json::json!({
                    "uri": r.uri,
                    "cid": r.cid,
                    "value": r.value,
                    "indexedAt": r.indexed_at
                })
            })
            .collect();

        // Apply hydration if requested
        if !payload.hydrate.is_empty() || !payload.blobs.is_empty() {
            hydrate_records(&mut values, &payload.hydrate, &payload.blobs, &app.store);
        }

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
        &payload.hydrate,
        &payload.blobs,
        payload.include_count,
    )
    .await
}

async fn execute_unsorted_query(
    app: &AppState,
    parsed: &Query,
    limit: usize,
    cursor: Option<&str>,
    hydrate: &std::collections::HashMap<String, String>,
    blobs: &std::collections::HashMap<String, String>,
    include_count: bool,
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
        .map(|r| {
            serde_json::json!({
                "uri": r.uri,
                "cid": r.cid,
                "value": r.value,
                "indexedAt": r.indexed_at
            })
        })
        .collect();

    // Apply hydration if requested
    if !hydrate.is_empty() || !blobs.is_empty() {
        hydrate_records(&mut values, hydrate, blobs, &app.store);
    }

    // Get count if requested
    let total = if include_count {
        match parsed {
            Query::AllCollection { collection } => app.store.count_collection(collection.as_str()).ok(),
            Query::Collection { collection, .. } => app.store.count_collection(collection.as_str()).ok(),
            Query::Exact(_) => Some(values.len()), // Exact query - count is just result count
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

async fn sync_handler(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, (StatusCode, Json<ErrorResponse>)> {
    let store = Arc::new(app.store.clone());
    let config = app.config();
    let collections = config.collections.clone();
    let indexes = config.indexes.clone();
    let result = tokio::task::spawn_blocking(move || sync::sync_repo(&payload.did, &store, &collections, &indexes))
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
        Ok(sync_result) => Ok(Json(SyncResponse {
            synced: sync_result.record_count,
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
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
    Ok(Json(ConfigResponse {
        mode: config.mode.to_string(),
        signal_collection: config.signal_collection,
        collections: config.collections,
        relay: config.relay,
        sync_parallelism: config.sync_parallelism,
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
    Ok(Json(ConfigResponse {
        mode: config.mode.to_string(),
        signal_collection: config.signal_collection,
        collections: config.collections,
        relay: config.relay,
        sync_parallelism: config.sync_parallelism,
    }))
}

fn start_firehose(
    relay: String,
    app: Arc<AppState>,
    sync_handle: Arc<SyncHandle>,
    running: Arc<AtomicBool>,
) {
    running.store(true, Ordering::Relaxed);

    std::thread::spawn(move || {
        // Load cursor from previous session (allows 72hr recovery)
        let cursor = app.get_firehose_cursor(&relay);
        if let Some(seq) = cursor {
            println!("Resuming firehose from cursor: {}", seq);
        }

        match FirehoseClient::connect(&relay, cursor) {
            Ok(mut client) => {
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

                            // Save cursor every 1000 events
                            if event_count % 1000 == 0 {
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
                                    let has_signal = config.signal_collection.as_ref().is_some_and(
                                        |signal| {
                                            operations.iter().any(|op| match op {
                                                Operation::Create { uri, .. } => {
                                                    uri.collection.as_str() == signal
                                                }
                                                _ => false,
                                            })
                                        },
                                    );

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
                                    let has_matching_collection = operations.iter().any(|op| {
                                        match op {
                                            Operation::Create { uri, .. } => {
                                                matches_any_filter(uri.collection.as_str(), &config.collections)
                                            }
                                            Operation::Delete { uri } => {
                                                matches_any_filter(uri.collection.as_str(), &config.collections)
                                            }
                                        }
                                    });

                                    if has_matching_collection && !app.repos.contains(did.as_str()).unwrap_or(false) {
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
                                        if let Err(e) = app.store.put_with_indexes(&uri, &record, &config.indexes) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                    }
                                    Operation::Delete { uri } => {
                                        if !matches_any_filter(
                                            uri.collection.as_str(),
                                            &config.collections,
                                        ) {
                                            continue;
                                        }
                                        if let Err(e) = app.store.delete_with_indexes(&uri, &config.indexes) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                    }
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
                            eprintln!("Firehose error: {}", e);
                            break;
                        }
                    }
                }

                // Save final cursor on exit
                if last_seq > 0 {
                    let _ = app.set_firehose_cursor(&relay, last_seq);
                    println!("Saved firehose cursor: {}", last_seq);
                }
            }
            Err(e) => {
                eprintln!("Failed to connect to firehose: {}", e);
            }
        }
        running.store(false, Ordering::Relaxed);
    });
}

pub async fn run(app: Arc<AppState>, port: u16, relay: Option<String>) {
    let running = Arc::new(AtomicBool::new(false));

    // Start sync worker
    let sync_handle = Arc::new(start_sync_worker(Arc::clone(&app)));

    // Start crawler
    start_crawler(Arc::clone(&app), Arc::clone(&sync_handle));

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
    let state: AppStateHandle = (app, sync_handle);

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
