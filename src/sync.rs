use crate::cbor::dagcbor_to_json;
use crate::config::{matches_any_filter, IndexConfig};
use crate::storage::{Record, Store};
use crate::types::AtUri;
use repo_stream::DriverBuilder;
use serde::Deserialize;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub struct SyncResult {
    pub record_count: usize,
    pub rev: Option<String>,
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
}

#[derive(Deserialize)]
struct DidService {
    id: String,
    #[serde(rename = "serviceEndpoint")]
    service_endpoint: String,
}

/// Resolve a DID to find the PDS endpoint
fn resolve_pds(did: &str) -> Result<String, SyncError> {
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

    let client = reqwest::blocking::Client::new();
    let doc: DidDocument = client.get(&url).send()?.json()?;

    doc.service
        .and_then(|services| {
            services
                .into_iter()
                .find(|s| s.id == "#atproto_pds" || s.id.ends_with("#atproto_pds"))
                .map(|s| s.service_endpoint)
        })
        .ok_or_else(|| SyncError::DidResolution("no PDS service found".to_string()))
}

/// Sync a repo by DID with optional collection filtering
pub fn sync_repo(
    did: &str,
    store: &Arc<Store>,
    collections: &[String],
    indexes: &[IndexConfig],
) -> Result<SyncResult, SyncError> {
    println!("Resolving DID...");
    let pds = resolve_pds(did)?;
    println!("PDS: {}", pds);

    println!("Fetching repo...");
    let url = format!("{}/xrpc/com.atproto.sync.getRepo?did={}", pds, did);
    let client = reqwest::blocking::Client::new();
    let response = client.get(&url).send()?;

    if !response.status().is_success() {
        return Err(SyncError::Http(response.error_for_status().unwrap_err()));
    }

    let car_bytes = response.bytes()?;
    println!("Downloaded {} bytes", car_bytes.len());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(process_car(&car_bytes, did, store, collections, indexes))?;

    Ok(result)
}

async fn process_car(
    car_bytes: &[u8],
    did: &str,
    store: &Arc<Store>,
    collections: &[String],
    indexes: &[IndexConfig],
) -> Result<SyncResult, SyncError> {
    let cursor = Cursor::new(car_bytes);
    let did = did.to_string();
    let store = Arc::clone(store);
    let collections = collections.to_vec();
    let indexes = indexes.to_vec();

    let driver = DriverBuilder::new()
        .with_mem_limit_mb(100)
        .with_block_processor(|data| data.to_vec())
        .load_car(cursor)
        .await
        .map_err(|e| SyncError::Car(e.to_string()))?;

    let mut count = 0;
    let rev;

    match driver {
        repo_stream::Driver::Memory(commit, mut driver) => {
            rev = Some(commit.rev.clone());

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
                                        uri: uri_str,
                                        cid: output.cid.to_string(),
                                        value,
                                        indexed_at: SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs(),
                                    };
                                    if store.put_with_indexes(&uri, &record, &indexes).is_ok() {
                                        count += 1;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("CBOR decode error for {}: {}", uri_str, e);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("URI parse error for {}: {}", uri_str, e);
                        }
                    }
                }
            }
        }
        repo_stream::Driver::Disk(_paused) => {
            return Err(SyncError::Car(
                "CAR too large for memory, disk mode not implemented".to_string(),
            ));
        }
    }

    Ok(SyncResult {
        record_count: count,
        rev,
    })
}
