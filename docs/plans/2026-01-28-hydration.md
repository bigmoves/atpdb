# Hydration Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add hydration support to query API - embed related records (like author profiles) and transform blob refs to CDN URLs.

**Architecture:** Extend QueryRequest with `hydrate` and `blobs` parameters. After fetching main results, batch-lookup hydrated records by DID, transform blob fields to CDN URLs, and embed in response.

**Tech Stack:** Rust, serde_json for JSON manipulation, existing Store for lookups

---

### Task 1: Extend QueryRequest with Hydration Fields

**Files:**
- Modify: `src/server.rs:64-73`

**Step 1: Update QueryRequest struct**

Add hydrate and blobs fields to the existing QueryRequest:

```rust
#[derive(Deserialize)]
pub struct QueryRequest {
    q: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    hydrate: std::collections::HashMap<String, String>,  // key -> "at://$.did/collection/rkey"
    #[serde(default)]
    blobs: std::collections::HashMap<String, String>,    // path -> preset (avatar, banner, feed_thumbnail)
}
```

**Step 2: Verify it compiles**

Run: `cargo build`
Expected: Compiles with no errors (new fields have defaults, so existing requests still work)

**Step 3: Commit**

```bash
git add src/server.rs
git commit -m "feat: add hydrate and blobs fields to QueryRequest"
```

---

### Task 2: Add Hydration Helper Functions

**Files:**
- Modify: `src/server.rs` (add new functions after extract_field_value)

**Step 1: Add DID extraction helper**

```rust
/// Extract DID from an AT-URI string
fn extract_did_from_uri(uri: &str) -> Option<String> {
    // at://did:plc:xyz/collection/rkey -> did:plc:xyz
    uri.strip_prefix("at://")
        .and_then(|s| s.split('/').next())
        .map(|s| s.to_string())
}
```

**Step 2: Add hydration pattern parser**

```rust
/// Parse hydration pattern like "at://$.did/app.bsky.actor.profile/self"
/// Returns (collection, rkey) if pattern uses $.did
fn parse_hydration_pattern(pattern: &str) -> Option<(String, String)> {
    // Only support $.did patterns for v1
    let pattern = pattern.strip_prefix("at://$.did/")?;
    let parts: Vec<&str> = pattern.splitn(2, '/').collect();
    if parts.len() == 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}
```

**Step 3: Verify it compiles**

Run: `cargo build`

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat: add hydration helper functions"
```

---

### Task 3: Add Blob Transformation Function

**Files:**
- Modify: `src/server.rs` (add after hydration helpers)

**Step 1: Add blob transformation function**

```rust
/// Transform a blob ref to include CDN URL
/// Blob refs look like: {"$type": "blob", "ref": {"$link": "bafkrei..."}, ...}
fn transform_blob(blob: &mut serde_json::Value, did: &str, preset: &str) {
    if let Some(cid) = blob
        .get("ref")
        .and_then(|r| r.get("$link"))
        .and_then(|l| l.as_str())
    {
        let url = format!(
            "https://cdn.bsky.app/img/{}/plain/{}/{}@jpeg",
            preset, did, cid
        );
        blob["url"] = serde_json::Value::String(url);
    }
}

/// Walk a JSON value at a dot-separated path and transform blob if found
fn transform_blob_at_path(value: &mut serde_json::Value, path: &str, did: &str, preset: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Last part - this should be the blob field
            if let Some(blob) = current.get_mut(*part) {
                if blob.get("$type").and_then(|t| t.as_str()) == Some("blob") {
                    transform_blob(blob, did, preset);
                }
            }
        } else {
            // Navigate deeper
            match current.get_mut(*part) {
                Some(next) => current = next,
                None => return,
            }
        }
    }
}
```

**Step 2: Verify it compiles**

Run: `cargo build`

**Step 3: Commit**

```bash
git add src/server.rs
git commit -m "feat: add blob transformation functions"
```

---

### Task 4: Add Batch Hydration Function

**Files:**
- Modify: `src/server.rs`
- Modify: `src/storage.rs` (add get_by_string_uri helper)

**Step 1: Add string URI lookup to Store**

In `src/storage.rs`, add a helper that takes a string URI:

```rust
/// Get record by string URI (parses the URI first)
pub fn get_by_uri_string(&self, uri: &str) -> Result<Option<Record>, StorageError> {
    match uri.parse::<AtUri>() {
        Ok(parsed) => self.get(&parsed),
        Err(_) => Ok(None),
    }
}
```

**Step 2: Add batch hydration function to server.rs**

```rust
/// Hydrate records with related data
fn hydrate_records(
    records: &mut [serde_json::Value],
    hydrate: &std::collections::HashMap<String, String>,
    blobs: &std::collections::HashMap<String, String>,
    store: &crate::storage::Store,
) {
    // Collect all DIDs and their hydration targets
    let mut hydration_lookups: std::collections::HashMap<String, Vec<(usize, String)>> =
        std::collections::HashMap::new();

    for (idx, record) in records.iter().enumerate() {
        if let Some(uri) = record.get("uri").and_then(|u| u.as_str()) {
            if let Some(did) = extract_did_from_uri(uri) {
                for (key, pattern) in hydrate {
                    if let Some((collection, rkey)) = parse_hydration_pattern(pattern) {
                        let target_uri = format!("at://{}/{}/{}", did, collection, rkey);
                        hydration_lookups
                            .entry(target_uri)
                            .or_default()
                            .push((idx, key.clone()));
                    }
                }
            }
        }
    }

    // Batch lookup all hydration targets
    let mut hydrated_records: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();

    for target_uri in hydration_lookups.keys() {
        if let Ok(Some(record)) = store.get_by_uri_string(target_uri) {
            let did = extract_did_from_uri(target_uri).unwrap_or_default();
            let mut record_json = serde_json::json!({
                "uri": record.uri,
                "cid": record.cid,
                "value": record.value,
                "indexedAt": record.indexed_at
            });

            // Transform blobs in hydrated record
            for (path, preset) in blobs {
                // Check if path starts with any hydration key
                for key in hydrate.keys() {
                    if let Some(sub_path) = path.strip_prefix(&format!("{}.", key)) {
                        transform_blob_at_path(&mut record_json["value"], sub_path, &did, preset);
                    }
                }
            }

            hydrated_records.insert(target_uri.clone(), record_json);
        }
    }

    // Attach hydrated records to main records
    for (target_uri, indices) in hydration_lookups {
        if let Some(hydrated) = hydrated_records.get(&target_uri) {
            for (idx, key) in indices {
                if let Some(record) = records.get_mut(idx) {
                    record[&key] = hydrated.clone();
                }
            }
        }
    }
}
```

**Step 3: Verify it compiles**

Run: `cargo build`

**Step 4: Commit**

```bash
git add src/server.rs src/storage.rs
git commit -m "feat: add batch hydration function"
```

---

### Task 5: Integrate Hydration into Query Handler

**Files:**
- Modify: `src/server.rs` (query_handler function)

**Step 1: Find the query_handler response building section**

Look for where `QueryResponse` is built (around line 327-342). After records are collected but before returning, add hydration call.

**Step 2: Add hydration call before building response**

After the line `let records: Vec<_> = records.into_iter().take(limit).collect();` and before building the response JSON, add:

```rust
// Convert records to JSON values
let mut values: Vec<serde_json::Value> = records
    .into_iter()
    .map(|r| {
        serde_json::json!({
            "uri": r.uri,
            "cid": r.cid,
            "value": r.value,
            "indexedAt": r.indexed_at
        })
    })
    .collect();

// Apply hydration if requested
if !payload.hydrate.is_empty() || !payload.blobs.is_empty() {
    hydrate_records(&mut values, &payload.hydrate, &payload.blobs, &app.store);
}

return Ok(Json(QueryResponse {
    records: values,
    cursor: next_cursor,
}));
```

**Step 3: Do the same for unsorted queries**

Find the `execute_unsorted_query` function and add the same hydration logic before returning.

**Step 4: Verify it compiles**

Run: `cargo build`

**Step 5: Commit**

```bash
git add src/server.rs
git commit -m "feat: integrate hydration into query handler"
```

---

### Task 6: Manual Integration Test

**Step 1: Start server with test data**

```bash
ATPDB_INDEXES=fm.teal.alpha.feed.play:playedTime:datetime cargo run -- serve
```

**Step 2: Test hydration query**

```bash
curl -X POST http://localhost:3000/query \
  -H "Content-Type: application/json" \
  -d '{
    "q": "at://*/fm.teal.alpha.feed.play/*",
    "limit": 2,
    "sort": "value.playedTime:datetime:desc",
    "hydrate": {
      "author": "at://$.did/app.bsky.actor.profile/self"
    },
    "blobs": {
      "author.value.avatar": "avatar",
      "author.value.banner": "banner"
    }
  }' | jq
```

**Step 3: Verify response structure**

Expected response should have `author` field in each record with profile data and blob URLs:

```json
{
  "records": [
    {
      "uri": "at://did:plc:xxx/fm.teal.alpha.feed.play/123",
      "value": {...},
      "author": {
        "uri": "at://did:plc:xxx/app.bsky.actor.profile/self",
        "value": {
          "displayName": "...",
          "avatar": {
            "$type": "blob",
            "ref": {"$link": "bafkrei..."},
            "url": "https://cdn.bsky.app/img/avatar/plain/did:plc:xxx/bafkrei...@jpeg"
          }
        }
      }
    }
  ]
}
```

**Step 4: Test without hydration (should still work)**

```bash
curl -X POST http://localhost:3000/query \
  -H "Content-Type: application/json" \
  -d '{"q": "at://*/fm.teal.alpha.feed.play/*", "limit": 2}' | jq
```

**Step 5: Commit**

```bash
git add -A
git commit -m "feat: complete hydration implementation"
```

---

### Task 7: Update Example HTML to Use Hydration

**Files:**
- Modify: `examples/fm-teal/index.html`

**Step 1: Update the fetch request to include hydration**

Find the `fetchPage` function and update the body:

```javascript
const body = {
    q: 'at://*/fm.teal.alpha.feed.play/*',
    limit: PAGE_SIZE,
    sort: 'value.playedTime:datetime:desc',
    hydrate: {
        author: 'at://$.did/app.bsky.actor.profile/self'
    },
    blobs: {
        'author.value.avatar': 'avatar'
    }
};
```

**Step 2: Update renderPlay to use hydrated profile**

```javascript
function renderPlay(record) {
    const did = extractDidFromUri(record.uri);
    const value = record.value;
    const author = record.author;

    const trackName = value.trackName || 'Unknown Track';
    const artistName = value.artists?.map(a => a.artistName).join(', ') || 'Unknown Artist';
    const albumName = value.releaseName || '';

    // Use hydrated profile data
    const displayName = author?.value?.displayName || truncateDid(did);
    const avatarUrl = author?.value?.avatar?.url;

    const avatarHtml = avatarUrl
        ? `<img src="${avatarUrl}" class="avatar" alt="${displayName}">`
        : `<div class="avatar">${getInitials(did)}</div>`;

    return `
        <div class="play-card">
            <div class="play-header">
                ${avatarHtml}
                <div>
                    <div class="display-name">${escapeHtml(displayName)}</div>
                    <div class="did">${truncateDid(did)}</div>
                </div>
            </div>
            <div class="track-info">
                <div class="track-name">${escapeHtml(trackName)}</div>
                <div class="track-artist">${escapeHtml(artistName)}</div>
                ${albumName ? `<div class="track-album">${escapeHtml(albumName)}</div>` : ''}
            </div>
            <div class="timestamp">${value.playedTime ? new Date(value.playedTime).toLocaleString() : formatTime(record.indexedAt)}</div>
        </div>
    `;
}
```

**Step 3: Add CSS for avatar image**

```css
.avatar {
    width: 40px;
    height: 40px;
    border-radius: 50%;
    background: linear-gradient(135deg, #14b8a6, #0d9488);
    display: flex;
    align-items: center;
    justify-content: center;
    font-weight: bold;
    font-size: 1rem;
    object-fit: cover;
}

.display-name {
    font-weight: 600;
    font-size: 0.9rem;
}
```

**Step 4: Test in browser**

Open `examples/fm-teal/index.html` in browser with server running.

**Step 5: Commit**

```bash
git add examples/fm-teal/index.html
git commit -m "feat: update example to use hydration with avatars"
```

---

Plan complete and saved to `docs/plans/2026-01-28-hydration.md`. Two execution options:

**1. Subagent-Driven (this session)** - I dispatch fresh subagent per task, review between tasks, fast iteration

**2. Parallel Session (separate)** - Open new session with executing-plans, batch execution with checkpoints

Which approach?
