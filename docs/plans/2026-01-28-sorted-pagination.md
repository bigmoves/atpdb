# Sorted Pagination Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add secondary indexes for efficient sorted pagination on datetime and integer fields at scale (millions of records).

**Architecture:** Secondary indexes stored as `index:{collection}:{field}\0{encoded_value}\0{did}\0{rkey}`. Indexes declared via config (`ATPDB_INDEXES`). Queries with sort use index scan + keyset cursor. Built-in `indexedAt` index always available.

**Tech Stack:** Rust, fjall (LSM storage), chrono (datetime), base64 (cursor encoding)

---

### Task 1: Add Index Configuration Types

**Files:**
- Modify: `src/config.rs`

**Step 1: Add IndexConfig struct and parsing**

Add after the `Config` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexConfig {
    pub collection: String,
    pub field: String,
    pub field_type: IndexFieldType,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IndexFieldType {
    Datetime,
    Integer,
}

impl std::fmt::Display for IndexFieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexFieldType::Datetime => write!(f, "datetime"),
            IndexFieldType::Integer => write!(f, "integer"),
        }
    }
}

impl IndexConfig {
    /// Parse from string format: "collection:field:type"
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 {
            return None;
        }
        let field_type = match parts[2] {
            "datetime" => IndexFieldType::Datetime,
            "integer" => IndexFieldType::Integer,
            _ => return None,
        };
        Some(IndexConfig {
            collection: parts[0].to_string(),
            field: parts[1].to_string(),
            field_type,
        })
    }

    /// Format as string: "collection:field:type"
    pub fn to_config_string(&self) -> String {
        format!("{}:{}:{}", self.collection, self.field, self.field_type)
    }
}
```

**Step 2: Add indexes field to Config struct**

Modify `Config` struct to add:

```rust
pub struct Config {
    pub mode: Mode,
    pub signal_collection: Option<String>,
    pub collections: Vec<String>,
    pub relay: String,
    pub sync_parallelism: u32,
    #[serde(default)]
    pub indexes: Vec<IndexConfig>,
}
```

Update `Default` impl to include `indexes: Vec::new()`.

**Step 3: Add env var parsing for indexes**

In `apply_env_overrides`, add:

```rust
if let Ok(indexes_str) = std::env::var("ATPDB_INDEXES") {
    self.indexes = indexes_str
        .split(',')
        .filter_map(|s| IndexConfig::parse(s.trim()))
        .collect();
}
```

**Step 4: Add helper to check if field is indexed**

```rust
impl Config {
    pub fn is_field_indexed(&self, collection: &str, field: &str) -> bool {
        self.indexes.iter().any(|idx| idx.collection == collection && idx.field == field)
    }

    pub fn get_index_config(&self, collection: &str, field: &str) -> Option<&IndexConfig> {
        self.indexes.iter().find(|idx| idx.collection == collection && idx.field == field)
    }
}
```

**Step 5: Run build**

Run: `cargo build`
Expected: Compiles with no errors

**Step 6: Commit**

```bash
git add src/config.rs
git commit -m "feat: add index configuration types"
```

---

### Task 2: Add Index Storage Layer

**Files:**
- Modify: `src/storage.rs`

**Step 1: Add index key encoding functions**

Add after existing `storage_key` functions:

```rust
/// Encode integer for lexicographic sorting (handles negative numbers)
fn encode_integer_for_sort(value: i64) -> String {
    // Prefix: 0 for negative, 1 for zero/positive
    // For negative: invert to sort correctly (more negative = smaller)
    if value >= 0 {
        format!("1{:020}", value)
    } else {
        // For negative numbers, invert so -1 > -2 in sort order
        format!("0{:020}", (i64::MAX as u64).wrapping_add(value as u64 + 1))
    }
}

/// Decode integer from sort encoding
fn decode_integer_from_sort(encoded: &str) -> Option<i64> {
    if encoded.len() != 21 {
        return None;
    }
    let prefix = &encoded[0..1];
    let num_str = &encoded[1..];
    let num: u64 = num_str.parse().ok()?;
    if prefix == "1" {
        Some(num as i64)
    } else {
        Some((num.wrapping_sub(i64::MAX as u64 + 1)) as i64)
    }
}

/// Build index key: index:{collection}:{field}\0{sort_value}\0{did}\0{rkey}
fn index_key(collection: &str, field: &str, sort_value: &str, did: &str, rkey: &str) -> Vec<u8> {
    format!("index:{}:{}\0{}\0{}\0{}", collection, field, sort_value, did, rkey).into_bytes()
}

/// Build index prefix for scanning: index:{collection}:{field}\0
fn index_prefix(collection: &str, field: &str) -> Vec<u8> {
    format!("index:{}:{}\0", collection, field).into_bytes()
}

/// Build indexedAt key: index:__indexedAt__:{collection}\0{timestamp}\0{did}\0{rkey}
fn indexed_at_key(collection: &str, timestamp: u64, did: &str, rkey: &str) -> Vec<u8> {
    format!("index:__indexedAt__:{}\0{:020}\0{}\0{}", collection, timestamp, did, rkey).into_bytes()
}

fn indexed_at_prefix(collection: &str) -> Vec<u8> {
    format!("index:__indexedAt__:{}\0", collection).into_bytes()
}
```

**Step 2: Add index write method**

```rust
use crate::config::IndexConfig;

impl Store {
    /// Write index entry for a record
    pub fn write_index_entry(
        &self,
        index: &IndexConfig,
        record: &Record,
        did: &str,
        rkey: &str,
    ) -> Result<(), StorageError> {
        // Extract field value from record
        let sort_value = self.extract_sort_value(&record.value, &index.field, index.field_type)?;
        if let Some(sv) = sort_value {
            let key = index_key(&index.collection, &index.field, &sv, did, rkey);
            self.records.insert(&key, b"")?;
        }
        Ok(())
    }

    /// Write indexedAt index entry (always)
    pub fn write_indexed_at_entry(
        &self,
        collection: &str,
        timestamp: u64,
        did: &str,
        rkey: &str,
    ) -> Result<(), StorageError> {
        let key = indexed_at_key(collection, timestamp, did, rkey);
        self.records.insert(&key, b"")?;
        Ok(())
    }

    /// Extract and encode sort value from record
    fn extract_sort_value(
        &self,
        value: &serde_json::Value,
        field_path: &str,
        field_type: crate::config::IndexFieldType,
    ) -> Result<Option<String>, StorageError> {
        // Navigate to field
        let mut current = value;
        for part in field_path.split('.') {
            current = match current.get(part) {
                Some(v) => v,
                None => return Ok(None),
            };
        }

        // Encode based on type
        use crate::config::IndexFieldType;
        match field_type {
            IndexFieldType::Datetime => {
                // ISO 8601 strings sort correctly as-is
                Ok(current.as_str().map(|s| s.to_string()))
            }
            IndexFieldType::Integer => {
                if let Some(n) = current.as_i64() {
                    Ok(Some(encode_integer_for_sort(n)))
                } else {
                    Ok(None)
                }
            }
        }
    }
}
```

**Step 3: Add index delete method**

```rust
impl Store {
    /// Delete index entry for a record
    pub fn delete_index_entry(
        &self,
        index: &IndexConfig,
        record: &Record,
        did: &str,
        rkey: &str,
    ) -> Result<(), StorageError> {
        let sort_value = self.extract_sort_value(&record.value, &index.field, index.field_type)?;
        if let Some(sv) = sort_value {
            let key = index_key(&index.collection, &index.field, &sv, did, rkey);
            self.records.remove(&key)?;
        }
        Ok(())
    }

    /// Delete indexedAt index entry
    pub fn delete_indexed_at_entry(
        &self,
        collection: &str,
        timestamp: u64,
        did: &str,
        rkey: &str,
    ) -> Result<(), StorageError> {
        let key = indexed_at_key(collection, timestamp, did, rkey);
        self.records.remove(&key)?;
        Ok(())
    }
}
```

**Step 4: Run build**

Run: `cargo build`
Expected: Compiles (warnings about unused functions OK for now)

**Step 5: Commit**

```bash
git add src/storage.rs
git commit -m "feat: add index storage layer"
```

---

### Task 3: Add Index Scan with Pagination

**Files:**
- Modify: `src/storage.rs`

**Step 1: Add ScanDirection enum**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDirection {
    Ascending,
    Descending,
}
```

**Step 2: Add index scan method**

```rust
impl Store {
    /// Scan an index with pagination, returns (records, has_more)
    pub fn scan_index(
        &self,
        collection: &str,
        field: &str,
        cursor_sort_value: Option<&str>,
        cursor_uri: Option<&str>,
        limit: usize,
        direction: ScanDirection,
    ) -> Result<Vec<Record>, StorageError> {
        let prefix = index_prefix(collection, field);

        let start_key = match (cursor_sort_value, cursor_uri) {
            (Some(sv), Some(uri)) => {
                // Parse URI to get did and rkey
                if let Some((did, rkey)) = parse_did_rkey_from_uri(uri) {
                    index_key(collection, field, sv, &did, &rkey)
                } else {
                    prefix.clone()
                }
            }
            _ => prefix.clone(),
        };

        let mut results = Vec::new();
        let mut skip_first = cursor_sort_value.is_some();

        // For descending, we need to scan backwards
        // fjall doesn't have reverse iteration, so we collect and reverse
        // TODO: optimize with reverse prefix scan if fjall adds support
        if direction == ScanDirection::Descending {
            return self.scan_index_descending(collection, field, cursor_sort_value, cursor_uri, limit);
        }

        for item in self.records.range(start_key..) {
            let value = item.value()?;

            // Check if still in index prefix by parsing the key from the empty value marker
            // We stored empty value, so reconstruct URI from key
            let key_bytes = item.key()?;
            let key_str = std::str::from_utf8(&key_bytes).unwrap_or("");

            if !key_str.starts_with(&String::from_utf8_lossy(&prefix)) {
                break;
            }

            if skip_first {
                skip_first = false;
                continue;
            }

            // Parse URI from index key and fetch record
            if let Some(uri) = parse_uri_from_index_key(key_str) {
                if let Ok(Some(record)) = self.get(&uri.parse().unwrap()) {
                    results.push(record);
                    if results.len() >= limit {
                        break;
                    }
                }
            }
        }

        Ok(results)
    }

    fn scan_index_descending(
        &self,
        collection: &str,
        field: &str,
        cursor_sort_value: Option<&str>,
        cursor_uri: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Record>, StorageError> {
        let prefix = index_prefix(collection, field);

        // Collect all entries, then reverse and filter
        // This is O(n) but necessary without reverse iteration support
        let mut all_entries: Vec<(String, String)> = Vec::new(); // (sort_value, uri)

        for item in self.records.prefix(&prefix) {
            let value = item.value()?;
            let key_bytes = item.key()?;
            let key_str = std::str::from_utf8(&key_bytes).unwrap_or("");

            if let Some((sv, uri)) = parse_sort_value_and_uri_from_index_key(key_str) {
                all_entries.push((sv, uri));
            }
        }

        // Reverse for descending
        all_entries.reverse();

        // Find cursor position and skip
        let start_idx = if let (Some(csv), Some(curi)) = (cursor_sort_value, cursor_uri) {
            all_entries
                .iter()
                .position(|(sv, uri)| sv == csv && uri == curi)
                .map(|i| i + 1)
                .unwrap_or(0)
        } else {
            0
        };

        let mut results = Vec::new();
        for (_, uri) in all_entries.into_iter().skip(start_idx).take(limit) {
            if let Ok(Some(record)) = self.get(&uri.parse().unwrap()) {
                results.push(record);
            }
        }

        Ok(results)
    }
}

/// Parse did and rkey from AT URI
fn parse_did_rkey_from_uri(uri: &str) -> Option<(String, String)> {
    // at://did:plc:xxx/collection/rkey
    let without_scheme = uri.strip_prefix("at://")?;
    let parts: Vec<&str> = without_scheme.splitn(3, '/').collect();
    if parts.len() == 3 {
        Some((parts[0].to_string(), parts[2].to_string()))
    } else {
        None
    }
}

/// Parse URI from index key
fn parse_uri_from_index_key(key: &str) -> Option<String> {
    // index:collection:field\0sort_value\0did\0rkey
    let parts: Vec<&str> = key.splitn(2, '\0').collect();
    if parts.len() != 2 {
        return None;
    }
    let remainder = parts[1];
    let subparts: Vec<&str> = remainder.splitn(3, '\0').collect();
    if subparts.len() == 3 {
        let _sort_value = subparts[0];
        let did = subparts[1];
        let rkey = subparts[2];
        // Reconstruct collection from the prefix part
        let prefix_parts: Vec<&str> = parts[0].splitn(3, ':').collect();
        if prefix_parts.len() == 3 {
            let collection = prefix_parts[1..].join(":");
            return Some(format!("at://{}/{}/{}", did, collection, rkey));
        }
    }
    None
}

/// Parse sort value and URI from index key
fn parse_sort_value_and_uri_from_index_key(key: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = key.splitn(2, '\0').collect();
    if parts.len() != 2 {
        return None;
    }
    let remainder = parts[1];
    let subparts: Vec<&str> = remainder.splitn(3, '\0').collect();
    if subparts.len() == 3 {
        let sort_value = subparts[0].to_string();
        let did = subparts[1];
        let rkey = subparts[2];
        let prefix_parts: Vec<&str> = parts[0].splitn(3, ':').collect();
        if prefix_parts.len() == 3 {
            let collection = prefix_parts[1..].join(":");
            let uri = format!("at://{}/{}/{}", did, collection, rkey);
            return Some((sort_value, uri));
        }
    }
    None
}
```

**Step 3: Add indexedAt scan method**

```rust
impl Store {
    pub fn scan_indexed_at(
        &self,
        collection: &str,
        cursor_timestamp: Option<u64>,
        cursor_uri: Option<&str>,
        limit: usize,
        direction: ScanDirection,
    ) -> Result<Vec<Record>, StorageError> {
        if direction == ScanDirection::Descending {
            return self.scan_indexed_at_descending(collection, cursor_timestamp, cursor_uri, limit);
        }

        let prefix = indexed_at_prefix(collection);
        let start_key = match (cursor_timestamp, cursor_uri) {
            (Some(ts), Some(uri)) => {
                if let Some((did, rkey)) = parse_did_rkey_from_uri(uri) {
                    indexed_at_key(collection, ts, &did, &rkey)
                } else {
                    prefix.clone()
                }
            }
            _ => prefix.clone(),
        };

        let mut results = Vec::new();
        let mut skip_first = cursor_timestamp.is_some();

        for item in self.records.range(start_key..) {
            let value = item.value()?;
            let key_bytes = item.key()?;
            let key_str = std::str::from_utf8(&key_bytes).unwrap_or("");

            if !key_str.starts_with(&String::from_utf8_lossy(&prefix)) {
                break;
            }

            if skip_first {
                skip_first = false;
                continue;
            }

            if let Some(uri) = parse_uri_from_indexed_at_key(key_str) {
                if let Ok(Some(record)) = self.get(&uri.parse().unwrap()) {
                    results.push(record);
                    if results.len() >= limit {
                        break;
                    }
                }
            }
        }

        Ok(results)
    }

    fn scan_indexed_at_descending(
        &self,
        collection: &str,
        cursor_timestamp: Option<u64>,
        cursor_uri: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Record>, StorageError> {
        let prefix = indexed_at_prefix(collection);
        let mut all_entries: Vec<(u64, String)> = Vec::new();

        for item in self.records.prefix(&prefix) {
            let value = item.value()?;
            let key_bytes = item.key()?;
            let key_str = std::str::from_utf8(&key_bytes).unwrap_or("");

            if let Some((ts, uri)) = parse_timestamp_and_uri_from_indexed_at_key(key_str) {
                all_entries.push((ts, uri));
            }
        }

        all_entries.reverse();

        let start_idx = if let (Some(cts), Some(curi)) = (cursor_timestamp, cursor_uri) {
            all_entries
                .iter()
                .position(|(ts, uri)| *ts == cts && uri == curi)
                .map(|i| i + 1)
                .unwrap_or(0)
        } else {
            0
        };

        let mut results = Vec::new();
        for (_, uri) in all_entries.into_iter().skip(start_idx).take(limit) {
            if let Ok(Some(record)) = self.get(&uri.parse().unwrap()) {
                results.push(record);
            }
        }

        Ok(results)
    }
}

fn parse_uri_from_indexed_at_key(key: &str) -> Option<String> {
    // index:__indexedAt__:collection\0timestamp\0did\0rkey
    let parts: Vec<&str> = key.splitn(2, '\0').collect();
    if parts.len() != 2 {
        return None;
    }
    let remainder = parts[1];
    let subparts: Vec<&str> = remainder.splitn(3, '\0').collect();
    if subparts.len() == 3 {
        let _timestamp = subparts[0];
        let did = subparts[1];
        let rkey = subparts[2];
        // Extract collection from prefix
        let prefix = parts[0];
        if let Some(collection) = prefix.strip_prefix("index:__indexedAt__:") {
            return Some(format!("at://{}/{}/{}", did, collection, rkey));
        }
    }
    None
}

fn parse_timestamp_and_uri_from_indexed_at_key(key: &str) -> Option<(u64, String)> {
    let parts: Vec<&str> = key.splitn(2, '\0').collect();
    if parts.len() != 2 {
        return None;
    }
    let remainder = parts[1];
    let subparts: Vec<&str> = remainder.splitn(3, '\0').collect();
    if subparts.len() == 3 {
        let timestamp: u64 = subparts[0].parse().ok()?;
        let did = subparts[1];
        let rkey = subparts[2];
        let prefix = parts[0];
        if let Some(collection) = prefix.strip_prefix("index:__indexedAt__:") {
            let uri = format!("at://{}/{}/{}", did, collection, rkey);
            return Some((timestamp, uri));
        }
    }
    None
}
```

**Step 4: Run build**

Run: `cargo build`
Expected: Compiles

**Step 5: Commit**

```bash
git add src/storage.rs
git commit -m "feat: add index scan with pagination"
```

---

### Task 4: Update Record Write to Maintain Indexes

**Files:**
- Modify: `src/storage.rs`
- Modify: `src/server.rs` (firehose handler)
- Modify: `src/sync.rs`

**Step 1: Add put_with_indexes method to Store**

```rust
impl Store {
    pub fn put_with_indexes(
        &self,
        uri: &AtUri,
        record: &Record,
        indexes: &[IndexConfig],
    ) -> Result<(), StorageError> {
        // Write primary record
        self.put(uri, record)?;

        // Write indexedAt index
        self.write_indexed_at_entry(
            uri.collection.as_str(),
            record.indexed_at,
            uri.did.as_str(),
            uri.rkey.as_str(),
        )?;

        // Write configured indexes
        for index in indexes {
            if index.collection == uri.collection.as_str() {
                self.write_index_entry(index, record, uri.did.as_str(), uri.rkey.as_str())?;
            }
        }

        Ok(())
    }

    pub fn delete_with_indexes(
        &self,
        uri: &AtUri,
        indexes: &[IndexConfig],
    ) -> Result<(), StorageError> {
        // Get record first to know index values
        if let Some(record) = self.get(uri)? {
            // Delete indexes
            self.delete_indexed_at_entry(
                uri.collection.as_str(),
                record.indexed_at,
                uri.did.as_str(),
                uri.rkey.as_str(),
            )?;

            for index in indexes {
                if index.collection == uri.collection.as_str() {
                    self.delete_index_entry(index, &record, uri.did.as_str(), uri.rkey.as_str())?;
                }
            }
        }

        // Delete primary record
        self.delete(uri)?;

        Ok(())
    }
}
```

**Step 2: Update firehose handler in server.rs**

Find the firehose event handler and update record writes to use `put_with_indexes`:

```rust
// In handle_operation or similar, change:
// store.put(&uri, &record)?;
// To:
let indexes = app.config().indexes;
store.put_with_indexes(&uri, &record, &indexes)?;
```

**Step 3: Update sync.rs**

Find where records are written during sync and update similarly.

**Step 4: Run build**

Run: `cargo build`
Expected: Compiles

**Step 5: Commit**

```bash
git add src/storage.rs src/server.rs src/sync.rs
git commit -m "feat: maintain indexes on record write/delete"
```

---

### Task 5: Add Index Rebuild Command

**Files:**
- Modify: `src/storage.rs`
- Modify: `src/main.rs`
- Modify: `src/app.rs`

**Step 1: Add rebuild_index method to Store**

```rust
impl Store {
    pub fn rebuild_index(&self, index: &IndexConfig) -> Result<usize, StorageError> {
        let collection_prefix = format!("{}\0", index.collection);
        let mut count = 0;

        for item in self.records.prefix(collection_prefix.as_bytes()) {
            let value = item.value()?;

            // Skip index entries (they start with "index:")
            let key_bytes = item.key()?;
            let key_str = std::str::from_utf8(&key_bytes).unwrap_or("");
            if key_str.starts_with("index:") {
                continue;
            }

            let record: Record = serde_json::from_slice(&value)?;

            // Parse did and rkey from key
            let parts: Vec<&str> = key_str.splitn(3, '\0').collect();
            if parts.len() == 3 {
                let did = parts[1];
                let rkey = parts[2];
                self.write_index_entry(index, &record, did, rkey)?;
                count += 1;
            }
        }

        Ok(count)
    }

    pub fn rebuild_indexed_at(&self, collection: &str) -> Result<usize, StorageError> {
        let collection_prefix = format!("{}\0", collection);
        let mut count = 0;

        for item in self.records.prefix(collection_prefix.as_bytes()) {
            let value = item.value()?;

            let key_bytes = item.key()?;
            let key_str = std::str::from_utf8(&key_bytes).unwrap_or("");
            if key_str.starts_with("index:") {
                continue;
            }

            let record: Record = serde_json::from_slice(&value)?;

            let parts: Vec<&str> = key_str.splitn(3, '\0').collect();
            if parts.len() == 3 {
                let did = parts[1];
                let rkey = parts[2];
                self.write_indexed_at_entry(collection, record.indexed_at, did, rkey)?;
                count += 1;
            }
        }

        Ok(count)
    }
}
```

**Step 2: Add CLI command in main.rs**

Add to handle_command:

```rust
cmd if cmd.starts_with(".index ") => {
    let parts: Vec<&str> = cmd.strip_prefix(".index ").unwrap().split_whitespace().collect();
    match parts.get(0).map(|s| *s) {
        Some("rebuild") => {
            if let Some(spec) = parts.get(1) {
                if let Some(index) = IndexConfig::parse(spec) {
                    println!("Rebuilding index {}...", spec);
                    match app.store.rebuild_index(&index) {
                        Ok(count) => println!("Rebuilt index with {} entries", count),
                        Err(e) => println!("Error: {}", e),
                    }
                } else {
                    println!("Invalid index spec. Format: collection:field:type");
                }
            } else {
                println!("Usage: .index rebuild <collection:field:type>");
            }
        }
        Some("list") => {
            let config = app.config();
            if config.indexes.is_empty() {
                println!("No indexes configured");
            } else {
                println!("Configured indexes:");
                for idx in &config.indexes {
                    println!("  {}", idx.to_config_string());
                }
            }
        }
        _ => {
            println!("Usage:");
            println!("  .index list                    List configured indexes");
            println!("  .index rebuild <col:field:type> Rebuild an index");
        }
    }
    CommandResult::Continue
}
```

**Step 3: Add .config indexes command**

Add to the `.config` match in main.rs:

```rust
"indexes" => {
    if parts.len() < 2 {
        println!("Usage: .config indexes <col:field:type,...>");
        return CommandResult::Continue;
    }
    let indexes: Vec<IndexConfig> = parts[1]
        .split(',')
        .filter_map(|s| IndexConfig::parse(s.trim()))
        .collect();
    if let Err(e) = app.update_config(|c| c.indexes = indexes.clone()) {
        println!("Error: {}", e);
    } else {
        println!("Indexes set to: {}", indexes.iter().map(|i| i.to_config_string()).collect::<Vec<_>>().join(", "));
    }
}
```

**Step 4: Run build**

Run: `cargo build`
Expected: Compiles

**Step 5: Commit**

```bash
git add src/storage.rs src/main.rs src/app.rs
git commit -m "feat: add index rebuild command"
```

---

### Task 6: Update Query Handler for Sorted Pagination

**Files:**
- Modify: `src/server.rs`

**Step 1: Add cursor encoding/decoding**

```rust
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

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
```

**Step 2: Update query_handler to use indexes**

Replace the sort handling in query_handler:

```rust
async fn query_handler(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let parsed = Query::parse(&payload.q).map_err(|e| {
        (StatusCode::BAD_REQUEST, Json(ErrorResponse { error: e.to_string() }))
    })?;

    let limit = payload.limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT);
    let config = app.config();

    // Handle sorted queries
    if let Some(ref sort) = payload.sort {
        let parts: Vec<&str> = sort.split(':').collect();
        let field = parts.get(0).unwrap_or(&"indexedAt");
        let field_type = parts.get(1).unwrap_or(&"datetime");
        let order = parts.get(2).unwrap_or(&"desc");
        let direction = if *order == "asc" { ScanDirection::Ascending } else { ScanDirection::Descending };

        // Get collection from query
        let collection = match &parsed {
            Query::AllCollection { collection } => collection.as_str(),
            Query::Collection { collection, .. } => collection.as_str(),
            Query::Exact(_) => {
                // Exact queries don't need sorting
                return execute_unsorted_query(&app, &parsed, limit, payload.cursor.as_deref()).await;
            }
        };

        // Decode cursor if present
        let cursor = payload.cursor.as_ref().and_then(|c| decode_cursor(c));

        // Check if using indexedAt or a configured index
        let records = if *field == "indexedAt" {
            app.store.scan_indexed_at(
                collection,
                cursor.as_ref().and_then(|c| c.sort_value.parse().ok()),
                cursor.as_ref().map(|c| c.uri.as_str()),
                limit + 1,
                direction,
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?
        } else {
            // Check if field is indexed
            let field_name = field.strip_prefix("value.").unwrap_or(field);
            if !config.is_field_indexed(collection, field_name) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!(
                            "Sort field '{}' is not indexed for collection '{}'. Add to ATPDB_INDEXES or use 'indexedAt'.",
                            field_name, collection
                        ),
                    }),
                ));
            }

            app.store.scan_index(
                collection,
                field_name,
                cursor.as_ref().map(|c| c.sort_value.as_str()),
                cursor.as_ref().map(|c| c.uri.as_str()),
                limit + 1,
                direction,
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?
        };

        let has_more = records.len() > limit;
        let records: Vec<_> = records.into_iter().take(limit).collect();

        let next_cursor = if has_more {
            records.last().map(|r| {
                let sort_value = if *field == "indexedAt" {
                    r.indexed_at.to_string()
                } else {
                    // Extract sort value from record
                    extract_field_value(&r.value, field.strip_prefix("value.").unwrap_or(field))
                        .unwrap_or_default()
                };
                encode_cursor(&sort_value, &r.uri)
            })
        } else {
            None
        };

        let values = records_to_json(records);
        return Ok(Json(QueryResponse { records: values, cursor: next_cursor }));
    }

    // Unsorted query - use existing pagination
    execute_unsorted_query(&app, &parsed, limit, payload.cursor.as_deref()).await
}

fn extract_field_value(value: &serde_json::Value, field_path: &str) -> Option<String> {
    let mut current = value;
    for part in field_path.split('.') {
        current = current.get(part)?;
    }
    current.as_str().map(|s| s.to_string())
        .or_else(|| current.as_i64().map(|n| n.to_string()))
}

fn records_to_json(records: Vec<Record>) -> Vec<serde_json::Value> {
    records.into_iter().map(|r| {
        serde_json::json!({
            "uri": r.uri,
            "cid": r.cid,
            "value": r.value,
            "indexedAt": r.indexed_at
        })
    }).collect()
}

async fn execute_unsorted_query(
    app: &AppState,
    parsed: &Query,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let records = query::execute_paginated(parsed, &app.store, cursor, limit + 1)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.to_string() })))?;

    let has_more = records.len() > limit;
    let records: Vec<_> = records.into_iter().take(limit).collect();

    let next_cursor = if has_more {
        records.last().map(|r| r.uri.clone())
    } else {
        None
    };

    Ok(Json(QueryResponse {
        records: records_to_json(records),
        cursor: next_cursor,
    }))
}
```

**Step 3: Add base64 dependency**

Run: `cargo add base64`

**Step 4: Run build**

Run: `cargo build`
Expected: Compiles

**Step 5: Commit**

```bash
git add src/server.rs Cargo.toml Cargo.lock
git commit -m "feat: update query handler for sorted pagination"
```

---

### Task 7: Auto-build Missing Indexes on Startup

**Files:**
- Modify: `src/app.rs`

**Step 1: Add index check and rebuild on startup**

Add to `AppState::open` after loading config:

```rust
impl AppState {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        // ... existing code ...

        let app = AppState {
            store: Store::from_keyspace(records),
            repos: RepoStore::from_keyspace(repos_keyspace),
            config: RwLock::new(config.clone()),
            config_keyspace,
            crawler_cursors,
        };

        // Build missing indexes
        app.ensure_indexes_exist(&config)?;

        Ok(app)
    }

    fn ensure_indexes_exist(&self, config: &Config) -> Result<(), AppError> {
        for index in &config.indexes {
            // Check if index has any entries
            let prefix = format!("index:{}:{}\0", index.collection, index.field);
            let has_entries = self.store.records.prefix(prefix.as_bytes()).next().is_some();

            if !has_entries {
                println!("Building index {}:{}...", index.collection, index.field);
                match self.store.rebuild_index(index) {
                    Ok(count) => println!("Built index with {} entries", count),
                    Err(e) => eprintln!("Error building index: {}", e),
                }
            }
        }
        Ok(())
    }
}
```

**Step 2: Run build**

Run: `cargo build`
Expected: Compiles

**Step 3: Commit**

```bash
git add src/app.rs
git commit -m "feat: auto-build missing indexes on startup"
```

---

### Task 8: Update Example with Sorted Pagination

**Files:**
- Modify: `examples/fm-teal/index.html`

**Step 1: Update fetch to use sort parameter**

Update the fetchPage function:

```javascript
const body = {
    q: 'at://*/fm.teal.alpha.feed.play/*',
    limit: PAGE_SIZE,
    sort: 'value.playedTime:datetime:desc'
};
if (currentCursor) {
    body.cursor = currentCursor;
}
```

**Step 2: Remove client-side sorting**

Remove the `data.records.sort()` call since server now sorts.

**Step 3: Commit**

```bash
git add examples/fm-teal/index.html
git commit -m "feat: update example to use sorted pagination"
```

---

### Task 9: Integration Test

**Step 1: Build release**

Run: `cargo build --release`

**Step 2: Test manually**

```bash
# Set up config with index
ATPDB_INDEXES=fm.teal.alpha.feed.play:playedTime:datetime \
ATPDB_MODE=manual \
./target/release/atpdb serve --port=3457

# Query with sort
curl -X POST http://localhost:3457/query \
  -H "Content-Type: application/json" \
  -d '{"q": "at://*/fm.teal.alpha.feed.play/*", "sort": "value.playedTime:datetime:desc", "limit": 10}'

# Verify cursor works
# Copy cursor from response and use in next request
```

**Step 3: Commit any fixes**

```bash
git add -A
git commit -m "fix: integration test fixes"
```

---

Plan complete and saved to `docs/plans/2026-01-28-sorted-pagination.md`.

**Two execution options:**

1. **Subagent-Driven (this session)** - I dispatch fresh subagent per task, review between tasks, fast iteration

2. **Parallel Session (separate)** - Open new session with executing-plans, batch execution with checkpoints

Which approach?
