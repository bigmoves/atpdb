# Crawler Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Auto-discover repos from relay using `listRepos` and `listReposByCollection` APIs based on configured mode.

**Architecture:** A crawler module that calls relay sync APIs to enumerate DIDs, persists pagination cursors for resumability, and queues discovered repos for backfill. Runs automatically on server startup based on mode (signal → listReposByCollection, full-network → listRepos).

**Tech Stack:** reqwest (blocking HTTP), serde for JSON parsing, fjall for cursor persistence

---

### Task 1: Add Crawler Types and Cursor Storage

**Files:**
- Create: `src/crawler.rs`
- Modify: `src/main.rs` (add mod declaration)
- Modify: `src/app.rs` (add crawler_cursors keyspace)

**Step 1: Create crawler.rs with types**

```rust
use crate::app::AppState;
use crate::repos::{RepoState, RepoStatus};
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CrawlerError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("storage error: {0}")]
    Storage(#[from] fjall::Error),
}

#[derive(Debug, Deserialize)]
pub struct ListReposResponse {
    pub cursor: Option<String>,
    pub repos: Vec<RepoInfo>,
}

#[derive(Debug, Deserialize)]
pub struct RepoInfo {
    pub did: String,
    pub head: String,
    #[serde(default)]
    pub active: Option<bool>,
}

pub struct Crawler {
    app: Arc<AppState>,
    client: reqwest::blocking::Client,
}
```

**Step 2: Add mod declaration to main.rs**

Add `mod crawler;` to the module list in `src/main.rs`.

**Step 3: Add crawler_cursors keyspace to AppState**

In `src/app.rs`, add:
```rust
pub struct AppState {
    pub store: Store,
    pub repos: RepoStore,
    config: RwLock<Config>,
    config_keyspace: fjall::Keyspace,
    crawler_cursors: fjall::Keyspace,  // NEW
}
```

Update `AppState::open` to create the keyspace:
```rust
let crawler_cursors = db.keyspace("crawler_cursors", KeyspaceCreateOptions::default)?;
```

And add a getter/setter for cursors:
```rust
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
```

**Step 4: Verify it compiles**

Run: `cargo build`

**Step 5: Commit**

```bash
git add src/crawler.rs src/main.rs src/app.rs
git commit -m "feat: add crawler types and cursor storage"
```

---

### Task 2: Implement listRepos Enumeration

**Files:**
- Modify: `src/crawler.rs`

**Step 1: Add enumerate_all method**

```rust
impl Crawler {
    pub fn new(app: Arc<AppState>) -> Self {
        Crawler {
            app,
            client: reqwest::blocking::Client::new(),
        }
    }

    /// Enumerate all repos using com.atproto.sync.listRepos
    pub fn enumerate_all(&self) -> Result<usize, CrawlerError> {
        let config = self.app.config();
        let relay = &config.relay;
        let cursor_key = format!("listRepos:{}", relay);

        let mut cursor = self.app.get_cursor(&cursor_key);
        let mut total_discovered = 0;

        loop {
            let url = match &cursor {
                Some(c) => format!(
                    "https://{}/xrpc/com.atproto.sync.listRepos?limit=1000&cursor={}",
                    relay, c
                ),
                None => format!(
                    "https://{}/xrpc/com.atproto.sync.listRepos?limit=1000",
                    relay
                ),
            };

            println!("Crawling: {}", url);
            let response: ListReposResponse = self.client.get(&url).send()?.json()?;

            for repo in &response.repos {
                // Skip inactive repos
                if repo.active == Some(false) {
                    continue;
                }

                // Add if not already tracked
                if !self.app.repos.contains(&repo.did).unwrap_or(true) {
                    let state = RepoState::new(repo.did.clone());
                    let _ = self.app.repos.put(&state);
                    total_discovered += 1;
                }
            }

            // Save cursor for resumability
            if let Some(ref c) = response.cursor {
                let _ = self.app.set_cursor(&cursor_key, c);
                cursor = Some(c.clone());
            } else {
                // Done - clear cursor for next full run
                let _ = self.app.set_cursor(&cursor_key, "");
                break;
            }

            // Rate limit
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        Ok(total_discovered)
    }
}
```

**Step 2: Verify it compiles**

Run: `cargo build`

**Step 3: Commit**

```bash
git add src/crawler.rs
git commit -m "feat: implement listRepos enumeration"
```

---

### Task 3: Implement listReposByCollection Enumeration

**Files:**
- Modify: `src/crawler.rs`

**Step 1: Add enumerate_by_collection method**

```rust
impl Crawler {
    // ... existing methods ...

    /// Enumerate repos by collection using com.atproto.sync.listReposByCollection
    pub fn enumerate_by_collection(&self, collection: &str) -> Result<usize, CrawlerError> {
        let config = self.app.config();
        let relay = &config.relay;
        let cursor_key = format!("listReposByCollection:{}:{}", relay, collection);

        let mut cursor = self.app.get_cursor(&cursor_key);
        let mut total_discovered = 0;

        loop {
            let url = match &cursor {
                Some(c) if !c.is_empty() => format!(
                    "https://{}/xrpc/com.atproto.sync.listReposByCollection?collection={}&limit=1000&cursor={}",
                    relay, collection, c
                ),
                _ => format!(
                    "https://{}/xrpc/com.atproto.sync.listReposByCollection?collection={}&limit=1000",
                    relay, collection
                ),
            };

            println!("Crawling by collection: {}", url);
            let response: ListReposResponse = self.client.get(&url).send()?.json()?;

            for repo in &response.repos {
                if repo.active == Some(false) {
                    continue;
                }

                if !self.app.repos.contains(&repo.did).unwrap_or(true) {
                    let state = RepoState::new(repo.did.clone());
                    let _ = self.app.repos.put(&state);
                    total_discovered += 1;
                }
            }

            if let Some(ref c) = response.cursor {
                if c.is_empty() {
                    break;
                }
                let _ = self.app.set_cursor(&cursor_key, c);
                cursor = Some(c.clone());
            } else {
                let _ = self.app.set_cursor(&cursor_key, "");
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        Ok(total_discovered)
    }
}
```

**Step 2: Verify it compiles**

Run: `cargo build`

**Step 3: Commit**

```bash
git add src/crawler.rs
git commit -m "feat: implement listReposByCollection enumeration"
```

---

### Task 4: Add run_based_on_mode and Background Startup

**Files:**
- Modify: `src/crawler.rs`
- Modify: `src/server.rs`

**Step 1: Add run_based_on_mode method to Crawler**

```rust
use crate::config::Mode;

impl Crawler {
    // ... existing methods ...

    /// Run crawler based on current config mode
    pub fn run_based_on_mode(&self) -> Result<usize, CrawlerError> {
        let config = self.app.config();

        match config.mode {
            Mode::Manual => {
                println!("Crawler: Manual mode, skipping enumeration");
                Ok(0)
            }
            Mode::Signal => {
                if let Some(ref collection) = config.signal_collection {
                    println!("Crawler: Signal mode, enumerating by collection: {}", collection);
                    self.enumerate_by_collection(collection)
                } else {
                    println!("Crawler: Signal mode but no signal_collection configured");
                    Ok(0)
                }
            }
            Mode::FullNetwork => {
                println!("Crawler: Full network mode, enumerating all repos");
                self.enumerate_all()
            }
        }
    }
}

/// Start crawler in background thread
pub fn start_crawler(app: Arc<AppState>) {
    std::thread::spawn(move || {
        let crawler = Crawler::new(app);
        match crawler.run_based_on_mode() {
            Ok(count) => println!("Crawler: discovered {} new repos", count),
            Err(e) => eprintln!("Crawler error: {}", e),
        }
    });
}
```

**Step 2: Start crawler on server startup in server.rs**

In `src/server.rs`, add import and call in `run()`:

```rust
use crate::crawler::start_crawler;

pub async fn run(app: Arc<AppState>, port: u16, relay: Option<String>) {
    let running = Arc::new(AtomicBool::new(false));

    // Start sync worker
    let sync_handle = Arc::new(start_sync_worker(Arc::clone(&app)));

    // Start crawler
    start_crawler(Arc::clone(&app));

    // ... rest unchanged ...
}
```

**Step 3: Verify it compiles**

Run: `cargo build`

**Step 4: Commit**

```bash
git add src/crawler.rs src/server.rs
git commit -m "feat: auto-start crawler based on config mode"
```

---

### Task 5: Queue Discovered Repos for Sync

**Files:**
- Modify: `src/crawler.rs`
- Modify: `src/server.rs`

**Step 1: Update Crawler to accept SyncHandle**

```rust
use crate::sync_worker::SyncHandle;

pub struct Crawler {
    app: Arc<AppState>,
    sync_handle: Arc<SyncHandle>,
    client: reqwest::blocking::Client,
}

impl Crawler {
    pub fn new(app: Arc<AppState>, sync_handle: Arc<SyncHandle>) -> Self {
        Crawler {
            app,
            sync_handle,
            client: reqwest::blocking::Client::new(),
        }
    }
```

**Step 2: Queue repos after discovery**

In both `enumerate_all` and `enumerate_by_collection`, after adding a new repo:

```rust
if !self.app.repos.contains(&repo.did).unwrap_or(true) {
    let state = RepoState::new(repo.did.clone());
    let _ = self.app.repos.put(&state);
    let _ = self.sync_handle.queue_blocking(repo.did.clone());
    total_discovered += 1;
}
```

**Step 3: Update start_crawler signature**

```rust
pub fn start_crawler(app: Arc<AppState>, sync_handle: Arc<SyncHandle>) {
    std::thread::spawn(move || {
        let crawler = Crawler::new(app, sync_handle);
        match crawler.run_based_on_mode() {
            Ok(count) => println!("Crawler: discovered {} new repos", count),
            Err(e) => eprintln!("Crawler error: {}", e),
        }
    });
}
```

**Step 4: Update server.rs call**

```rust
// Start crawler
start_crawler(Arc::clone(&app), Arc::clone(&sync_handle));
```

**Step 5: Verify it compiles**

Run: `cargo build`

**Step 6: Commit**

```bash
git add src/crawler.rs src/server.rs
git commit -m "feat: queue discovered repos for sync"
```

---

### Task 6: Add Crawler Status Endpoint

**Files:**
- Modify: `src/server.rs`
- Modify: `src/app.rs`

**Step 1: Add cursor listing to AppState**

In `src/app.rs`:

```rust
pub fn list_cursors(&self) -> Result<Vec<(String, String)>, AppError> {
    let mut cursors = Vec::new();
    for item in self.crawler_cursors.prefix(b"") {
        let key = String::from_utf8(item.key()?.to_vec()).unwrap_or_default();
        let value = String::from_utf8(item.value()?.to_vec()).unwrap_or_default();
        cursors.push((key, value));
    }
    Ok(cursors)
}
```

**Step 2: Add status endpoint to server.rs**

```rust
#[derive(Serialize)]
pub struct CrawlerStatusResponse {
    mode: String,
    cursors: Vec<CursorInfo>,
    tracked_repos: usize,
}

#[derive(Serialize)]
pub struct CursorInfo {
    key: String,
    cursor: String,
}

async fn crawler_status(
    State((app, _)): State<AppStateHandle>,
) -> Result<Json<CrawlerStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let config = app.config();
    let cursors = app.list_cursors().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse { error: e.to_string() }),
        )
    })?;

    let tracked = app.repos.list().map(|r| r.len()).unwrap_or(0);

    Ok(Json(CrawlerStatusResponse {
        mode: config.mode.to_string(),
        cursors: cursors.into_iter().map(|(k, v)| CursorInfo { key: k, cursor: v }).collect(),
        tracked_repos: tracked,
    }))
}
```

**Step 3: Add route**

```rust
.route("/crawler", get(crawler_status))
```

**Step 4: Verify it compiles**

Run: `cargo build`

**Step 5: Commit**

```bash
git add src/server.rs src/app.rs
git commit -m "feat: add crawler status endpoint"
```

---

### Task 7: Add CLI Crawler Status Command

**Files:**
- Modify: `src/main.rs`

**Step 1: Add .crawl command to handle_command**

```rust
".crawl" => {
    let config = app.config();
    println!("Mode: {}", config.mode);

    match app.list_cursors() {
        Ok(cursors) => {
            if cursors.is_empty() {
                println!("No crawler cursors");
            } else {
                println!("Crawler cursors:");
                for (key, value) in cursors {
                    let display = if value.is_empty() { "(complete)" } else { &value };
                    println!("  {}: {}", key, display);
                }
            }
        }
        Err(e) => println!("Error: {}", e),
    }

    match app.repos.list() {
        Ok(repos) => println!("Tracked repos: {}", repos.len()),
        Err(e) => println!("Error: {}", e),
    }

    CommandResult::Continue
}
```

**Step 2: Update .help to include .crawl**

Add to help text:
```rust
println!("  .crawl              Show crawler status");
```

**Step 3: Verify it compiles**

Run: `cargo build`

**Step 4: Run tests**

Run: `cargo test`

**Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: add CLI crawler status command"
```

---

### Task 8: Manual Test

**Step 1: Build release**

```bash
cargo build --release
```

**Step 2: Test with signal mode**

```bash
./target/release/atpdb serve --port=3000 &

# Set signal mode
curl -X POST http://localhost:3000/config \
  -H "Content-Type: application/json" \
  -d '{"mode": "signal", "signal_collection": "app.bsky.feed.post"}'

# Check crawler status
curl http://localhost:3000/crawler

# Stop server
kill %1
```

**Step 3: Final commit**

```bash
git add -A
git commit -m "docs: complete crawler implementation"
```
