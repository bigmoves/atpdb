# Tantivy Full-Text Search Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add field-specified full-text search to atpdb using Tantivy, with explicit query parameters and config-driven field indexing.

**Architecture:** Tantivy runs alongside Fjall as a secondary search index. Tantivy stores indexed text fields + AT-URI only. Fjall remains source of truth. Search queries return URIs from Tantivy, then hydrate full records from Fjall.

**Tech Stack:** tantivy (Rust search library), existing Fjall storage, Axum HTTP server

---

## Design Decisions (from brainstorming)

| Aspect | Decision |
|--------|----------|
| Query syntax | Explicit params: `?collection=...&search.text="query"` |
| Field specification | Explicit: `search.text`, `search.displayName` |
| Configuration | `[[search_fields]]` with `collection` + `field` |
| Storage | Tantivy: indexed text + URI. Fjall: source of truth |
| Sorting | Relevance default, time sort via existing Fjall indexes |
| Deletions | Immediate (Tantivy soft-delete) |
| Commits | Hybrid: 1000 records or 5 seconds |
| Reindexing | Auto background when field added |

---

## Task 1: Add Tantivy Dependency

**Files:**
- Modify: `Cargo.toml`

**Step 1: Add tantivy to dependencies**

In `Cargo.toml`, add to `[dependencies]`:

```toml
tantivy = "0.22"
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles successfully with tantivy dependency

**Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "feat(search): add tantivy dependency"
```

---

## Task 2: Add SearchFieldConfig to Config

**Files:**
- Modify: `src/config.rs`
- Test: `src/config.rs` (inline tests)

**Step 1: Write the failing test**

Add to `src/config.rs` in the `#[cfg(test)] mod tests` section:

```rust
#[test]
fn test_search_field_config_parse() {
    let config = SearchFieldConfig::parse("app.bsky.feed.post:text").unwrap();
    assert_eq!(config.collection, "app.bsky.feed.post");
    assert_eq!(config.field, "text");

    // Nested field
    let config2 = SearchFieldConfig::parse("app.bsky.actor.profile:description").unwrap();
    assert_eq!(config2.collection, "app.bsky.actor.profile");
    assert_eq!(config2.field, "description");

    // Invalid format
    assert!(SearchFieldConfig::parse("invalid").is_none());
}

#[test]
fn test_search_field_config_to_string() {
    let config = SearchFieldConfig {
        collection: "app.bsky.feed.post".to_string(),
        field: "text".to_string(),
    };
    assert_eq!(config.to_config_string(), "app.bsky.feed.post:text");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_search_field_config`
Expected: FAIL with "cannot find type `SearchFieldConfig`"

**Step 3: Write SearchFieldConfig struct**

Add after `IndexConfig` struct in `src/config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchFieldConfig {
    pub collection: String,
    pub field: String,
}

impl SearchFieldConfig {
    /// Parse from string format: "collection:field"
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.splitn(2, ':').collect();
        if parts.len() != 2 {
            return None;
        }
        Some(SearchFieldConfig {
            collection: parts[0].to_string(),
            field: parts[1].to_string(),
        })
    }

    /// Format as string: "collection:field"
    pub fn to_config_string(&self) -> String {
        format!("{}:{}", self.collection, self.field)
    }
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test test_search_field_config`
Expected: PASS

**Step 5: Add search_fields to Config struct**

In `src/config.rs`, add to `Config` struct:

```rust
#[serde(default)]
pub search_fields: Vec<SearchFieldConfig>,
```

And in `Config::default()`:

```rust
search_fields: Vec::new(),
```

**Step 6: Add env var override**

In `Config::apply_env_overrides()`, add:

```rust
if let Ok(search_str) = std::env::var("ATPDB_SEARCH_FIELDS") {
    self.search_fields = search_str
        .split(',')
        .filter_map(|s| SearchFieldConfig::parse(s.trim()))
        .collect();
}
```

**Step 7: Add helper method**

Add to `impl Config`:

```rust
pub fn is_field_searchable(&self, collection: &str, field: &str) -> bool {
    self.search_fields
        .iter()
        .any(|sf| sf.collection == collection && sf.field == field)
}

pub fn search_fields_for_collection(&self, collection: &str) -> Vec<&SearchFieldConfig> {
    self.search_fields
        .iter()
        .filter(|sf| sf.collection == collection)
        .collect()
}
```

**Step 8: Run all config tests**

Run: `cargo test config`
Expected: All tests pass

**Step 9: Commit**

```bash
git add src/config.rs
git commit -m "feat(search): add SearchFieldConfig to config"
```

---

## Task 3: Create SearchIndex Module

**Files:**
- Create: `src/search.rs`
- Modify: `src/main.rs` (add module)

**Step 1: Create search.rs with basic structure**

Create `src/search.rs`:

```rust
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, STORED, TEXT, Field};
use tantivy::{Index, IndexWriter, ReloadPolicy, TantivyDocument};
use thiserror::Error;

use crate::config::SearchFieldConfig;

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("query parse error: {0}")]
    QueryParse(#[from] tantivy::query::QueryParserError),
    #[error("index not configured for collection: {0}")]
    NotConfigured(String),
    #[error("field not indexed: {0}.{1}")]
    FieldNotIndexed(String, String),
}

/// Manages Tantivy search indexes for configured collections
pub struct SearchIndex {
    index: Index,
    writer: Arc<RwLock<IndexWriter>>,
    uri_field: Field,
    collection_field: Field,
    content_field: Field,
    field_name_field: Field,
    // Commit batching
    pending_count: Arc<RwLock<usize>>,
    last_commit: Arc<RwLock<Instant>>,
}

const COMMIT_BATCH_SIZE: usize = 1000;
const COMMIT_INTERVAL: Duration = Duration::from_secs(5);

impl SearchIndex {
    /// Open or create a search index at the given path
    pub fn open(path: &Path) -> Result<Self, SearchError> {
        let schema = Self::build_schema();

        let index = if path.exists() {
            Index::open_in_dir(path)?
        } else {
            std::fs::create_dir_all(path).map_err(|e| {
                tantivy::TantivyError::SystemError(format!("Failed to create index dir: {}", e))
            })?;
            Index::create_in_dir(path, schema.clone())?
        };

        let writer = index.writer(50_000_000)?; // 50MB heap

        let uri_field = schema.get_field("uri").unwrap();
        let collection_field = schema.get_field("collection").unwrap();
        let content_field = schema.get_field("content").unwrap();
        let field_name_field = schema.get_field("field_name").unwrap();

        Ok(SearchIndex {
            index,
            writer: Arc::new(RwLock::new(writer)),
            uri_field,
            collection_field,
            content_field,
            field_name_field,
            pending_count: Arc::new(RwLock::new(0)),
            last_commit: Arc::new(RwLock::new(Instant::now())),
        })
    }

    fn build_schema() -> Schema {
        let mut schema_builder = Schema::builder();
        // URI is stored for retrieval, not indexed for search
        schema_builder.add_text_field("uri", STORED);
        // Collection for filtering
        schema_builder.add_text_field("collection", TEXT | STORED);
        // The actual searchable content
        schema_builder.add_text_field("content", TEXT);
        // Which field this content came from (for field-specific search)
        schema_builder.add_text_field("field_name", TEXT | STORED);
        schema_builder.build()
    }

    /// Index a record's searchable fields
    pub fn index_record(
        &self,
        uri: &str,
        collection: &str,
        value: &serde_json::Value,
        search_fields: &[SearchFieldConfig],
    ) -> Result<(), SearchError> {
        let fields_for_collection: Vec<_> = search_fields
            .iter()
            .filter(|sf| sf.collection == collection)
            .collect();

        if fields_for_collection.is_empty() {
            return Ok(()); // No search fields configured for this collection
        }

        let mut writer = self.writer.write().unwrap();

        for sf in fields_for_collection {
            if let Some(text) = extract_text_field(value, &sf.field) {
                let mut doc = TantivyDocument::new();
                doc.add_text(self.uri_field, uri);
                doc.add_text(self.collection_field, collection);
                doc.add_text(self.content_field, &text);
                doc.add_text(self.field_name_field, &sf.field);
                writer.add_document(doc)?;
            }
        }

        drop(writer);
        self.maybe_commit()?;
        Ok(())
    }

    /// Delete all documents for a URI
    pub fn delete_record(&self, uri: &str) -> Result<(), SearchError> {
        let mut writer = self.writer.write().unwrap();
        let term = tantivy::Term::from_field_text(self.uri_field, uri);
        writer.delete_term(term);
        drop(writer);
        self.maybe_commit()?;
        Ok(())
    }

    /// Check if we should commit based on batch size or time
    fn maybe_commit(&self) -> Result<(), SearchError> {
        let mut count = self.pending_count.write().unwrap();
        *count += 1;

        let last = *self.last_commit.read().unwrap();
        let should_commit = *count >= COMMIT_BATCH_SIZE || last.elapsed() >= COMMIT_INTERVAL;

        if should_commit {
            let mut writer = self.writer.write().unwrap();
            writer.commit()?;
            *count = 0;
            *self.last_commit.write().unwrap() = Instant::now();
        }

        Ok(())
    }

    /// Force a commit (useful for shutdown)
    pub fn commit(&self) -> Result<(), SearchError> {
        let mut writer = self.writer.write().unwrap();
        writer.commit()?;
        *self.pending_count.write().unwrap() = 0;
        *self.last_commit.write().unwrap() = Instant::now();
        Ok(())
    }

    /// Search for records matching a query in a specific field
    pub fn search(
        &self,
        collection: &str,
        field: &str,
        query_text: &str,
        limit: usize,
    ) -> Result<Vec<String>, SearchError> {
        let reader = self.index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        // Build query: collection filter + field filter + content search
        let query_parser = QueryParser::for_index(&self.index, vec![self.content_field]);

        // Construct a boolean query: collection:X AND field_name:Y AND content:query
        let query_string = format!(
            "collection:\"{}\" AND field_name:\"{}\" AND ({})",
            collection, field, query_text
        );

        let query = query_parser.parse_query(&query_string)?;
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let mut uris = Vec::with_capacity(top_docs.len());
        for (_score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)?;
            if let Some(uri) = doc.get_first(self.uri_field).and_then(|v| v.as_str()) {
                uris.push(uri.to_string());
            }
        }

        Ok(uris)
    }
}

/// Extract text from a JSON value at a dot-separated field path
fn extract_text_field(value: &serde_json::Value, field_path: &str) -> Option<String> {
    let mut current = value;
    for part in field_path.split('.') {
        current = current.get(part)?;
    }
    current.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_search_index_create() {
        let dir = tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        assert!(index.commit().is_ok());
    }

    #[test]
    fn test_extract_text_field() {
        let value = serde_json::json!({
            "text": "hello world",
            "nested": {
                "field": "nested value"
            }
        });

        assert_eq!(extract_text_field(&value, "text"), Some("hello world".to_string()));
        assert_eq!(extract_text_field(&value, "nested.field"), Some("nested value".to_string()));
        assert_eq!(extract_text_field(&value, "missing"), None);
    }
}
```

**Step 2: Add module to main.rs**

In `src/main.rs`, add with other module declarations:

```rust
mod search;
```

**Step 3: Run tests**

Run: `cargo test search`
Expected: All tests pass

**Step 4: Commit**

```bash
git add src/search.rs src/main.rs
git commit -m "feat(search): add SearchIndex module with Tantivy"
```

---

## Task 4: Integrate SearchIndex into AppState

**Files:**
- Modify: `src/app.rs`

**Step 1: Read current app.rs**

Read `src/app.rs` to understand current structure.

**Step 2: Add SearchIndex to AppState**

Add import at top of `src/app.rs`:

```rust
use crate::search::SearchIndex;
```

Add field to `AppState` struct:

```rust
pub search: Option<SearchIndex>,
```

**Step 3: Update AppState::new to initialize SearchIndex**

In `AppState::new()`, after creating the store, add:

```rust
// Initialize search index
let search_path = path.join("search");
let search = match SearchIndex::open(&search_path) {
    Ok(idx) => {
        println!("Search index opened at {:?}", search_path);
        Some(idx)
    }
    Err(e) => {
        eprintln!("Warning: Failed to open search index: {}. Search disabled.", e);
        None
    }
};
```

Update the `AppState` construction to include `search`.

**Step 4: Add flush for search index**

In `AppState::flush()`, add:

```rust
if let Some(ref search) = self.search {
    search.commit().map_err(|e| {
        fjall::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
    })?;
}
```

**Step 5: Verify it compiles**

Run: `cargo check`
Expected: Compiles successfully

**Step 6: Commit**

```bash
git add src/app.rs
git commit -m "feat(search): integrate SearchIndex into AppState"
```

---

## Task 5: Index Records on Write

**Files:**
- Modify: `src/storage.rs`
- Modify: `src/firehose.rs`
- Modify: `src/sync.rs`

**Step 1: Update firehose to index on create**

In `src/server.rs` (firehose handling in `start_firehose`), after `app.store.put_with_indexes()`, add:

```rust
// Index for search
if let Some(ref search) = app.search {
    if let Err(e) = search.index_record(
        &uri.to_string(),
        uri.collection.as_str(),
        &value,
        &config.search_fields,
    ) {
        eprintln!("Search index error: {}", e);
    }
}
```

**Step 2: Update firehose to delete from search index**

After `app.store.delete_with_indexes()`, add:

```rust
// Remove from search index
if let Some(ref search) = app.search {
    if let Err(e) = search.delete_record(&uri.to_string()) {
        eprintln!("Search delete error: {}", e);
    }
}
```

**Step 3: Update sync.rs to index records**

In `src/sync.rs`, the `sync_repo` function stores records. Add search indexing there too.

First, update the function signature to accept search index:

```rust
pub fn sync_repo(
    did: &str,
    store: &Store,
    collections: &[String],
    indexes: &[IndexConfig],
    search: Option<&SearchIndex>,
    search_fields: &[SearchFieldConfig],
) -> Result<SyncResult, SyncError>
```

Then after `store.put_with_indexes()`, add:

```rust
if let Some(search) = search {
    let _ = search.index_record(&uri, &collection, &value, search_fields);
}
```

**Step 4: Update callers of sync_repo**

Update `src/sync_worker.rs` and `src/server.rs` to pass the new parameters.

**Step 5: Verify it compiles**

Run: `cargo check`
Expected: Compiles successfully

**Step 6: Commit**

```bash
git add src/server.rs src/sync.rs src/sync_worker.rs
git commit -m "feat(search): index records on write/sync"
```

---

## Task 6: Add Search Query Endpoint

**Files:**
- Modify: `src/server.rs`

**Step 1: Add search request/response types**

Add to `src/server.rs`:

```rust
#[derive(Deserialize)]
pub struct SearchRequest {
    collection: String,
    #[serde(flatten)]
    search_fields: std::collections::HashMap<String, String>, // search.text, search.displayName, etc.
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    hydrate: std::collections::HashMap<String, String>,
    #[serde(default)]
    blobs: std::collections::HashMap<String, String>,
}

#[derive(Serialize)]
pub struct SearchResponse {
    records: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
}
```

**Step 2: Add search handler**

```rust
async fn search_handler(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ErrorResponse>)> {
    let search = app.search.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Search is not enabled".to_string(),
            }),
        )
    })?;

    let limit = payload.limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT);
    let config = app.config();

    // Find search.* fields in the request
    let mut uris: Vec<String> = Vec::new();
    for (key, query) in &payload.search_fields {
        if let Some(field) = key.strip_prefix("search.") {
            // Verify field is configured
            if !config.is_field_searchable(&payload.collection, field) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!(
                            "Field '{}' is not searchable for collection '{}'. Configure ATPDB_SEARCH_FIELDS={}:{}",
                            field, payload.collection, payload.collection, field
                        ),
                    }),
                ));
            }

            let results = search
                .search(&payload.collection, field, query, limit)
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
    }

    // Deduplicate URIs while preserving relevance order
    let mut seen = std::collections::HashSet::new();
    uris.retain(|uri| seen.insert(uri.clone()));
    uris.truncate(limit);

    // Hydrate records from Fjall
    let mut records: Vec<serde_json::Value> = Vec::with_capacity(uris.len());
    for uri in &uris {
        if let Ok(Some(record)) = app.store.get_by_uri_string(uri) {
            let did = extract_did_from_uri(uri).unwrap_or_default();
            let mut record_json = serde_json::json!({
                "uri": record.uri,
                "cid": record.cid,
                "value": record.value,
                "indexedAt": record.indexed_at
            });

            // Transform blobs
            for (path, preset) in &payload.blobs {
                transform_blob_at_path(&mut record_json, path, &did, preset);
            }

            records.push(record_json);
        }
    }

    // Apply hydration if requested
    if !payload.hydrate.is_empty() || !payload.blobs.is_empty() {
        hydrate_records(&mut records, &payload.hydrate, &payload.blobs, &app.store);
    }

    Ok(Json(SearchResponse {
        records,
        total: Some(uris.len()),
    }))
}
```

**Step 3: Add route**

In the router construction, add:

```rust
.route("/search", post(search_handler))
```

**Step 4: Verify it compiles**

Run: `cargo check`
Expected: Compiles successfully

**Step 5: Commit**

```bash
git add src/server.rs
git commit -m "feat(search): add /search endpoint"
```

---

## Task 7: Add CLI Commands for Search Config

**Files:**
- Modify: `src/main.rs`

**Step 1: Add .config search commands**

In the CLI command handling section of `src/main.rs`, add handling for search config:

```rust
".config search list" => {
    let config = app.config();
    if config.search_fields.is_empty() {
        println!("No search fields configured");
    } else {
        println!("Search fields:");
        for sf in &config.search_fields {
            println!("  {}:{}", sf.collection, sf.field);
        }
    }
}

cmd if cmd.starts_with(".config search add ") => {
    let spec = cmd.strip_prefix(".config search add ").unwrap().trim();
    if let Some(sf) = SearchFieldConfig::parse(spec) {
        app.update_config(|config| {
            if !config.search_fields.contains(&sf) {
                config.search_fields.push(sf.clone());
            }
        })?;
        println!("Added search field: {}:{}", sf.collection, sf.field);
        println!("Note: Run .reindex search to index existing records");
    } else {
        println!("Invalid format. Use: .config search add collection:field");
    }
}

cmd if cmd.starts_with(".config search remove ") => {
    let spec = cmd.strip_prefix(".config search remove ").unwrap().trim();
    if let Some(sf) = SearchFieldConfig::parse(spec) {
        app.update_config(|config| {
            config.search_fields.retain(|s| s != &sf);
        })?;
        println!("Removed search field: {}:{}", sf.collection, sf.field);
    } else {
        println!("Invalid format. Use: .config search remove collection:field");
    }
}
```

**Step 2: Add import for SearchFieldConfig**

At the top of main.rs:

```rust
use crate::config::SearchFieldConfig;
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles successfully

**Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(search): add CLI commands for search config"
```

---

## Task 8: Add Reindex Command for Search

**Files:**
- Modify: `src/main.rs`
- Modify: `src/search.rs`

**Step 1: Add reindex_from_store method to SearchIndex**

In `src/search.rs`, add:

```rust
/// Reindex all records from the store for configured search fields
pub fn reindex_from_store(
    &self,
    store: &crate::storage::Store,
    search_fields: &[SearchFieldConfig],
) -> Result<usize, SearchError> {
    let mut count = 0;

    // Get unique collections from search fields
    let collections: std::collections::HashSet<_> = search_fields
        .iter()
        .map(|sf| sf.collection.as_str())
        .collect();

    for collection in collections {
        let collection_nsid: crate::types::Nsid = match collection.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        // Scan all records in collection
        let records = store.scan_all_collection(&collection_nsid).map_err(|e| {
            SearchError::Tantivy(tantivy::TantivyError::SystemError(e.to_string()))
        })?;

        for record in records {
            self.index_record(
                &record.uri,
                collection,
                &record.value,
                search_fields,
            )?;
            count += 1;
        }
    }

    // Force commit after reindex
    self.commit()?;
    Ok(count)
}
```

**Step 2: Add .reindex search command**

In `src/main.rs` CLI handling:

```rust
".reindex search" => {
    if let Some(ref search) = app.search {
        let config = app.config();
        println!("Reindexing search fields...");
        match search.reindex_from_store(&app.store, &config.search_fields) {
            Ok(count) => println!("Reindexed {} records", count),
            Err(e) => eprintln!("Reindex error: {}", e),
        }
    } else {
        println!("Search is not enabled");
    }
}
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles successfully

**Step 4: Commit**

```bash
git add src/main.rs src/search.rs
git commit -m "feat(search): add reindex command for search"
```

---

## Task 9: Add Auto-Reindex on Config Change

**Files:**
- Modify: `src/app.rs`

**Step 1: Detect new search fields on config update**

In `AppState::update_config`, after saving the config, check for new search fields and trigger background reindex:

```rust
pub fn update_config<F>(&self, f: F) -> Result<(), ConfigError>
where
    F: FnOnce(&mut Config),
{
    let mut config = self.config.write();
    let old_search_fields = config.search_fields.clone();

    f(&mut config);

    // Check for new search fields
    let new_fields: Vec<_> = config.search_fields
        .iter()
        .filter(|sf| !old_search_fields.contains(sf))
        .cloned()
        .collect();

    // Save config
    let value = serde_json::to_vec(&*config)?;
    self.config_keyspace.insert(CONFIG_KEY, &value)?;

    // Trigger background reindex for new fields
    if !new_fields.is_empty() {
        if let Some(ref search) = self.search {
            let search = search.clone(); // Need to make SearchIndex Clone or use Arc
            let store = self.store.clone();
            let fields = new_fields;
            std::thread::spawn(move || {
                println!("Auto-reindexing {} new search field(s)...", fields.len());
                match search.reindex_from_store(&store, &fields) {
                    Ok(count) => println!("Auto-reindexed {} records", count),
                    Err(e) => eprintln!("Auto-reindex error: {}", e),
                }
            });
        }
    }

    Ok(())
}
```

**Step 2: Make SearchIndex work with Arc/Clone**

Update `src/search.rs` to wrap in Arc for sharing:

```rust
#[derive(Clone)]
pub struct SearchIndex {
    index: Index,
    writer: Arc<RwLock<IndexWriter>>,
    // ... rest unchanged
}
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles successfully

**Step 4: Commit**

```bash
git add src/app.rs src/search.rs
git commit -m "feat(search): auto-reindex when search fields added"
```

---

## Task 10: Integration Test

**Files:**
- Create: `tests/search_integration.rs`

**Step 1: Write integration test**

Create `tests/search_integration.rs`:

```rust
use tempfile::tempdir;

#[test]
fn test_search_end_to_end() {
    // This test verifies:
    // 1. SearchIndex can be created
    // 2. Records can be indexed
    // 3. Search returns correct URIs
    // 4. Deletion removes from index

    let dir = tempdir().unwrap();
    let search = atpdb::search::SearchIndex::open(dir.path()).unwrap();

    let search_fields = vec![
        atpdb::config::SearchFieldConfig {
            collection: "app.bsky.feed.post".to_string(),
            field: "text".to_string(),
        },
    ];

    // Index a record
    let value = serde_json::json!({
        "text": "Hello Rust programming world!",
        "createdAt": "2024-01-01T00:00:00Z"
    });

    search.index_record(
        "at://did:plc:test/app.bsky.feed.post/123",
        "app.bsky.feed.post",
        &value,
        &search_fields,
    ).unwrap();

    search.commit().unwrap();

    // Search for it
    let results = search.search("app.bsky.feed.post", "text", "Rust", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0], "at://did:plc:test/app.bsky.feed.post/123");

    // Search for non-existent term
    let results = search.search("app.bsky.feed.post", "text", "Python", 10).unwrap();
    assert!(results.is_empty());

    // Delete and verify
    search.delete_record("at://did:plc:test/app.bsky.feed.post/123").unwrap();
    search.commit().unwrap();

    let results = search.search("app.bsky.feed.post", "text", "Rust", 10).unwrap();
    assert!(results.is_empty());
}
```

**Step 2: Make modules public for testing**

In `src/lib.rs` (create if needed) or `src/main.rs`, ensure modules are accessible.

**Step 3: Run integration test**

Run: `cargo test search_integration`
Expected: PASS

**Step 4: Commit**

```bash
git add tests/search_integration.rs
git commit -m "test(search): add integration test for search"
```

---

## Task 11: Update Documentation

**Files:**
- Modify: `README.md` or create `docs/search.md`

**Step 1: Document search feature**

Create `docs/search.md`:

```markdown
# Full-Text Search

atpdb supports full-text search using Tantivy.

## Configuration

Configure which fields to index for search:

```bash
# Environment variable
ATPDB_SEARCH_FIELDS="app.bsky.feed.post:text,app.bsky.actor.profile:displayName"

# CLI
.config search add app.bsky.feed.post:text
.config search list
.config search remove app.bsky.feed.post:text
```

## Querying

Search via HTTP API:

```bash
curl -X POST http://localhost:3000/search \
  -H "Content-Type: application/json" \
  -d '{
    "collection": "app.bsky.feed.post",
    "search.text": "rust programming",
    "limit": 20
  }'
```

## Reindexing

When adding search fields to an existing database:

```bash
# CLI
.reindex search

# Or it auto-reindexes in background when you add fields
```

## How It Works

- Tantivy index stored in `atpdb.data/search/`
- Only configured fields are indexed
- Search returns URIs, then hydrates full records from Fjall
- Commits batched: every 1000 records or 5 seconds
- Deletions immediate (soft-delete in Tantivy)
```

**Step 2: Commit**

```bash
git add docs/search.md
git commit -m "docs: add search documentation"
```

---

## Summary

This plan adds Tantivy full-text search to atpdb with:

1. **Config-driven field indexing** - `SearchFieldConfig` in config
2. **Explicit query syntax** - `search.text="query"` parameter
3. **Hybrid storage** - Tantivy for search, Fjall for records
4. **Batched commits** - 1000 records or 5 seconds
5. **Immediate deletes** - Soft-delete in Tantivy
6. **Auto-reindex** - Background reindex when fields added
7. **CLI commands** - `.config search add/remove/list`, `.reindex search`
8. **HTTP endpoint** - `POST /search`

Total: 11 tasks, approximately 40-50 individual steps.
