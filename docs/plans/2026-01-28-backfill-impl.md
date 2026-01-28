# Backfill System Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add tap-like sync/resync capabilities with repo state tracking, retry logic, collection filtering, and three operating modes (manual, signal, full-network).

**Architecture:** New `repos` module manages repo state in fjall. Background sync worker pulls from channel queue. Firehose integration routes events based on mode and detects desync. Config stored in fjall `config` keyspace.

**Tech Stack:** fjall (storage), tokio (async runtime), mpsc channels (sync queue)

---

### Task 1: Add RepoState Types

**Files:**
- Create: `src/repos.rs`
- Modify: `src/main.rs:1-8` (add mod declaration)

**Step 1: Create the repos module with types**

Create `src/repos.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoStatus {
    Pending,
    Syncing,
    Synced,
    Error,
    Desync,
}

impl std::fmt::Display for RepoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoStatus::Pending => write!(f, "pending"),
            RepoStatus::Syncing => write!(f, "syncing"),
            RepoStatus::Synced => write!(f, "synced"),
            RepoStatus::Error => write!(f, "error"),
            RepoStatus::Desync => write!(f, "desync"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    pub did: String,
    pub status: RepoStatus,
    pub rev: Option<String>,
    pub record_count: u64,
    pub last_sync: Option<u64>,
    pub error: Option<String>,
    pub retry_count: u32,
    pub next_retry: Option<u64>,
}

impl RepoState {
    pub fn new(did: String) -> Self {
        RepoState {
            did,
            status: RepoStatus::Pending,
            rev: None,
            record_count: 0,
            last_sync: None,
            error: None,
            retry_count: 0,
            next_retry: None,
        }
    }
}
```

**Step 2: Add mod declaration to main.rs**

In `src/main.rs`, add after line 7 (`mod types;`):

```rust
mod repos;
```

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: Compiles with no errors (warning about unused is fine)

**Step 4: Commit**

```bash
git add src/repos.rs src/main.rs
git commit -m "feat: add RepoState types for backfill tracking"
```

---

### Task 2: Add Repos Keyspace to Storage

**Files:**
- Modify: `src/storage.rs:23-36` (Store struct and open)
- Modify: `src/repos.rs` (add RepoStore)

**Step 1: Write test for RepoStore**

Add to end of `src/repos.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_repo_store_put_get() {
        let dir = tempdir().unwrap();
        let store = RepoStore::open(dir.path()).unwrap();

        let did = "did:plc:test123";
        let state = RepoState::new(did.to_string());

        store.put(&state).unwrap();
        let retrieved = store.get(did).unwrap().unwrap();

        assert_eq!(retrieved.did, did);
        assert_eq!(retrieved.status, RepoStatus::Pending);
    }

    #[test]
    fn test_repo_store_list() {
        let dir = tempdir().unwrap();
        let store = RepoStore::open(dir.path()).unwrap();

        store.put(&RepoState::new("did:plc:aaa".to_string())).unwrap();
        store.put(&RepoState::new("did:plc:bbb".to_string())).unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_repo_store_delete() {
        let dir = tempdir().unwrap();
        let store = RepoStore::open(dir.path()).unwrap();

        let did = "did:plc:test";
        store.put(&RepoState::new(did.to_string())).unwrap();
        store.delete(did).unwrap();

        assert!(store.get(did).unwrap().is_none());
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_repo_store --no-run 2>&1 | head -20`
Expected: Fails to compile - RepoStore not defined

**Step 3: Implement RepoStore**

Add to `src/repos.rs` before the tests:

```rust
use fjall::{Keyspace, PartitionCreateOptions};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum RepoError {
    #[error("storage error: {0}")]
    Storage(#[from] fjall::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub struct RepoStore {
    repos: Keyspace,
}

impl RepoStore {
    pub fn open(path: &Path) -> Result<Self, RepoError> {
        let db = fjall::Config::new(path).open()?;
        let repos = db.open_partition("repos", PartitionCreateOptions::default())?;
        Ok(RepoStore { repos })
    }

    pub fn open_with_db(db: &fjall::Keyspace) -> Result<Self, RepoError> {
        // For sharing db with main Store - we'll use this later
        Ok(RepoStore { repos: db.clone() })
    }

    pub fn put(&self, state: &RepoState) -> Result<(), RepoError> {
        let value = serde_json::to_vec(state)?;
        self.repos.insert(state.did.as_bytes(), &value)?;
        Ok(())
    }

    pub fn get(&self, did: &str) -> Result<Option<RepoState>, RepoError> {
        match self.repos.get(did.as_bytes())? {
            Some(bytes) => {
                let state: RepoState = serde_json::from_slice(&bytes)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    pub fn delete(&self, did: &str) -> Result<(), RepoError> {
        self.repos.remove(did.as_bytes())?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<RepoState>, RepoError> {
        let mut results = Vec::new();
        for item in self.repos.iter() {
            let (_, value) = item?;
            let state: RepoState = serde_json::from_slice(&value)?;
            results.push(state);
        }
        Ok(results)
    }

    pub fn list_by_status(&self, status: RepoStatus) -> Result<Vec<RepoState>, RepoError> {
        let mut results = Vec::new();
        for item in self.repos.iter() {
            let (_, value) = item?;
            let state: RepoState = serde_json::from_slice(&value)?;
            if state.status == status {
                results.push(state);
            }
        }
        Ok(results)
    }

    pub fn contains(&self, did: &str) -> Result<bool, RepoError> {
        Ok(self.repos.contains_key(did.as_bytes())?)
    }
}
```

**Step 4: Run tests**

Run: `cargo test test_repo_store`
Expected: All 3 tests pass

**Step 5: Commit**

```bash
git add src/repos.rs
git commit -m "feat: add RepoStore for repo state persistence"
```

---

### Task 3: Add Config Types and Storage

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs` (add mod declaration)

**Step 1: Create config module with types and tests**

Create `src/config.rs`:

```rust
use fjall::{Keyspace, PartitionCreateOptions};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("storage error: {0}")]
    Storage(#[from] fjall::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Manual,
    Signal,
    FullNetwork,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Manual => write!(f, "manual"),
            Mode::Signal => write!(f, "signal"),
            Mode::FullNetwork => write!(f, "full-network"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mode: Mode,
    pub signal_collection: Option<String>,
    pub collections: Vec<String>,
    pub relay: String,
    pub sync_parallelism: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: Mode::Manual,
            signal_collection: None,
            collections: Vec::new(),
            relay: "bsky.network".to_string(),
            sync_parallelism: 3,
        }
    }
}

pub struct ConfigStore {
    partition: Keyspace,
}

const CONFIG_KEY: &[u8] = b"config";

impl ConfigStore {
    pub fn open(path: &Path) -> Result<Self, ConfigError> {
        let db = fjall::Config::new(path).open()?;
        let partition = db.open_partition("config", PartitionCreateOptions::default())?;
        Ok(ConfigStore { partition })
    }

    pub fn get(&self) -> Result<Config, ConfigError> {
        match self.partition.get(CONFIG_KEY)? {
            Some(bytes) => {
                let config: Config = serde_json::from_slice(&bytes)?;
                Ok(config)
            }
            None => Ok(Config::default()),
        }
    }

    pub fn set(&self, config: &Config) -> Result<(), ConfigError> {
        let value = serde_json::to_vec(config)?;
        self.partition.insert(CONFIG_KEY, &value)?;
        Ok(())
    }
}

/// Check if a collection matches a filter pattern
/// Supports wildcards at NSID segment boundaries: "fm.teal.*" matches "fm.teal.alpha.feed"
pub fn matches_collection_filter(collection: &str, filter: &str) -> bool {
    if filter == "*" {
        return true;
    }
    if filter.ends_with(".*") {
        let prefix = &filter[..filter.len() - 2];
        collection.starts_with(prefix) && collection.len() > prefix.len() && collection.as_bytes()[prefix.len()] == b'.'
    } else {
        collection == filter
    }
}

/// Check if collection matches any of the filters (empty filters = match all)
pub fn matches_any_filter(collection: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters.iter().any(|f| matches_collection_filter(collection, f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.mode, Mode::Manual);
        assert_eq!(config.relay, "bsky.network");
        assert!(config.collections.is_empty());
    }

    #[test]
    fn test_config_store() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::open(dir.path()).unwrap();

        // Should return default initially
        let config = store.get().unwrap();
        assert_eq!(config.mode, Mode::Manual);

        // Update and read back
        let mut config = Config::default();
        config.mode = Mode::Signal;
        config.signal_collection = Some("fm.teal.alpha.feed".to_string());
        config.collections = vec!["fm.teal.*".to_string()];
        store.set(&config).unwrap();

        let loaded = store.get().unwrap();
        assert_eq!(loaded.mode, Mode::Signal);
        assert_eq!(loaded.signal_collection, Some("fm.teal.alpha.feed".to_string()));
    }

    #[test]
    fn test_matches_collection_filter_exact() {
        assert!(matches_collection_filter("app.bsky.feed.post", "app.bsky.feed.post"));
        assert!(!matches_collection_filter("app.bsky.feed.like", "app.bsky.feed.post"));
    }

    #[test]
    fn test_matches_collection_filter_wildcard() {
        assert!(matches_collection_filter("fm.teal.alpha.feed", "fm.teal.*"));
        assert!(matches_collection_filter("fm.teal.alpha.like", "fm.teal.*"));
        assert!(matches_collection_filter("fm.teal.beta", "fm.teal.*"));
        assert!(!matches_collection_filter("fm.teal", "fm.teal.*")); // Must have segment after
        assert!(!matches_collection_filter("app.bsky.feed.post", "fm.teal.*"));
    }

    #[test]
    fn test_matches_any_filter() {
        let filters = vec!["fm.teal.*".to_string(), "app.bsky.feed.post".to_string()];
        assert!(matches_any_filter("fm.teal.alpha.feed", &filters));
        assert!(matches_any_filter("app.bsky.feed.post", &filters));
        assert!(!matches_any_filter("app.bsky.feed.like", &filters));

        // Empty filters match all
        assert!(matches_any_filter("anything", &[]));
    }
}
```

**Step 2: Add mod declaration to main.rs**

In `src/main.rs`, add after `mod repos;`:

```rust
mod config;
```

**Step 3: Run tests**

Run: `cargo test config`
Expected: All config tests pass

**Step 4: Commit**

```bash
git add src/config.rs src/main.rs
git commit -m "feat: add Config types with collection filtering"
```

---

### Task 4: Create Unified AppState

**Files:**
- Create: `src/app.rs`
- Modify: `src/main.rs` (add mod, refactor to use AppState)

**Step 1: Create app module with shared state**

Create `src/app.rs`:

```rust
use crate::config::{Config, ConfigError};
use crate::repos::{RepoError, RepoState, RepoStatus, RepoStore};
use crate::storage::{Store, StorageError};
use fjall::{Keyspace, PartitionCreateOptions};
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
    config_partition: Keyspace,
}

const CONFIG_KEY: &[u8] = b"config";

impl AppState {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        let db = fjall::Config::new(path).open()?;

        let records = db.open_partition("records", PartitionCreateOptions::default())?;
        let repos_partition = db.open_partition("repos", PartitionCreateOptions::default())?;
        let config_partition = db.open_partition("config", PartitionCreateOptions::default())?;

        // Load config or use default
        let config = match config_partition.get(CONFIG_KEY)? {
            Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            None => Config::default(),
        };

        Ok(AppState {
            store: Store::from_partition(records),
            repos: RepoStore::from_partition(repos_partition),
            config: RwLock::new(config),
            config_partition,
        })
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn set_config(&self, config: Config) -> Result<(), AppError> {
        let value = serde_json::to_vec(&config).map_err(|e| ConfigError::Serialization(e))?;
        self.config_partition.insert(CONFIG_KEY, &value)?;
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
}
```

**Step 2: Update Store to support from_partition**

In `src/storage.rs`, add method to Store impl (after `open`):

```rust
    pub fn from_partition(records: fjall::Keyspace) -> Self {
        Store { records }
    }
```

And remove the `db` field since we won't need it:

Change struct to:
```rust
pub struct Store {
    /// Records keyed by: {collection}\0{did}\0{rkey}
    records: Keyspace,
}
```

Remove `#[allow(dead_code)]` and `db` field.

**Step 3: Update RepoStore to support from_partition**

In `src/repos.rs`, replace the `open` and `open_with_db` methods with:

```rust
    pub fn open(path: &Path) -> Result<Self, RepoError> {
        let db = fjall::Config::new(path).open()?;
        let repos = db.open_partition("repos", PartitionCreateOptions::default())?;
        Ok(RepoStore { repos })
    }

    pub fn from_partition(repos: Keyspace) -> Self {
        RepoStore { repos }
    }
```

Add to imports at top: `use fjall::PartitionCreateOptions;`

**Step 4: Add mod declaration to main.rs**

In `src/main.rs`, add after `mod config;`:

```rust
mod app;
```

**Step 5: Verify it compiles**

Run: `cargo build`
Expected: Compiles (warnings about unused ok)

**Step 6: Commit**

```bash
git add src/app.rs src/storage.rs src/repos.rs src/main.rs
git commit -m "feat: add unified AppState with shared database"
```

---

### Task 5: Implement Sync Queue and Worker

**Files:**
- Create: `src/sync_worker.rs`
- Modify: `src/main.rs` (add mod declaration)
- Modify: `src/sync.rs` (extract sync logic, add rev tracking)

**Step 1: Update sync.rs to return rev and accept collection filter**

Modify `src/sync.rs` to change the return type and add filtering:

At the top, add import:
```rust
use crate::config::matches_any_filter;
```

Change `sync_repo` signature and add a new function:

```rust
pub struct SyncResult {
    pub record_count: usize,
    pub rev: Option<String>,
}

/// Sync a repo by DID with optional collection filtering
pub fn sync_repo_filtered(
    did: &str,
    store: &Arc<Store>,
    collections: &[String],
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

    // Process CAR using repo-stream
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(process_car_filtered(&car_bytes, did, store, collections))?;

    Ok(result)
}

/// Original function for backwards compatibility
pub fn sync_repo(did: &str, store: &Arc<Store>) -> Result<usize, SyncError> {
    let result = sync_repo_filtered(did, store, &[])?;
    Ok(result.record_count)
}
```

Update `process_car` to `process_car_filtered`:

```rust
async fn process_car_filtered(
    car_bytes: &[u8],
    did: &str,
    store: &Arc<Store>,
    collections: &[String],
) -> Result<SyncResult, SyncError> {
    let cursor = Cursor::new(car_bytes);
    let did = did.to_string();
    let store = Arc::clone(store);
    let collections = collections.to_vec();

    let driver = DriverBuilder::new()
        .with_mem_limit_mb(100)
        .with_block_processor(|data| data.to_vec())
        .load_car(cursor)
        .await
        .map_err(|e| SyncError::Car(e.to_string()))?;

    let mut count = 0;
    let mut rev = None;

    match driver {
        repo_stream::Driver::Memory(commit, mut driver) => {
            // Capture the rev from commit
            rev = Some(commit.rev);

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
                                    if store.put(&uri, &record).is_ok() {
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
```

**Step 2: Create sync_worker module**

Create `src/sync_worker.rs`:

```rust
use crate::app::AppState;
use crate::repos::{RepoState, RepoStatus};
use crate::sync::sync_repo_filtered;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

const RETRY_BASE_SECS: u64 = 60;
const RETRY_MAX_SECS: u64 = 3600;

pub struct SyncWorker {
    app: Arc<AppState>,
    queue_rx: mpsc::Receiver<String>,
    parallelism: u32,
}

pub struct SyncHandle {
    queue_tx: mpsc::Sender<String>,
}

impl SyncHandle {
    pub async fn queue(&self, did: String) -> Result<(), mpsc::error::SendError<String>> {
        self.queue_tx.send(did).await
    }

    pub fn queue_blocking(&self, did: String) -> Result<(), mpsc::error::SendError<String>> {
        self.queue_tx.blocking_send(did)
    }
}

pub fn start_sync_worker(app: Arc<AppState>) -> SyncHandle {
    let config = app.config();
    let (queue_tx, queue_rx) = mpsc::channel(1000);

    let worker = SyncWorker {
        app,
        queue_rx,
        parallelism: config.sync_parallelism,
    };

    tokio::spawn(worker.run());

    SyncHandle { queue_tx }
}

impl SyncWorker {
    async fn run(mut self) {
        // Spawn retry checker
        let app_clone = Arc::clone(&self.app);
        tokio::spawn(async move {
            retry_loop(app_clone).await;
        });

        // Process queue
        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.parallelism as usize));

        while let Some(did) = self.queue_rx.recv().await {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let app = Arc::clone(&self.app);

            tokio::spawn(async move {
                sync_one_repo(&app, &did).await;
                drop(permit);
            });
        }
    }
}

async fn sync_one_repo(app: &AppState, did: &str) {
    // Update status to syncing
    if let Ok(Some(mut state)) = app.repos.get(did) {
        state.status = RepoStatus::Syncing;
        state.error = None;
        let _ = app.repos.put(&state);
    }

    let config = app.config();
    let store = Arc::new(app.store.clone());

    // Run sync in blocking task
    let did_owned = did.to_string();
    let collections = config.collections.clone();

    let result = tokio::task::spawn_blocking(move || {
        sync_repo_filtered(&did_owned, &store, &collections)
    })
    .await;

    // Update state based on result
    if let Ok(Some(mut state)) = app.repos.get(did) {
        match result {
            Ok(Ok(sync_result)) => {
                state.status = RepoStatus::Synced;
                state.rev = sync_result.rev;
                state.record_count = sync_result.record_count as u64;
                state.last_sync = Some(now_secs());
                state.error = None;
                state.retry_count = 0;
                state.next_retry = None;
            }
            Ok(Err(e)) | Err(e) => {
                state.status = RepoStatus::Error;
                state.error = Some(format!("{}", e));
                state.retry_count += 1;
                state.next_retry = Some(calculate_next_retry(state.retry_count));
            }
        }
        let _ = app.repos.put(&state);
    }
}

async fn retry_loop(app: Arc<AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        let now = now_secs();
        if let Ok(errored) = app.repos.list_by_status(RepoStatus::Error) {
            for repo in errored {
                if let Some(next_retry) = repo.next_retry {
                    if next_retry <= now {
                        // Re-queue for sync
                        let mut state = repo.clone();
                        state.status = RepoStatus::Pending;
                        let _ = app.repos.put(&state);
                        // Note: would need SyncHandle to actually queue
                        // For now, the main loop will pick up Pending repos
                    }
                }
            }
        }
    }
}

fn calculate_next_retry(retry_count: u32) -> u64 {
    let delay = RETRY_BASE_SECS * 2u64.pow(retry_count.min(6));
    let delay = delay.min(RETRY_MAX_SECS);
    now_secs() + delay
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
```

**Step 3: Add mod declaration**

In `src/main.rs`, add after `mod app;`:

```rust
mod sync_worker;
```

**Step 4: Make Store cloneable**

In `src/storage.rs`, add Clone derive:

```rust
#[derive(Clone)]
pub struct Store {
    records: Keyspace,
}
```

**Step 5: Verify it compiles**

Run: `cargo build`
Expected: Compiles (may have warnings)

**Step 6: Commit**

```bash
git add src/sync.rs src/sync_worker.rs src/storage.rs src/main.rs
git commit -m "feat: add sync worker with queue and retry logic"
```

---

### Task 6: Add HTTP Endpoints for Repos

**Files:**
- Modify: `src/server.rs` (add /repos/* endpoints)

**Step 1: Add request/response types**

In `src/server.rs`, add after existing types:

```rust
use crate::app::AppState;
use crate::repos::{RepoState, RepoStatus};
use crate::sync_worker::{start_sync_worker, SyncHandle};

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
```

**Step 2: Add handler functions**

Add handlers:

```rust
async fn repos_add(
    State((app, sync_handle)): State<(Arc<AppState>, Arc<SyncHandle>)>,
    Json(payload): Json<ReposAddRequest>,
) -> Result<Json<ReposAddResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut queued = 0;

    for did in payload.dids {
        // Create pending state
        let state = RepoState::new(did.clone());
        app.repos.put(&state).map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
        })?;

        // Queue for sync
        sync_handle.queue(did).await.map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
        })?;

        queued += 1;
    }

    Ok(Json(ReposAddResponse { queued }))
}

async fn repos_remove(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
    Json(payload): Json<ReposRemoveRequest>,
) -> Result<Json<ReposRemoveResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut removed = 0;

    for did in payload.dids {
        if app.repos.contains(&did).unwrap_or(false) {
            app.repos.delete(&did).map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
            })?;
            removed += 1;
        }
    }

    Ok(Json(ReposRemoveResponse { removed }))
}

async fn repos_list(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
) -> Result<Json<ReposListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let repos = app.repos.list().map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
    })?;

    let repos: Vec<RepoStateJson> = repos.into_iter().map(Into::into).collect();
    Ok(Json(ReposListResponse { repos }))
}

async fn repos_get(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
    axum::extract::Path(did): axum::extract::Path<String>,
) -> Result<Json<RepoStateJson>, (StatusCode, Json<ErrorResponse>)> {
    let state = app.repos.get(&did).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
    })?;

    match state {
        Some(s) => Ok(Json(s.into())),
        None => Err((StatusCode::NOT_FOUND, Json(ErrorResponse { error: "repo not found".to_string() }))),
    }
}

async fn repos_errors(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
) -> Result<Json<ReposListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let repos = app.repos.list_by_status(RepoStatus::Error).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
    })?;

    let repos: Vec<RepoStateJson> = repos.into_iter().map(Into::into).collect();
    Ok(Json(ReposListResponse { repos }))
}

async fn repos_resync(
    State((app, sync_handle)): State<(Arc<AppState>, Arc<SyncHandle>)>,
    Json(payload): Json<ReposResyncRequest>,
) -> Result<Json<ReposResyncResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut queued = 0;

    for did in payload.dids {
        if let Ok(Some(mut state)) = app.repos.get(&did) {
            state.status = RepoStatus::Pending;
            state.error = None;
            app.repos.put(&state).map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
            })?;

            sync_handle.queue(did).await.map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
            })?;

            queued += 1;
        }
    }

    Ok(Json(ReposResyncResponse { queued }))
}
```

**Step 3: Update router**

Update the `run` function to use new state and routes. This is a larger refactor - the function signature and body change significantly. Replace the entire `run` function:

```rust
pub async fn run(app: Arc<AppState>, port: u16, relay: Option<String>) {
    let running = Arc::new(AtomicBool::new(false));

    // Start sync worker
    let sync_handle = Arc::new(start_sync_worker(Arc::clone(&app)));

    // Start firehose if relay specified
    if let Some(relay) = relay {
        start_firehose(relay, Arc::clone(&app), Arc::clone(&running));
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let state = (app, sync_handle);

    let app = Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/query", post(query_handler))
        .route("/sync", post(sync_handler))
        .route("/repos", get(repos_list))
        .route("/repos/add", post(repos_add))
        .route("/repos/remove", post(repos_remove))
        .route("/repos/errors", get(repos_errors))
        .route("/repos/resync", post(repos_resync))
        .route("/repos/:did", get(repos_get))
        .layer(cors)
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    println!("Starting server on http://localhost:{}", port);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();

    running.store(false, Ordering::Relaxed);
}
```

Update the existing handlers to use the new state type. Change `State(store)` to `State((app, _))` and use `app.store` instead of `store`:

```rust
async fn stats(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
) -> Result<Json<StatsResponse>, StatusCode> {
    match app.store.count() {
        Ok(count) => Ok(Json(StatsResponse { records: count })),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn query_handler(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
    Json(payload): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    // ... use &app.store instead of &store
}

async fn sync_handler(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
    Json(payload): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, (StatusCode, Json<ErrorResponse>)> {
    let store = Arc::new(app.store.clone());
    // ... rest same but use store
}
```

Also update `start_firehose` to take `AppState`:

```rust
fn start_firehose(relay: String, app: Arc<AppState>, running: Arc<AtomicBool>) {
    // ... use app.store instead of store
}
```

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: Compiles

**Step 5: Commit**

```bash
git add src/server.rs
git commit -m "feat: add HTTP endpoints for repo management"
```

---

### Task 7: Add CLI Commands for Repos

**Files:**
- Modify: `src/main.rs` (add .repos commands)

**Step 1: Refactor main.rs to use AppState**

This is a significant refactor. Update `main.rs` to:
1. Use AppState instead of Store
2. Add .repos and .config commands
3. Update handle_command signature

Update the imports at the top:

```rust
mod app;
mod cbor;
mod config;
mod firehose;
mod indexer;
mod query;
mod repos;
mod server;
mod storage;
mod sync;
mod sync_worker;
mod types;

use app::AppState;
use config::Mode;
use firehose::{Event, FirehoseClient, Operation};
use query::Query;
use repos::RepoState;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
```

Update `handle_command` to take `AppState` and add new commands:

```rust
fn handle_command(
    line: &str,
    app: &Arc<AppState>,
    running: &Arc<AtomicBool>,
) -> CommandResult {
    match line {
        ".quit" | ".exit" => CommandResult::Exit,

        ".help" => {
            println!("Commands:");
            println!("  .connect <relay>  Connect to firehose (default: bsky.network)");
            println!("  .disconnect       Stop firehose");
            println!("  .stats            Show statistics");
            println!("  .dids             List unique DIDs");
            println!("  .collections      List unique collections");
            println!();
            println!("Repo management:");
            println!("  .repos                    List tracked repos");
            println!("  .repos add <did> [...]    Add repos to track");
            println!("  .repos remove <did>       Remove tracked repo");
            println!("  .repos status <did>       Show repo status");
            println!("  .repos errors             List errored repos");
            println!("  .repos resync <did>       Force resync");
            println!("  .repos resync --errors    Resync all errored");
            println!();
            println!("Config:");
            println!("  .config                   Show current config");
            println!("  .config mode <mode>       Set mode (manual|signal|full-network)");
            println!("  .config signal <nsid>     Set signal collection");
            println!("  .config collections <..>  Set collection filters");
            println!();
            println!("Queries:");
            println!("  at://did/collection/rkey   Get single record");
            println!("  at://did/collection/*      Get all records in collection for DID");
            println!("  at://*/collection/*        Get all records in collection (all DIDs)");
            println!("  at://did?sync              Sync a user's repo from their PDS");
            println!();
            println!("  .quit             Exit");
            CommandResult::Continue
        }

        ".stats" => {
            match app.store.count() {
                Ok(count) => println!("Records: {}", count),
                Err(e) => println!("Error: {}", e),
            }
            CommandResult::Continue
        }

        ".dids" => {
            match app.store.unique_dids() {
                Ok(dids) => {
                    println!("Unique DIDs ({}):", dids.len());
                    for did in dids {
                        println!("  {}", did);
                    }
                }
                Err(e) => println!("Error: {}", e),
            }
            CommandResult::Continue
        }

        ".collections" => {
            match app.store.unique_collections() {
                Ok(cols) => {
                    println!("Unique collections ({}):", cols.len());
                    for col in cols {
                        println!("  {}", col);
                    }
                }
                Err(e) => println!("Error: {}", e),
            }
            CommandResult::Continue
        }

        ".disconnect" => {
            running.store(false, Ordering::Relaxed);
            println!("Disconnecting...");
            CommandResult::Continue
        }

        ".config" => {
            let config = app.config();
            println!("Mode: {}", config.mode);
            if let Some(signal) = &config.signal_collection {
                println!("Signal collection: {}", signal);
            }
            if !config.collections.is_empty() {
                println!("Collection filters: {}", config.collections.join(", "));
            }
            println!("Relay: {}", config.relay);
            println!("Sync parallelism: {}", config.sync_parallelism);
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".config ") => {
            let parts: Vec<&str> = cmd.strip_prefix(".config ").unwrap().splitn(2, ' ').collect();
            if parts.is_empty() {
                println!("Usage: .config <key> <value>");
                return CommandResult::Continue;
            }

            match parts[0] {
                "mode" => {
                    if parts.len() < 2 {
                        println!("Usage: .config mode <manual|signal|full-network>");
                        return CommandResult::Continue;
                    }
                    let mode = match parts[1] {
                        "manual" => Mode::Manual,
                        "signal" => Mode::Signal,
                        "full-network" => Mode::FullNetwork,
                        _ => {
                            println!("Invalid mode. Use: manual, signal, full-network");
                            return CommandResult::Continue;
                        }
                    };
                    if let Err(e) = app.update_config(|c| c.mode = mode) {
                        println!("Error: {}", e);
                    } else {
                        println!("Mode set to {}", mode);
                    }
                }
                "signal" => {
                    if parts.len() < 2 {
                        println!("Usage: .config signal <collection>");
                        return CommandResult::Continue;
                    }
                    let signal = parts[1].to_string();
                    if let Err(e) = app.update_config(|c| c.signal_collection = Some(signal.clone())) {
                        println!("Error: {}", e);
                    } else {
                        println!("Signal collection set to {}", signal);
                    }
                }
                "collections" => {
                    if parts.len() < 2 {
                        println!("Usage: .config collections <col1,col2,...>");
                        return CommandResult::Continue;
                    }
                    let collections: Vec<String> = parts[1].split(',').map(|s| s.trim().to_string()).collect();
                    if let Err(e) = app.update_config(|c| c.collections = collections.clone()) {
                        println!("Error: {}", e);
                    } else {
                        println!("Collection filters set to: {}", collections.join(", "));
                    }
                }
                _ => {
                    println!("Unknown config key: {}", parts[0]);
                }
            }
            CommandResult::Continue
        }

        ".repos" => {
            match app.repos.list() {
                Ok(repos) => {
                    if repos.is_empty() {
                        println!("No tracked repos");
                    } else {
                        println!("Tracked repos ({}):", repos.len());
                        for repo in repos {
                            println!("  {} [{}] {} records", repo.did, repo.status, repo.record_count);
                        }
                    }
                }
                Err(e) => println!("Error: {}", e),
            }
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".repos ") => {
            let rest = cmd.strip_prefix(".repos ").unwrap();
            let parts: Vec<&str> = rest.split_whitespace().collect();

            match parts.first().copied() {
                Some("add") => {
                    for did in &parts[1..] {
                        let state = RepoState::new(did.to_string());
                        match app.repos.put(&state) {
                            Ok(()) => println!("Added {} (pending sync)", did),
                            Err(e) => println!("Error adding {}: {}", did, e),
                        }
                    }
                }
                Some("remove") => {
                    if parts.len() < 2 {
                        println!("Usage: .repos remove <did>");
                    } else {
                        let did = parts[1];
                        match app.repos.delete(did) {
                            Ok(()) => println!("Removed {}", did),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                }
                Some("status") => {
                    if parts.len() < 2 {
                        println!("Usage: .repos status <did>");
                    } else {
                        let did = parts[1];
                        match app.repos.get(did) {
                            Ok(Some(state)) => {
                                println!("DID: {}", state.did);
                                println!("Status: {}", state.status);
                                if let Some(rev) = &state.rev {
                                    println!("Rev: {}", rev);
                                }
                                println!("Records: {}", state.record_count);
                                if let Some(ts) = state.last_sync {
                                    println!("Last sync: {}", ts);
                                }
                                if let Some(err) = &state.error {
                                    println!("Error: {}", err);
                                    println!("Retry count: {}", state.retry_count);
                                }
                            }
                            Ok(None) => println!("Repo not found"),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                }
                Some("errors") => {
                    match app.repos.list_by_status(repos::RepoStatus::Error) {
                        Ok(repos) => {
                            if repos.is_empty() {
                                println!("No errored repos");
                            } else {
                                println!("Errored repos ({}):", repos.len());
                                for repo in repos {
                                    println!("  {} - {}", repo.did, repo.error.unwrap_or_default());
                                }
                            }
                        }
                        Err(e) => println!("Error: {}", e),
                    }
                }
                Some("resync") => {
                    if parts.len() < 2 {
                        println!("Usage: .repos resync <did> or .repos resync --errors");
                    } else if parts[1] == "--errors" {
                        match app.repos.list_by_status(repos::RepoStatus::Error) {
                            Ok(repos) => {
                                for mut repo in repos {
                                    repo.status = repos::RepoStatus::Pending;
                                    repo.error = None;
                                    let _ = app.repos.put(&repo);
                                    println!("Queued {} for resync", repo.did);
                                }
                            }
                            Err(e) => println!("Error: {}", e),
                        }
                    } else {
                        let did = parts[1];
                        match app.repos.get(did) {
                            Ok(Some(mut state)) => {
                                state.status = repos::RepoStatus::Pending;
                                state.error = None;
                                let _ = app.repos.put(&state);
                                println!("Queued {} for resync", did);
                            }
                            Ok(None) => println!("Repo not found"),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                }
                _ => {
                    println!("Unknown repos command. Try .repos add/remove/status/errors/resync");
                }
            }
            CommandResult::Continue
        }

        cmd if cmd.starts_with(".connect") => {
            let relay = cmd
                .strip_prefix(".connect")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("bsky.network");

            if running.load(Ordering::Relaxed) {
                println!("Already connected. Use .disconnect first.");
                return CommandResult::Continue;
            }

            CommandResult::StartStreaming(relay.to_string())
        }

        line if line.starts_with("at://") => {
            if line.contains("?sync") {
                let uri_part = line.strip_prefix("at://").unwrap();
                let did = uri_part.split('?').next().unwrap();

                println!("Syncing {}...", did);
                let start = std::time::Instant::now();
                let store = Arc::new(app.store.clone());
                match sync::sync_repo(did, &store) {
                    Ok(count) => {
                        let elapsed = start.elapsed();
                        println!("Synced {} records ({:.2?})", count, elapsed);
                    }
                    Err(e) => println!("Sync error: {}", e),
                }
                return CommandResult::Continue;
            }

            match Query::parse(line) {
                Ok(q) => {
                    let start = std::time::Instant::now();
                    match query::execute(&q, &app.store) {
                        Ok(records) => {
                            let elapsed = start.elapsed();
                            for record in &records {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&record.value).unwrap()
                                );
                                println!("---");
                            }
                            println!("({} records, {:.2?})", records.len(), elapsed);
                        }
                        Err(e) => println!("Query error: {}", e),
                    }
                }
                Err(e) => println!("Parse error: {}", e),
            }
            CommandResult::Continue
        }

        _ => {
            println!("Unknown command. Type .help for help.");
            CommandResult::Continue
        }
    }
}
```

Update `start_streaming` to use `AppState`:

```rust
fn start_streaming(relay: String, app: Arc<AppState>, running: Arc<AtomicBool>) {
    running.store(true, Ordering::Relaxed);

    thread::spawn(move || {
        match FirehoseClient::connect(&relay) {
            Ok(mut client) => {
                println!("Connected to {}", relay);

                while running.load(Ordering::Relaxed) {
                    match client.next_event() {
                        Ok(Some(Event::Commit { did: _, operations })) => {
                            for op in operations {
                                match op {
                                    Operation::Create { uri, cid, value } => {
                                        let record = storage::Record {
                                            uri: uri.to_string(),
                                            cid,
                                            value,
                                            indexed_at: SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap()
                                                .as_secs(),
                                        };
                                        if let Err(e) = app.store.put(&uri, &record) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                    }
                                    Operation::Delete { uri } => {
                                        if let Err(e) = app.store.delete(&uri) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Some(Event::Unknown)) => {}
                        Ok(None) => {
                            println!("Connection closed");
                            break;
                        }
                        Err(e) => {
                            eprintln!("Firehose error: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to connect: {}", e);
            }
        }
        running.store(false, Ordering::Relaxed);
    });
}
```

Update `main()` to use `AppState`:

```rust
fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Check for serve subcommand
    if args.len() > 1 && args[1] == "serve" {
        let mut port: u16 = 3000;
        let mut relay: Option<String> = None;

        for arg in &args[2..] {
            if let Some(p) = arg.strip_prefix("--port=") {
                port = p.parse().unwrap_or(3000);
            } else if arg == "--connect" {
                relay = Some("bsky.network".to_string());
            } else if let Some(r) = arg.strip_prefix("--connect=") {
                relay = Some(r.to_string());
            }
        }

        let app = match AppState::open(Path::new("./atpdb.data")) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                eprintln!("Failed to open storage: {}", e);
                return;
            }
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(server::run(app, port, relay));
        return;
    }

    // Non-interactive mode
    if args.len() > 1 {
        let app = match AppState::open(Path::new("./atpdb.data")) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                eprintln!("Failed to open storage: {}", e);
                return;
            }
        };
        let running = Arc::new(AtomicBool::new(false));

        let command = args[1..].join(" ");
        handle_command(&command, &app, &running);
        return;
    }

    // Interactive mode
    println!(
        r#"
   ___  ______ ___  ___  ___
  / _ |/_  __// _ \/ _ \/ _ )
 / __ | / /  / ___/ // / _  |
/_/ |_|/_/  /_/  /____/____/
"#
    );

    let app = match AppState::open(Path::new("./atpdb.data")) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("Failed to open storage: {}", e);
            return;
        }
    };

    let running = Arc::new(AtomicBool::new(false));
    let mut rl = DefaultEditor::new().unwrap();

    loop {
        let prompt = if running.load(Ordering::Relaxed) {
            "atp> (streaming) "
        } else {
            "atp> "
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let _ = rl.add_history_entry(line);

                match handle_command(line, &app, &running) {
                    CommandResult::Continue => {}
                    CommandResult::Exit => break,
                    CommandResult::StartStreaming(relay) => {
                        start_streaming(relay, Arc::clone(&app), Arc::clone(&running));
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("^C");
            }
            Err(ReadlineError::Eof) => {
                break;
            }
            Err(e) => {
                println!("Error: {:?}", e);
                break;
            }
        }
    }

    running.store(false, Ordering::Relaxed);
    println!("Goodbye!");
}
```

**Step 2: Verify it compiles**

Run: `cargo build`
Expected: Compiles

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add CLI commands for repos and config management"
```

---

### Task 8: Integrate Mode-Based Firehose Routing

**Files:**
- Modify: `src/firehose.rs` (add rev to Event)
- Modify: `src/server.rs` (update firehose handler with mode routing)

**Step 1: Add rev to commit Event**

In `src/firehose.rs`, update the Event enum and decode_message to include rev:

```rust
#[derive(Debug)]
pub enum Event {
    Commit {
        #[allow(dead_code)]
        did: Did,
        rev: String,
        operations: Vec<Operation>,
    },
    Unknown,
}
```

Update `CommitBody` to include rev:

```rust
#[derive(Debug, Deserialize)]
struct CommitBody {
    repo: String,
    rev: String,
    ops: Vec<RepoOp>,
    #[serde(with = "serde_bytes")]
    blocks: Vec<u8>,
}
```

Update decode_message to return rev:

```rust
Ok(Some(Event::Commit { did, rev: body.rev, operations }))
```

**Step 2: Update firehose handler in server.rs**

Update `start_firehose` to handle modes and collection filtering:

```rust
fn start_firehose(relay: String, app: Arc<AppState>, running: Arc<AtomicBool>) {
    running.store(true, Ordering::Relaxed);

    std::thread::spawn(move || {
        match FirehoseClient::connect(&relay) {
            Ok(mut client) => {
                println!("Connected to firehose: {}", relay);

                while running.load(Ordering::Relaxed) {
                    match client.next_event() {
                        Ok(Some(Event::Commit { did, rev: _, operations })) => {
                            let config = app.config();
                            let did_str = did.to_string();

                            // Check if we should process this repo based on mode
                            let is_tracked = app.repos.contains(&did_str).unwrap_or(false);

                            let should_process = match config.mode {
                                Mode::Manual => is_tracked,
                                Mode::Signal => {
                                    if is_tracked {
                                        true
                                    } else {
                                        // Check if any op matches signal collection
                                        let matches_signal = config.signal_collection.as_ref().map_or(false, |signal| {
                                            operations.iter().any(|op| {
                                                let collection = match op {
                                                    Operation::Create { uri, .. } => uri.collection.as_str(),
                                                    Operation::Delete { uri } => uri.collection.as_str(),
                                                };
                                                crate::config::matches_collection_filter(collection, signal)
                                            })
                                        });
                                        if matches_signal {
                                            // Auto-add this repo
                                            let state = RepoState::new(did_str.clone());
                                            let _ = app.repos.put(&state);
                                            println!("Auto-tracking new repo: {}", did_str);
                                            true
                                        } else {
                                            false
                                        }
                                    }
                                }
                                Mode::FullNetwork => {
                                    if !is_tracked {
                                        let state = RepoState::new(did_str.clone());
                                        let _ = app.repos.put(&state);
                                    }
                                    true
                                }
                            };

                            if !should_process {
                                continue;
                            }

                            for op in operations {
                                // Apply collection filter
                                let collection = match &op {
                                    Operation::Create { uri, .. } => uri.collection.as_str(),
                                    Operation::Delete { uri } => uri.collection.as_str(),
                                };

                                if !crate::config::matches_any_filter(collection, &config.collections) {
                                    continue;
                                }

                                match op {
                                    Operation::Create { uri, cid, value } => {
                                        let record = Record {
                                            uri: uri.to_string(),
                                            cid,
                                            value,
                                            indexed_at: SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap()
                                                .as_secs(),
                                        };
                                        if let Err(e) = app.store.put(&uri, &record) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                    }
                                    Operation::Delete { uri } => {
                                        if let Err(e) = app.store.delete(&uri) {
                                            eprintln!("Storage error: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Some(Event::Unknown)) => {}
                        Ok(None) => {
                            println!("Firehose connection closed");
                            break;
                        }
                        Err(e) => {
                            eprintln!("Firehose error: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to connect to firehose: {}", e);
            }
        }
        running.store(false, Ordering::Relaxed);
    });
}
```

Add required imports at top of server.rs:

```rust
use crate::config::Mode;
use crate::repos::RepoState;
```

**Step 3: Verify it compiles**

Run: `cargo build`
Expected: Compiles

**Step 4: Commit**

```bash
git add src/firehose.rs src/server.rs
git commit -m "feat: add mode-based firehose routing with collection filtering"
```

---

### Task 9: Add Config HTTP Endpoints

**Files:**
- Modify: `src/server.rs` (add /config endpoints)

**Step 1: Add config types and handlers**

Add to server.rs:

```rust
#[derive(Deserialize)]
pub struct ConfigUpdateRequest {
    mode: Option<String>,
    signal_collection: Option<String>,
    collections: Option<Vec<String>>,
    relay: Option<String>,
    sync_parallelism: Option<u32>,
}

#[derive(Serialize)]
pub struct ConfigResponse {
    mode: String,
    signal_collection: Option<String>,
    collections: Vec<String>,
    relay: String,
    sync_parallelism: u32,
}

async fn config_get(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
) -> Json<ConfigResponse> {
    let config = app.config();
    Json(ConfigResponse {
        mode: config.mode.to_string(),
        signal_collection: config.signal_collection,
        collections: config.collections,
        relay: config.relay,
        sync_parallelism: config.sync_parallelism,
    })
}

async fn config_update(
    State((app, _)): State<(Arc<AppState>, Arc<SyncHandle>)>,
    Json(payload): Json<ConfigUpdateRequest>,
) -> Result<Json<ConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
    app.update_config(|config| {
        if let Some(mode) = &payload.mode {
            config.mode = match mode.as_str() {
                "manual" => Mode::Manual,
                "signal" => Mode::Signal,
                "full-network" => Mode::FullNetwork,
                _ => config.mode,
            };
        }
        if let Some(signal) = payload.signal_collection.clone() {
            config.signal_collection = Some(signal);
        }
        if let Some(collections) = payload.collections.clone() {
            config.collections = collections;
        }
        if let Some(relay) = payload.relay.clone() {
            config.relay = relay;
        }
        if let Some(parallelism) = payload.sync_parallelism {
            config.sync_parallelism = parallelism;
        }
    }).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() }))
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
```

Add routes in run():

```rust
        .route("/config", get(config_get))
        .route("/config", post(config_update))
```

**Step 2: Verify it compiles**

Run: `cargo build`
Expected: Compiles

**Step 3: Commit**

```bash
git add src/server.rs
git commit -m "feat: add HTTP endpoints for config management"
```

---

### Task 10: Manual Integration Test

**Files:** None (testing only)

**Step 1: Build and run**

```bash
cargo build --release
./target/release/atpdb serve --port=3001 &
```

**Step 2: Test repos API**

```bash
# Add a repo
curl -X POST http://localhost:3001/repos/add \
  -H "Content-Type: application/json" \
  -d '{"dids": ["did:plc:ewvi7nxzyoun6zhxrhs64oiz"]}'

# List repos
curl http://localhost:3001/repos

# Get repo status
curl http://localhost:3001/repos/did:plc:ewvi7nxzyoun6zhxrhs64oiz
```

**Step 3: Test config API**

```bash
# Get config
curl http://localhost:3001/config

# Update config
curl -X POST http://localhost:3001/config \
  -H "Content-Type: application/json" \
  -d '{"mode": "signal", "signal_collection": "fm.teal.alpha.feed", "collections": ["fm.teal.*"]}'
```

**Step 4: Stop server and commit**

```bash
pkill atpdb
git add -A
git commit -m "test: verify backfill system integration"
```

---

## Summary

This plan implements the backfill system in 10 tasks:

1. **RepoState types** - Basic data structures
2. **RepoStore** - Persistence for repo state
3. **Config types** - Mode, filters, collection matching
4. **AppState** - Unified state with shared database
5. **Sync Worker** - Background queue with retry logic
6. **HTTP /repos/** - API for repo management
7. **CLI .repos** - Commands for repo management
8. **Firehose routing** - Mode-based filtering
9. **HTTP /config** - API for config management
10. **Integration test** - Verify it all works

Each task is self-contained with tests and commits.
