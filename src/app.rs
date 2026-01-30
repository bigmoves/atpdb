use crate::config::{Config, ConfigError, IndexDirection, IndexFieldType};
use crate::repos::{RepoError, RepoStore};
use crate::resync_buffer::ResyncBuffer;
use crate::search::SearchIndex;
use crate::storage::{StorageError, Store};
use fjall::{Database, KeyspaceCreateOptions};
use parking_lot::RwLock;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("repo error: {0}")]
    Repo(#[from] RepoError),
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("fjall error: {0}")]
    Fjall(#[from] fjall::Error),
}

pub struct AppState {
    db: Database,
    pub store: Store,
    pub repos: RepoStore,
    pub resync_buffer: ResyncBuffer,
    pub search: Option<SearchIndex>,
    config: RwLock<Config>,
    config_keyspace: fjall::Keyspace,
    crawler_cursors: fjall::Keyspace,
    handles: fjall::Keyspace,
}

const CONFIG_KEY: &[u8] = b"config";

/// Default cache size in bytes (1 GB)
/// fjall docs recommend 20-25% of available memory, or more if dataset fits in memory
const DEFAULT_CACHE_SIZE: u64 = 1024 * 1024 * 1024;

impl AppState {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        let start = std::time::Instant::now();

        // Configure cache size from env or use default
        let cache_size = std::env::var("ATPDB_CACHE_SIZE_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|mb| mb * 1024 * 1024)
            .unwrap_or(DEFAULT_CACHE_SIZE);

        let db = Database::builder(path).cache_size(cache_size).open()?;
        tracing::info!(
            "db open with {}MB cache: {:?}",
            cache_size / 1024 / 1024,
            start.elapsed()
        );

        let records = db.keyspace("records", KeyspaceCreateOptions::default)?;
        let repos_keyspace = db.keyspace("repos", KeyspaceCreateOptions::default)?;
        let config_keyspace = db.keyspace("config", KeyspaceCreateOptions::default)?;
        let crawler_cursors = db.keyspace("crawler_cursors", KeyspaceCreateOptions::default)?;
        let handles = db.keyspace("handles", KeyspaceCreateOptions::default)?;
        let resync_buffer_keyspace =
            db.keyspace("resync_buffer", KeyspaceCreateOptions::default)?;

        // Load config or use default, then apply env overrides
        let mut config: Config = match config_keyspace.get(CONFIG_KEY)? {
            Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            None => Config::default(),
        };
        config.apply_env_overrides();

        let store = Store::new(db.clone(), records);
        tracing::debug!("keyspaces ready: {:?}", start.elapsed());

        // Build missing indexes on startup
        for index in &config.indexes {
            // Check if index has entries - different prefix for at-uri vs sorted indexes
            let (prefix, index_label) = if index.field_type == IndexFieldType::AtUri {
                // at-uri indexes use rev: prefix
                (
                    format!("rev:{}:{}\0", index.collection, index.field),
                    format!("{}:{}:{}", index.collection, index.field, index.field_type),
                )
            } else {
                // Sorted indexes use idx:{a|d}: prefix
                let dir_char = match index.direction {
                    IndexDirection::Asc => 'a',
                    IndexDirection::Desc => 'd',
                };
                (
                    format!("idx:{}:{}:{}\0", dir_char, index.collection, index.field),
                    format!("{}:{}:{}", index.collection, index.field, index.direction),
                )
            };

            let needs_rebuild = match store.records_keyspace().prefix(prefix.as_bytes()).next() {
                None => true, // No entries at all
                Some(item) => {
                    // Check if value is empty (old format) or has data (new format)
                    // at-uri indexes always have empty values, so only check sorted indexes
                    if index.field_type == IndexFieldType::AtUri {
                        false // Has entries, no rebuild needed
                    } else {
                        match item.value() {
                            Ok(v) => v.is_empty(),
                            Err(_) => true,
                        }
                    }
                }
            };

            if needs_rebuild {
                // Delete old index entries first
                let keys: Vec<_> = store
                    .records_keyspace()
                    .prefix(prefix.as_bytes())
                    .filter_map(|item| item.key().ok().map(|k| k.to_vec()))
                    .collect();
                for key in keys {
                    let _ = store.records_keyspace().remove(&key);
                }

                tracing::info!(index = index_label, "Building index");
                match store.rebuild_index(index) {
                    Ok(count) => tracing::info!(index = index_label, count, "Built index"),
                    Err(e) => {
                        tracing::error!(index = index_label, error = %e, "Error building index")
                    }
                }
            }
        }

        // Rebuild __ts__ indexes if missing (for indexedAt sorting and count)
        // Only check configured collections to avoid slow full-DB scan
        for collection in &config.collections {
            // Handle wildcard patterns - skip them
            if collection.contains('*') {
                continue;
            }
            let ts_prefix = format!("idx:a:__ts__:{}\0", collection);
            let has_ts_index = store
                .records_keyspace()
                .prefix(ts_prefix.as_bytes())
                .next()
                .is_some();

            if !has_ts_index {
                tracing::info!(collection, "Rebuilding __ts__ index");
                match store.rebuild_indexed_at(collection) {
                    Ok(count) => tracing::info!(collection, count, "Built __ts__ index"),
                    Err(e) => {
                        tracing::error!(collection, error = %e, "Error building __ts__ index")
                    }
                }
            }
        }

        tracing::debug!("indexes checked: {:?}", start.elapsed());

        // Initialize search index
        let search_path = path.join("search");
        let search = match SearchIndex::open(&search_path) {
            Ok(idx) => {
                tracing::debug!("search index opened at {:?}", search_path);
                Some(idx)
            }
            Err(e) => {
                tracing::warn!("failed to open search index: {}. Search disabled.", e);
                None
            }
        };

        Ok(AppState {
            db,
            store,
            repos: RepoStore::from_keyspace(repos_keyspace),
            resync_buffer: ResyncBuffer::from_keyspace(resync_buffer_keyspace),
            search,
            config: RwLock::new(config),
            config_keyspace,
            crawler_cursors,
            handles,
        })
    }

    /// Flush all pending writes to disk
    pub fn flush(&self) -> Result<(), AppError> {
        self.db.persist(fjall::PersistMode::SyncAll)?;
        if let Some(ref search) = self.search {
            search.commit().map_err(|e| {
                AppError::Fjall(fjall::Error::Io(std::io::Error::other(e.to_string())))
            })?;
        }
        Ok(())
    }

    /// Returns the disk space usage of the database in bytes
    pub fn disk_space(&self) -> Result<u64, AppError> {
        Ok(self.db.disk_space()?)
    }

    pub fn config(&self) -> Config {
        self.config.read().clone()
    }

    pub fn set_config(&self, config: Config) -> Result<(), AppError> {
        let value = serde_json::to_vec(&config).map_err(ConfigError::Serialization)?;
        self.config_keyspace.insert(CONFIG_KEY, &value)?;
        *self.config.write() = config;
        Ok(())
    }

    pub fn update_config<F>(&self, f: F) -> Result<(), AppError>
    where
        F: FnOnce(&mut Config),
    {
        let old_search_fields = self.config().search_fields.clone();
        let mut config = self.config();
        f(&mut config);

        // Check for new search fields
        let new_fields: Vec<_> = config
            .search_fields
            .iter()
            .filter(|sf| !old_search_fields.contains(sf))
            .cloned()
            .collect();

        self.set_config(config)?;

        // Trigger background reindex for new fields
        if !new_fields.is_empty() {
            if let Some(ref search) = self.search {
                let search = search.clone();
                let store = self.store.clone();
                std::thread::spawn(move || {
                    tracing::info!(
                        field_count = new_fields.len(),
                        "Auto-reindexing new search fields"
                    );
                    match search.reindex_from_store(&store, &new_fields) {
                        Ok(count) => tracing::info!(count, "Auto-reindexed records"),
                        Err(e) => tracing::error!(error = %e, "Auto-reindex error"),
                    }
                });
            }
        }

        Ok(())
    }

    pub fn get_cursor(&self, key: &str) -> Option<String> {
        self.crawler_cursors
            .get(key.as_bytes())
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b.to_vec()).ok())
    }

    pub fn set_cursor(&self, key: &str, cursor: &str) -> Result<(), AppError> {
        self.crawler_cursors
            .insert(key.as_bytes(), cursor.as_bytes())?;
        Ok(())
    }

    /// Get firehose cursor for a relay
    pub fn get_firehose_cursor(&self, relay: &str) -> Option<i64> {
        let key = format!("firehose:{}", relay);
        self.get_cursor(&key).and_then(|s| s.parse().ok())
    }

    /// Set firehose cursor for a relay
    pub fn set_firehose_cursor(&self, relay: &str, seq: i64) -> Result<(), AppError> {
        let key = format!("firehose:{}", relay);
        self.set_cursor(&key, &seq.to_string())
    }

    /// Get handle for a DID
    pub fn get_handle(&self, did: &str) -> Option<String> {
        self.handles
            .get(did.as_bytes())
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b.to_vec()).ok())
    }

    /// Set handle for a DID
    pub fn set_handle(&self, did: &str, handle: &str) -> Result<(), AppError> {
        self.handles.insert(did.as_bytes(), handle.as_bytes())?;
        Ok(())
    }

    pub fn list_cursors(&self) -> Result<Vec<(String, String)>, AppError> {
        let config = self.config();
        let relay = &config.relay;
        let mut cursors = Vec::new();

        // Check listRepos cursor
        let list_repos_key = format!("listRepos:{}", relay);
        if let Some(cursor) = self.get_cursor(&list_repos_key) {
            cursors.push((list_repos_key, cursor));
        }

        // Check listReposByCollection cursor if signal collection is configured
        if let Some(ref collection) = config.signal_collection {
            let collection_key = format!("listReposByCollection:{}:{}", relay, collection);
            if let Some(cursor) = self.get_cursor(&collection_key) {
                cursors.push((collection_key, cursor));
            }
        }

        Ok(cursors)
    }
}
