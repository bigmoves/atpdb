use crate::cbor::dagcbor_to_json;
use crate::config::{matches_any_filter, IndexConfig, SearchFieldConfig};
use crate::search::SearchIndex;
use crate::storage::{Record, Store};
use crate::types::AtUri;
use metrics::histogram;
use repo_stream::{DiskBuilder, DriverBuilder};
use serde::Deserialize;
use std::io::Cursor;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::warn;

/// Timeout for repo fetch operations (5 minutes for large repos)
const REPO_FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// Shared HTTP client for connection pooling
fn http_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(REPO_FETCH_TIMEOUT)
            .user_agent("atpdb/0.1")
            .pool_max_idle_per_host(10)
            .build()
            .expect("Failed to create HTTP client")
    })
}

pub struct SyncResult {
    pub record_count: usize,
    pub rev: Option<String>,
    pub handle: Option<String>,
}

#[derive(Error, Debug)]
pub enum SyncError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("DID resolution failed: {0}")]
    DidResolution(String),
    #[error("CAR processing error: {0}")]
    Car(String),
    #[error("storage error: {0}")]
    Storage(#[from] crate::storage::StorageError),
}

#[derive(Deserialize)]
struct DidDocument {
    service: Option<Vec<DidService>>,
    #[serde(rename = "alsoKnownAs")]
    also_known_as: Option<Vec<String>>,
}

struct ResolvedDid {
    pds: String,
    handle: Option<String>,
}

#[derive(Deserialize)]
struct DidService {
    id: String,
    #[serde(rename = "serviceEndpoint")]
    service_endpoint: String,
}

/// Resolve a DID to find the PDS endpoint and handle
fn resolve_did(did: &str) -> Result<ResolvedDid, SyncError> {
    let url = if did.starts_with("did:plc:") {
        format!("https://plc.directory/{}", did)
    } else if did.starts_with("did:web:") {
        let domain = did.strip_prefix("did:web:").unwrap();
        format!("https://{}/.well-known/did.json", domain)
    } else {
        return Err(SyncError::DidResolution(format!(
            "unsupported DID method: {}",
            did
        )));
    };

    let doc: DidDocument = http_client().get(&url).send()?.json()?;

    let pds = doc
        .service
        .and_then(|services| {
            services
                .into_iter()
                .find(|s| s.id == "#atproto_pds" || s.id.ends_with("#atproto_pds"))
                .map(|s| s.service_endpoint)
        })
        .ok_or_else(|| SyncError::DidResolution("no PDS service found".to_string()))?;

    // Extract handle from alsoKnownAs (format: "at://handle")
    let handle = doc.also_known_as.and_then(|aka| {
        aka.into_iter()
            .find_map(|s| s.strip_prefix("at://").map(|h| h.to_string()))
    });

    Ok(ResolvedDid { pds, handle })
}

/// Sync a repo by DID with optional collection filtering
/// If `is_fresh_sync` is true, uses optimized batch writes (assumes all records are new)
#[allow(clippy::too_many_arguments)]
pub fn sync_repo(
    did: &str,
    store: &Arc<Store>,
    collections: &[String],
    indexes: &[IndexConfig],
    search: Option<&SearchIndex>,
    search_fields: &[SearchFieldConfig],
    is_fresh_sync: bool,
) -> Result<SyncResult, SyncError> {
    let start = Instant::now();

    tracing::debug!("resolving DID {}", did);
    let resolved = resolve_did(did)?;
    tracing::debug!("PDS: {}, handle: {:?}", resolved.pds, resolved.handle);

    tracing::debug!("fetching repo...");
    let url = format!("{}/xrpc/com.atproto.sync.getRepo?did={}", resolved.pds, did);
    let response = http_client().get(&url).send()?;

    if !response.status().is_success() {
        return Err(SyncError::Http(response.error_for_status().unwrap_err()));
    }

    let car_bytes = response.bytes()?;
    tracing::debug!("downloaded {} bytes", car_bytes.len());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut result = rt.block_on(process_car(
        &car_bytes,
        did,
        store,
        collections,
        indexes,
        search,
        search_fields,
        is_fresh_sync,
    ))?;

    histogram!("sync_duration_seconds").record(start.elapsed().as_secs_f64());
    histogram!("sync_repo_size_bytes").record(car_bytes.len() as f64);

    result.handle = resolved.handle;
    Ok(result)
}

/// Batch size for database writes during sync
const SYNC_BATCH_SIZE: usize = 500;

#[allow(clippy::too_many_arguments)]
async fn process_car(
    car_bytes: &[u8],
    did: &str,
    store: &Arc<Store>,
    collections: &[String],
    indexes: &[IndexConfig],
    search: Option<&SearchIndex>,
    search_fields: &[SearchFieldConfig],
    is_fresh_sync: bool,
) -> Result<SyncResult, SyncError> {
    let cursor = Cursor::new(car_bytes);
    let did = did.to_string();
    let store = Arc::clone(store);
    let collections = collections.to_vec();
    let indexes = indexes.to_vec();
    let search_fields = search_fields.to_vec();

    let driver = DriverBuilder::new()
        .with_mem_limit_mb(500)
        .with_block_processor(|data| data.to_vec())
        .load_car(cursor)
        .await
        .map_err(|e| SyncError::Car(e.to_string()))?;

    let mut count = 0;
    let rev;

    match driver {
        repo_stream::Driver::Memory(commit, mut driver) => {
            rev = Some(commit.rev.clone());
            let mut batch: Vec<(AtUri, Record)> = if is_fresh_sync {
                Vec::with_capacity(SYNC_BATCH_SIZE)
            } else {
                Vec::new()
            };

            while let Some(chunk) = driver
                .next_chunk(256)
                .await
                .map_err(|e| SyncError::Car(e.to_string()))?
            {
                for output in chunk {
                    // output.rkey is "collection/rkey"
                    let parts: Vec<&str> = output.rkey.splitn(2, '/').collect();
                    if parts.len() != 2 {
                        continue;
                    }
                    let collection = parts[0];

                    // Apply collection filter
                    if !matches_any_filter(collection, &collections) {
                        continue;
                    }

                    let uri_str = format!("at://{}/{}", did, output.rkey);

                    match uri_str.parse::<AtUri>() {
                        Ok(uri) => {
                            match dagcbor_to_json(&output.data) {
                                Ok(value) => {
                                    let record = Record {
                                        uri: uri_str.clone(),
                                        cid: output.cid.to_string(),
                                        value,
                                        indexed_at: SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs(),
                                    };

                                    if is_fresh_sync {
                                        // Batch writes for fresh sync
                                        batch.push((uri, record));
                                        if batch.len() >= SYNC_BATCH_SIZE {
                                            match store.put_batch_with_indexes(&batch, &indexes) {
                                                Ok(n) => {
                                                    if let Some(search) = search {
                                                        for (_, rec) in &batch {
                                                            let col = rec
                                                                .uri
                                                                .split('/')
                                                                .nth(3)
                                                                .unwrap_or("");
                                                            let _ = search.index_record(
                                                                &rec.uri,
                                                                col,
                                                                &rec.value,
                                                                &search_fields,
                                                            );
                                                        }
                                                    }
                                                    count += n;
                                                }
                                                Err(e) => warn!(error = %e, "Batch write error"),
                                            }
                                            batch.clear();
                                        }
                                    } else {
                                        // Individual writes for resync (handles existence checks)
                                        if store.put_with_indexes(&uri, &record, &indexes).is_ok() {
                                            if let Some(search) = search {
                                                let _ = search.index_record(
                                                    &record.uri,
                                                    collection,
                                                    &record.value,
                                                    &search_fields,
                                                );
                                            }
                                            count += 1;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(uri = %uri_str, error = %e, "CBOR decode error");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(uri = %uri_str, error = %e, "URI parse error");
                        }
                    }
                }
            }

            // Flush remaining records (batch mode only)
            if is_fresh_sync && !batch.is_empty() {
                match store.put_batch_with_indexes(&batch, &indexes) {
                    Ok(n) => {
                        if let Some(search) = search {
                            for (_, rec) in &batch {
                                let col = rec.uri.split('/').nth(3).unwrap_or("");
                                let _ =
                                    search.index_record(&rec.uri, col, &rec.value, &search_fields);
                            }
                        }
                        count += n;
                    }
                    Err(e) => warn!(error = %e, "Final batch write error"),
                }
            }
        }
        repo_stream::Driver::Disk(need_disk) => {
            // Large repo - use disk-backed storage
            // Use timestamp + thread ID for unique temp directory
            let unique_id = format!(
                "{}-{:?}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                std::thread::current().id()
            );
            let temp_dir = std::env::temp_dir().join(format!("atpdb-sync-{}", unique_id));

            let disk_store = DiskBuilder::new()
                .with_cache_size_mb(64)
                .with_max_stored_mb(2048) // 2GB max for really large repos
                .open(temp_dir.clone())
                .await
                .map_err(|e| SyncError::Car(format!("Failed to create disk store: {}", e)))?;

            let (commit, mut driver) = need_disk
                .finish_loading(disk_store)
                .await
                .map_err(|e| SyncError::Car(format!("Failed to finish loading: {}", e)))?;

            rev = Some(commit.rev.clone());
            let mut batch: Vec<(AtUri, Record)> = if is_fresh_sync {
                Vec::with_capacity(SYNC_BATCH_SIZE)
            } else {
                Vec::new()
            };

            while let Some(chunk) = driver
                .next_chunk(256)
                .await
                .map_err(|e| SyncError::Car(e.to_string()))?
            {
                for output in chunk {
                    let parts: Vec<&str> = output.rkey.splitn(2, '/').collect();
                    if parts.len() != 2 {
                        continue;
                    }
                    let collection = parts[0];

                    if !matches_any_filter(collection, &collections) {
                        continue;
                    }

                    let uri_str = format!("at://{}/{}", did, output.rkey);

                    match uri_str.parse::<AtUri>() {
                        Ok(uri) => match dagcbor_to_json(&output.data) {
                            Ok(value) => {
                                let record = Record {
                                    uri: uri_str.clone(),
                                    cid: output.cid.to_string(),
                                    value,
                                    indexed_at: SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs(),
                                };

                                if is_fresh_sync {
                                    batch.push((uri, record));
                                    if batch.len() >= SYNC_BATCH_SIZE {
                                        match store.put_batch_with_indexes(&batch, &indexes) {
                                            Ok(n) => {
                                                if let Some(search) = search {
                                                    for (_, rec) in &batch {
                                                        let col =
                                                            rec.uri.split('/').nth(3).unwrap_or("");
                                                        let _ = search.index_record(
                                                            &rec.uri,
                                                            col,
                                                            &rec.value,
                                                            &search_fields,
                                                        );
                                                    }
                                                }
                                                count += n;
                                            }
                                            Err(e) => warn!(error = %e, "Batch write error"),
                                        }
                                        batch.clear();
                                    }
                                } else if store.put_with_indexes(&uri, &record, &indexes).is_ok() {
                                    if let Some(search) = search {
                                        let _ = search.index_record(
                                            &record.uri,
                                            collection,
                                            &record.value,
                                            &search_fields,
                                        );
                                    }
                                    count += 1;
                                }
                            }
                            Err(e) => {
                                warn!(uri = %uri_str, error = %e, "CBOR decode error");
                            }
                        },
                        Err(e) => {
                            warn!(uri = %uri_str, error = %e, "URI parse error");
                        }
                    }
                }
            }

            // Flush remaining records (batch mode only)
            if is_fresh_sync && !batch.is_empty() {
                match store.put_batch_with_indexes(&batch, &indexes) {
                    Ok(n) => {
                        if let Some(search) = search {
                            for (_, rec) in &batch {
                                let col = rec.uri.split('/').nth(3).unwrap_or("");
                                let _ =
                                    search.index_record(&rec.uri, col, &rec.value, &search_fields);
                            }
                        }
                        count += n;
                    }
                    Err(e) => warn!(error = %e, "Final batch write error"),
                }
            }

            // Clean up temp directory
            if let Err(e) = std::fs::remove_dir_all(&temp_dir) {
                warn!(path = %temp_dir.display(), error = %e, "Failed to clean up temp dir");
            }
        }
    }

    Ok(SyncResult {
        record_count: count,
        rev,
        handle: None, // Set by caller after DID resolution
    })
}
