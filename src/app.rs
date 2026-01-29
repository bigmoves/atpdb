use crate::config::{Config, ConfigError, IndexDirection};
use crate::repos::{RepoError, RepoStore};
use crate::storage::{StorageError, Store};
use fjall::{Database, KeyspaceCreateOptions};
use std::path::Path;
use parking_lot::RwLock;
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
    config: RwLock<Config>,
    config_keyspace: fjall::Keyspace,
    crawler_cursors: fjall::Keyspace,
}

const CONFIG_KEY: &[u8] = b"config";

impl AppState {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        let start = std::time::Instant::now();
        let db = Database::builder(path).open()?;
        eprintln!("[startup] db open: {:?}", start.elapsed());

        let records = db.keyspace("records", KeyspaceCreateOptions::default)?;
        let repos_keyspace = db.keyspace("repos", KeyspaceCreateOptions::default)?;
        let config_keyspace = db.keyspace("config", KeyspaceCreateOptions::default)?;
        let crawler_cursors = db.keyspace("crawler_cursors", KeyspaceCreateOptions::default)?;

        // Load config or use default, then apply env overrides
        let mut config: Config = match config_keyspace.get(CONFIG_KEY)? {
            Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            None => Config::default(),
        };
        config.apply_env_overrides();

        let store = Store::from_keyspace(records);
        eprintln!("[startup] keyspaces ready: {:?}", start.elapsed());

        // Build missing indexes on startup
        for index in &config.indexes {
            // Check if index has entries with data (not empty values from old format)
            let dir_char = match index.direction {
                IndexDirection::Asc => 'a',
                IndexDirection::Desc => 'd',
            };
            let prefix = format!("idx:{}:{}:{}\0", dir_char, index.collection, index.field);
            let needs_rebuild = match store.records_keyspace().prefix(prefix.as_bytes()).next() {
                None => true, // No entries at all
                Some(item) => {
                    // Check if value is empty (old format) or has data (new format)
                    match item.value() {
                        Ok(v) => v.is_empty(),
                        Err(_) => true,
                    }
                }
            };

            if needs_rebuild {
                // Delete old index entries first (for this direction only)
                let keys: Vec<_> = store.records_keyspace()
                    .prefix(prefix.as_bytes())
                    .filter_map(|item| item.key().ok().map(|k| k.to_vec()))
                    .collect();
                for key in keys {
                    let _ = store.records_keyspace().remove(&key);
                }

                println!("Building index {}:{}:{}...", index.collection, index.field, index.direction);
                match store.rebuild_index(index) {
                    Ok(count) => println!("Built index with {} entries", count),
                    Err(e) => eprintln!("Error building index: {}", e),
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
            let has_ts_index = store.records_keyspace().prefix(ts_prefix.as_bytes()).next().is_some();

            if !has_ts_index {
                println!("Rebuilding __ts__ index for {}...", collection);
                match store.rebuild_indexed_at(collection) {
                    Ok(count) => println!("Built __ts__ index with {} entries", count),
                    Err(e) => eprintln!("Error building __ts__ index: {}", e),
                }
            }
        }

        eprintln!("[startup] indexes checked: {:?}", start.elapsed());

        Ok(AppState {
            db,
            store,
            repos: RepoStore::from_keyspace(repos_keyspace),
            config: RwLock::new(config),
            config_keyspace,
            crawler_cursors,
        })
    }

    /// Flush all pending writes to disk
    pub fn flush(&self) -> Result<(), AppError> {
        self.db.persist(fjall::PersistMode::SyncAll)?;
        Ok(())
    }

    pub fn config(&self) -> Config {
        self.config.read().clone()
    }

    pub fn set_config(&self, config: Config) -> Result<(), AppError> {
        let value =
            serde_json::to_vec(&config).map_err(ConfigError::Serialization)?;
        self.config_keyspace.insert(CONFIG_KEY, &value)?;
        *self.config.write() = config;
        Ok(())
    }

    pub fn update_config<F>(&self, f: F) -> Result<(), AppError>
    where
        F: FnOnce(&mut Config),
    {
        let mut config = self.config();
        f(&mut config);
        self.set_config(config)
    }

    pub fn get_cursor(&self, key: &str) -> Option<String> {
        self.crawler_cursors
            .get(key.as_bytes())
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b.to_vec()).ok())
    }

    pub fn set_cursor(&self, key: &str, cursor: &str) -> Result<(), AppError> {
        self.crawler_cursors.insert(key.as_bytes(), cursor.as_bytes())?;
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
