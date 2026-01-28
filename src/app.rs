use crate::config::{Config, ConfigError, IndexDirection};
use crate::repos::{RepoError, RepoStore};
use crate::storage::{StorageError, Store};
use fjall::{Database, KeyspaceCreateOptions};
use std::path::Path;
use std::sync::RwLock;
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
    pub store: Store,
    pub repos: RepoStore,
    config: RwLock<Config>,
    config_keyspace: fjall::Keyspace,
    crawler_cursors: fjall::Keyspace,
}

const CONFIG_KEY: &[u8] = b"config";

impl AppState {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        let db = Database::builder(path).open()?;

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

        // Clean up indexes not in config
        let configured_prefixes: std::collections::HashSet<String> = config
            .indexes
            .iter()
            .map(|idx| {
                let dir_char = match idx.direction {
                    IndexDirection::Asc => 'a',
                    IndexDirection::Desc => 'd',
                };
                format!("idx:{}:{}:{}\0", dir_char, idx.collection, idx.field)
            })
            .collect();

        // Find and remove orphaned indexes (skip built-in __ts__ indexes for indexedAt)
        for dir in ['a', 'd'] {
            let scan_prefix = format!("idx:{}:", dir);
            let builtin_ts_prefix = format!("idx:{}:__ts__:", dir);
            let mut current_index_prefix: Option<String> = None;
            let mut keys_to_delete: Vec<Vec<u8>> = Vec::new();

            for item in store.records_keyspace().prefix(scan_prefix.as_bytes()) {
                if let Ok(key) = item.key() {
                    if let Ok(key_str) = std::str::from_utf8(&key) {
                        // Skip built-in __ts__ indexes (used for indexedAt sorting)
                        if key_str.starts_with(&builtin_ts_prefix) {
                            continue;
                        }

                        // Extract prefix up to first \0
                        if let Some(null_pos) = key_str.find('\0') {
                            let prefix = format!("{}\0", &key_str[..null_pos]);

                            // Check if this is a new prefix we haven't seen
                            if current_index_prefix.as_ref() != Some(&prefix) {
                                current_index_prefix = Some(prefix.clone());

                                // If not in config, mark for deletion
                                if !configured_prefixes.contains(&prefix) {
                                    println!(
                                        "Removing orphaned index: {}",
                                        &prefix[..prefix.len() - 1]
                                    );
                                }
                            }

                            // If current prefix is orphaned, delete this key
                            if let Some(ref p) = current_index_prefix {
                                if !configured_prefixes.contains(p) {
                                    keys_to_delete.push(key.to_vec());
                                }
                            }
                        }
                    }
                }
            }

            for key in keys_to_delete {
                let _ = store.records_keyspace().remove(&key);
            }
        }

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
                let keys: Vec<_> = store
                    .records_keyspace()
                    .prefix(prefix.as_bytes())
                    .filter_map(|item| item.key().ok().map(|k| k.to_vec()))
                    .collect();
                for key in keys {
                    let _ = store.records_keyspace().remove(&key);
                }

                println!(
                    "Building index {}:{}:{}...",
                    index.collection, index.field, index.direction
                );
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
            let has_ts_index = store
                .records_keyspace()
                .prefix(ts_prefix.as_bytes())
                .next()
                .is_some();

            if !has_ts_index {
                println!("Rebuilding __ts__ index for {}...", collection);
                match store.rebuild_indexed_at(collection) {
                    Ok(count) => println!("Built __ts__ index with {} entries", count),
                    Err(e) => eprintln!("Error building __ts__ index: {}", e),
                }
            }
        }

        Ok(AppState {
            store,
            repos: RepoStore::from_keyspace(repos_keyspace),
            config: RwLock::new(config),
            config_keyspace,
            crawler_cursors,
        })
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn set_config(&self, config: Config) -> Result<(), AppError> {
        let value = serde_json::to_vec(&config).map_err(ConfigError::Serialization)?;
        self.config_keyspace.insert(CONFIG_KEY, &value)?;
        *self.config.write().unwrap() = config;
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
        self.crawler_cursors
            .insert(key.as_bytes(), cursor.as_bytes())?;
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
