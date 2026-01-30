# Reverse Lookups & Counts Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable counting and fetching records that reference a given record (e.g., like counts on posts).

**Architecture:** Add `at-uri` index type for field value lookups. Parse reverse lookup patterns from `count.*` and `hydrate.*` fields (detected by wildcards + query params). Execute lookups using the new index.

**Tech Stack:** Rust, fjall (existing), AT-URI pattern parsing

---

## Task 1: Add `at-uri` Index Field Type

**Files:**
- Modify: `src/config.rs:57-71` (IndexFieldType enum)
- Modify: `src/config.rs:98-126` (IndexConfig::parse)

**Step 1: Add AtUri variant to IndexFieldType**

In `src/config.rs`, update the enum:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IndexFieldType {
    Datetime,
    Integer,
    AtUri,
}

impl std::fmt::Display for IndexFieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexFieldType::Datetime => write!(f, "datetime"),
            IndexFieldType::Integer => write!(f, "integer"),
            IndexFieldType::AtUri => write!(f, "at-uri"),
        }
    }
}
```

**Step 2: Update IndexConfig::parse to handle at-uri**

In `src/config.rs`, update the parse function:

```rust
let field_type = match parts[2] {
    "datetime" => IndexFieldType::Datetime,
    "integer" => IndexFieldType::Integer,
    "at-uri" => IndexFieldType::AtUri,
    _ => return None,
};
```

**Step 3: Run tests**

Run: `cargo test config`
Expected: PASS

**Step 4: Commit**

```bash
git add src/config.rs
git commit -m "feat: add at-uri index field type for reverse lookups"
```

---

## Task 2: Index at-uri Fields in Storage

**Files:**
- Modify: `src/storage.rs` (add reverse index key functions and indexing logic)

**Step 1: Add reverse index key functions**

Add after `index_prefix_desc` function (~line 142):

```rust
/// Build reverse lookup index key: rev:{collection}:{field}\0{field_value}\0{did}\0{rkey}
fn reverse_index_key(
    collection: &str,
    field: &str,
    field_value: &str,
    did: &str,
    rkey: &str,
) -> Vec<u8> {
    format!(
        "rev:{}:{}\0{}\0{}\0{}",
        Self::sanitize_key_value(collection),
        Self::sanitize_key_value(field),
        Self::sanitize_key_value(field_value),
        did,
        rkey
    )
    .into_bytes()
}

/// Build reverse lookup index prefix for scanning by field value
fn reverse_index_prefix(collection: &str, field: &str, field_value: &str) -> Vec<u8> {
    format!(
        "rev:{}:{}\0{}\0",
        Self::sanitize_key_value(collection),
        Self::sanitize_key_value(field),
        Self::sanitize_key_value(field_value)
    )
    .into_bytes()
}
```

**Step 2: Update put_with_indexes to handle AtUri type**

Find the `put_with_indexes` function and add handling for `IndexFieldType::AtUri`:

```rust
IndexFieldType::AtUri => {
    if let Some(value) = Self::get_nested_value(&record.value, &index.field) {
        if let Some(uri_str) = value.as_str() {
            // Index the at-uri value for reverse lookups
            let key = Self::reverse_index_key(
                &index.collection,
                &index.field,
                uri_str,
                uri.did.as_str(),
                uri.rkey.as_str(),
            );
            batch.insert(&self.records, key, b"");
        }
    }
}
```

**Step 3: Update delete to clean up reverse indexes**

In the `delete` function, add cleanup for reverse indexes (similar pattern to existing index cleanup).

**Step 4: Run tests**

Run: `cargo test storage`
Expected: PASS

**Step 5: Commit**

```bash
git add src/storage.rs
git commit -m "feat: index at-uri fields for reverse lookups"
```

---

## Task 3: Add Reverse Lookup Query Method to Storage

**Files:**
- Modify: `src/storage.rs` (add count and fetch methods)

**Step 1: Add count_by_field_value method**

```rust
/// Count records where a field equals a specific value
pub fn count_by_field_value(
    &self,
    collection: &str,
    field: &str,
    field_value: &str,
) -> Result<usize, StorageError> {
    let prefix = Self::reverse_index_prefix(collection, field, field_value);
    let mut count = 0;
    for item in self.records.prefix(&prefix) {
        let _ = item?;
        count += 1;
    }
    Ok(count)
}
```

**Step 2: Add get_by_field_value method**

```rust
/// Get records where a field equals a specific value
pub fn get_by_field_value(
    &self,
    collection: &str,
    field: &str,
    field_value: &str,
    limit: usize,
) -> Result<Vec<Record>, StorageError> {
    let prefix = Self::reverse_index_prefix(collection, field, field_value);
    let mut records = Vec::new();

    for item in self.records.prefix(&prefix) {
        if records.len() >= limit {
            break;
        }
        let (key, _) = item?;
        // Parse key to extract did and rkey: rev:collection:field\0value\0did\0rkey
        let key_str = String::from_utf8_lossy(&key);
        let parts: Vec<&str> = key_str.split('\0').collect();
        if parts.len() >= 4 {
            let did = parts[2];
            let rkey = parts[3];
            // Fetch the actual record
            if let Some(record) = self.get_by_key(collection, did, rkey)? {
                records.push(record);
            }
        }
    }

    Ok(records)
}
```

**Step 3: Run tests**

Run: `cargo test storage`
Expected: PASS

**Step 4: Commit**

```bash
git add src/storage.rs
git commit -m "feat: add count_by_field_value and get_by_field_value methods"
```

---

## Task 4: Parse Reverse Lookup Patterns in Server

**Files:**
- Modify: `src/server.rs` (add pattern parsing and count.* extraction)

**Step 1: Add reverse lookup pattern struct**

Add near other request structs:

```rust
#[derive(Debug, Clone)]
struct ReverseLookup {
    collection: String,
    field: String,
    match_field: String,  // e.g., "$.uri"
    limit: Option<usize>,
}

/// Parse a reverse lookup pattern like "at://*/app.bsky.feed.like/*?subject.uri=$.uri&limit=5"
fn parse_reverse_lookup(pattern: &str) -> Option<ReverseLookup> {
    // Must have wildcards and query params
    if !pattern.contains('*') || !pattern.contains('?') {
        return None;
    }

    // Parse: at://*/collection/*?field=$.value&limit=N
    let parts: Vec<&str> = pattern.splitn(2, '?').collect();
    if parts.len() != 2 {
        return None;
    }

    let uri_part = parts[0];
    let query_part = parts[1];

    // Extract collection from at://*/collection/*
    let uri_parts: Vec<&str> = uri_part.split('/').collect();
    if uri_parts.len() < 4 {
        return None;
    }
    let collection = uri_parts[3].trim_end_matches('*').trim_end_matches('/');

    // Parse query params
    let mut field = None;
    let mut match_field = None;
    let mut limit = None;

    for param in query_part.split('&') {
        let kv: Vec<&str> = param.splitn(2, '=').collect();
        if kv.len() == 2 {
            if kv[1].starts_with("$.") {
                field = Some(kv[0].to_string());
                match_field = Some(kv[1].to_string());
            } else if kv[0] == "limit" {
                limit = kv[1].parse().ok();
            }
        }
    }

    Some(ReverseLookup {
        collection: collection.to_string(),
        field: field?,
        match_field: match_field?,
        limit,
    })
}
```

**Step 2: Add count.* field extraction**

Add function to extract count fields (similar to extract_search_fields):

```rust
/// Extract count.* fields from the flattened map
fn extract_count_fields(fields: &HashMap<String, serde_json::Value>) -> HashMap<String, String> {
    fields
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("count.").and_then(|field| {
                v.as_str()
                    .map(|pattern| (field.to_string(), pattern.to_string()))
            })
        })
        .collect()
}
```

**Step 3: Run tests**

Run: `cargo check`
Expected: PASS (compiles)

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat: parse reverse lookup patterns from count.* fields"
```

---

## Task 5: Execute Reverse Lookups in Query Handler

**Files:**
- Modify: `src/server.rs` (integrate reverse lookups into query execution)

**Step 1: Update QueryRequest to capture count.* fields**

The existing `#[serde(flatten)] search_fields` already captures extra fields. We'll extract count.* from there.

**Step 2: Add reverse lookup execution in query handler**

After hydration, add count execution:

```rust
// Extract and execute count fields
let count_fields = extract_count_fields(&payload.search_fields);
if !count_fields.is_empty() {
    for record in &mut values {
        for (key, pattern) in &count_fields {
            if let Some(lookup) = parse_reverse_lookup(pattern) {
                // Substitute $.field with actual value from record
                let match_value = if lookup.match_field == "$.uri" {
                    record.get("uri").and_then(|v| v.as_str())
                } else {
                    // Handle other $.field patterns
                    let field_path = lookup.match_field.strip_prefix("$.").unwrap_or("");
                    get_nested_str(record, field_path)
                };

                if let Some(value) = match_value {
                    let count = app.store.count_by_field_value(
                        &lookup.collection,
                        &lookup.field,
                        value,
                    ).unwrap_or(0);
                    record[key] = serde_json::Value::Number(count.into());
                }
            }
        }
    }
}
```

**Step 3: Run tests**

Run: `cargo check`
Expected: PASS

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat: execute count.* reverse lookups in query handler"
```

---

## Task 6: Add Reverse Hydrate Support

**Files:**
- Modify: `src/server.rs` (extend hydrate to support reverse patterns)

**Step 1: Update hydrate_records to detect and handle reverse patterns**

In the hydration logic, check if pattern has wildcards:

```rust
// In hydrate_records function, when parsing patterns:
if pattern.contains('*') && pattern.contains('?') {
    // This is a reverse lookup pattern
    if let Some(lookup) = parse_reverse_lookup(pattern) {
        // Handle reverse hydration
        let match_value = /* extract from record */;
        if let Some(value) = match_value {
            let related = store.get_by_field_value(
                &lookup.collection,
                &lookup.field,
                value,
                lookup.limit.unwrap_or(10),
            )?;
            record[key] = serde_json::to_value(related)?;
        }
    }
} else {
    // Existing forward hydration logic
}
```

**Step 2: Run full test suite**

Run: `cargo test`
Expected: PASS

**Step 3: Commit**

```bash
git add src/server.rs
git commit -m "feat: support reverse hydration patterns with wildcards"
```

---

## Task 7: Create app-bsky Example

**Files:**
- Create: `examples/app-bsky/README.md`
- Create: `examples/app-bsky/index.html`

**Step 1: Create README**

```markdown
# Bluesky Timeline Example

A demo timeline UI for Bluesky posts, powered by atpdb.

## Running atpdb

Start the server with Bluesky configuration:

\`\`\`bash
ATPDB_MODE=signal \
ATPDB_SIGNAL_COLLECTION=app.bsky.feed.post \
ATPDB_COLLECTIONS=app.bsky.feed.post,app.bsky.actor.profile,app.bsky.feed.like,app.bsky.feed.repost \
ATPDB_INDEXES=app.bsky.feed.post:createdAt:datetime,app.bsky.feed.like:subject.uri:at-uri,app.bsky.feed.repost:subject.uri:at-uri \
ATPDB_SEARCH_FIELDS=app.bsky.feed.post:text \
cargo run -- serve
\`\`\`

## Running the UI

Open \`index.html\` in your browser. It connects to \`http://localhost:3000\` by default.

## Query Examples

\`\`\`bash
# Latest posts with like/repost counts
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/app.bsky.feed.post/*", "sort": "createdAt:datetime:desc", "hydrate.author": "at://$.did/app.bsky.actor.profile/self", "count.likes": "at://*/app.bsky.feed.like/*?subject.uri=$.uri", "count.reposts": "at://*/app.bsky.feed.repost/*?subject.uri=$.uri"}'

# Search posts
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/app.bsky.feed.post/*", "search.text": "hello"}'
\`\`\`
```

**Step 2: Create index.html**

Base on fm-teal example but render posts with:
- Author avatar + displayName
- Post text
- Like count (heart icon)
- Repost count
- Timestamp
- Search by post text

**Step 3: Commit**

```bash
git add examples/app-bsky/
git commit -m "feat: add app-bsky example with timeline UI"
```

---

## Task 8: Update Documentation

**Files:**
- Modify: `README.md` (add count.* and reverse hydrate docs)

**Step 1: Add to Query Body table**

```markdown
| `count.<key>` | Count records matching reverse lookup pattern |
```

**Step 2: Add example**

```bash
# Posts with like counts
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/app.bsky.feed.post/*", "count.likes": "at://*/app.bsky.feed.like/*?subject.uri=$.uri"}'
```

**Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add count.* and reverse lookup documentation"
```

---

## Summary

| Task | Description |
|------|-------------|
| 1 | Add `at-uri` index field type to config |
| 2 | Index at-uri fields in storage layer |
| 3 | Add count/fetch by field value methods |
| 4 | Parse reverse lookup patterns in server |
| 5 | Execute count.* lookups in query handler |
| 6 | Support reverse hydration patterns |
| 7 | Create app-bsky example |
| 8 | Update documentation |
