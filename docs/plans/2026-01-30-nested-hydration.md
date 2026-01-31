# Nested Hydration Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add support for nested hydration in REST API using dot notation (e.g., `items.photo`) limited to 2 levels.

**Architecture:** Extend `hydrate_records` to process hydration keys by depth level. Level 0 keys execute first, then level 1 keys iterate over the hydrated arrays/objects and apply their patterns. All field paths are normalized to insert `value.` automatically.

**Tech Stack:** Rust, serde_json, rayon (parallel processing)

---

## Background

### Data Model (Social Grain Example)

```
social.grain.gallery          - title, description, createdAt
     │
     ├── social.grain.actor.profile (via $.did)
     │        └── avatar (blob)
     │
     └── social.grain.gallery.item (via gallery=$.uri)
              │
              └── social.grain.photo (via item field)
                       └── photo (blob)
```

### Target API

```json
{
  "collection": "social.grain.gallery",
  "sort": "createdAt:datetime:desc",
  "hydrate": {
    "author": "at://$.did/social.grain.actor.profile/self",
    "items": "at://*/social.grain.gallery.item/*?gallery=$.uri&limit=10",
    "items.photo": "at://$.item"
  },
  "blobs": {
    "author.avatar": "avatar",
    "items.photo.photo": "feed_thumbnail"
  },
  "counts": {
    "favoriteCount": "at://*/social.grain.favorite/*?subject=$.uri",
    "commentCount": "at://*/social.grain.comment/*?subject=$.uri"
  }
}
```

### Key Design Decisions

1. **Dot notation**: `items.photo` means "for each record in `items`, hydrate `photo`"
2. **Max depth**: 2 levels (1 dot maximum)
3. **Implicit `value.`**: Paths like `$.item` normalize to `$.value.item` internally
4. **Execution order**: Level 0 first, then level 1
5. **Batch fetching**: Collect all URIs at each level, fetch in parallel

---

## Task 1: Add Hydration Depth Validation

**Files:**
- Modify: `src/server.rs` (near line 247, in `hydrate_records`)

**Step 1: Add validation helper function**

Add this function before `hydrate_records`:

```rust
/// Validate hydration keys and return them sorted by depth
/// Returns error message if validation fails
fn validate_and_sort_hydration_keys<'a>(
    hydrate: &'a HashMap<String, String>,
) -> Result<Vec<(&'a str, &'a str, usize)>, String> {
    let mut keys_with_depth: Vec<(&str, &str, usize)> = Vec::new();

    for (key, pattern) in hydrate {
        let depth = key.matches('.').count();

        // Max depth is 1 (2 levels total)
        if depth > 1 {
            return Err(format!(
                "hydration key '{}' exceeds max depth of 2 levels",
                key
            ));
        }

        // If depth > 0, verify parent exists
        if depth > 0 {
            let parent = key.split('.').next().unwrap();
            if !hydrate.contains_key(parent) {
                return Err(format!(
                    "hydration key '{}' requires parent '{}' to be defined",
                    key, parent
                ));
            }
        }

        keys_with_depth.push((key.as_str(), pattern.as_str(), depth));
    }

    // Sort by depth (0 first, then 1)
    keys_with_depth.sort_by_key(|(_, _, depth)| *depth);

    Ok(keys_with_depth)
}
```

**Step 2: Test manually**

```bash
cargo build
# Start server and test with invalid request:
curl -X POST http://localhost:3000/query -H "Content-Type: application/json" -d '{
  "collection": "social.grain.gallery",
  "hydrate": {
    "items.photo": "at://$.item"
  }
}'
# Should return error about missing parent "items"
```

**Step 3: Commit**

```bash
git add src/server.rs
git commit -m "feat: add hydration depth validation"
```

---

## Task 2: Normalize Field Paths (Implicit value.)

**Files:**
- Modify: `src/server.rs`

**Step 1: Add path normalization helper**

Add this function:

```rust
/// Normalize a field path by inserting 'value.' after hydration keys
/// Example: "$.item" -> "$.value.item" for hydration patterns
/// Example: "author.avatar" -> "author.value.avatar" for blob paths
fn normalize_field_path(path: &str, hydration_keys: &[&str]) -> String {
    // For patterns starting with $. (hydration patterns)
    if path.starts_with("$.") {
        let rest = &path[2..];
        if rest.starts_with("value.") || rest == "uri" || rest == "did" || rest == "cid" {
            return path.to_string();
        }
        return format!("$.value.{}", rest);
    }

    // For blob paths like "author.avatar" or "items.photo.photo"
    let parts: Vec<&str> = path.split('.').collect();
    let mut result = Vec::new();

    for (i, part) in parts.iter().enumerate() {
        result.push(*part);
        // If this part is a hydration key and next part isn't "value", insert "value"
        if hydration_keys.contains(part) && parts.get(i + 1) != Some(&"value") {
            result.push("value");
        }
    }

    result.join(".")
}
```

**Step 2: Add unit test**

Add at the bottom of `src/server.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_field_path() {
        let keys = vec!["author", "items", "photo"];

        // Hydration patterns
        assert_eq!(normalize_field_path("$.item", &keys), "$.value.item");
        assert_eq!(normalize_field_path("$.value.item", &keys), "$.value.item");
        assert_eq!(normalize_field_path("$.uri", &keys), "$.uri");

        // Blob paths
        assert_eq!(normalize_field_path("author.avatar", &keys), "author.value.avatar");
        assert_eq!(normalize_field_path("author.value.avatar", &keys), "author.value.avatar");
        assert_eq!(normalize_field_path("items.photo.photo", &keys), "items.value.photo.value.photo");
    }
}
```

**Step 3: Run tests**

```bash
cargo test test_normalize_field_path -- --nocapture
```

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat: add field path normalization for implicit value"
```

---

## Task 3: Implement Nested Forward Hydration

**Files:**
- Modify: `src/server.rs` (in `hydrate_records` function)

**Step 1: Add nested hydration helper**

Add this function:

```rust
/// Apply forward hydration to nested records (level 1)
/// parent_key: "items", child_key: "photo", pattern: "at://$.item"
fn hydrate_nested_forward(
    records: &mut [serde_json::Value],
    parent_key: &str,
    child_key: &str,
    pattern: &str,
    blobs: &HashMap<String, String>,
    hydration_keys: &[&str],
    app: &AppState,
) {
    let store = &app.store;

    // Parse pattern to extract field reference (e.g., "$.item" -> "value.item")
    let field_path = if pattern.starts_with("at://$.") {
        let raw = &pattern[7..]; // Remove "at://$."
        if raw.starts_with("value.") {
            raw.to_string()
        } else {
            format!("value.{}", raw)
        }
    } else {
        return; // Invalid pattern for nested forward hydration
    };

    // Collect all URIs to fetch: (record_idx, nested_idx, target_uri)
    let mut lookups: Vec<(usize, usize, String)> = Vec::new();

    for (rec_idx, record) in records.iter().enumerate() {
        let nested_array = match record.get(parent_key) {
            Some(serde_json::Value::Array(arr)) => arr,
            Some(obj) if obj.is_object() => {
                // Single object, treat as array of 1
                if let Some(uri) = get_nested_str(obj, &field_path) {
                    lookups.push((rec_idx, usize::MAX, uri.to_string())); // MAX = single object
                }
                continue;
            }
            _ => continue,
        };

        for (nested_idx, nested) in nested_array.iter().enumerate() {
            if let Some(uri) = get_nested_str(nested, &field_path) {
                lookups.push((rec_idx, nested_idx, uri.to_string()));
            }
        }
    }

    // Batch fetch all target URIs in parallel
    let fetched: HashMap<String, serde_json::Value> = lookups
        .iter()
        .map(|(_, _, uri)| uri.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .par_bridge()
        .filter_map(|uri| {
            store.get_by_uri_string(&uri).ok().flatten().map(|record| {
                let did = extract_did_from_uri(&uri).unwrap_or_default();
                let mut json = build_record_json(
                    &record.uri,
                    &record.cid,
                    record.value,
                    record.indexed_at,
                    app,
                );

                // Transform blobs for this nested record
                let blob_prefix = format!("{}.{}.", parent_key, child_key);
                for (path, preset) in blobs {
                    if let Some(sub_path) = path.strip_prefix(&blob_prefix) {
                        let normalized = normalize_field_path(sub_path, hydration_keys);
                        transform_blob_at_path(&mut json, &normalized, &did, preset);
                    }
                }

                (uri, json)
            })
        })
        .collect();

    // Attach fetched records to nested items
    for (rec_idx, nested_idx, uri) in lookups {
        if let Some(hydrated) = fetched.get(&uri) {
            if nested_idx == usize::MAX {
                // Single object case
                if let Some(parent) = records[rec_idx].get_mut(parent_key) {
                    parent[child_key] = hydrated.clone();
                }
            } else if let Some(serde_json::Value::Array(arr)) =
                records[rec_idx].get_mut(parent_key)
            {
                if let Some(nested) = arr.get_mut(nested_idx) {
                    nested[child_key] = hydrated.clone();
                }
            }
        }
    }
}
```

**Step 2: Commit**

```bash
git add src/server.rs
git commit -m "feat: add nested forward hydration helper"
```

---

## Task 4: Implement Nested Reverse Hydration

**Files:**
- Modify: `src/server.rs`

**Step 1: Add nested reverse hydration helper**

```rust
/// Apply reverse hydration to nested records (level 1)
/// parent_key: "items", child_key: "comments", pattern: "at://*/collection/*?field=$.uri"
fn hydrate_nested_reverse(
    records: &mut [serde_json::Value],
    parent_key: &str,
    child_key: &str,
    lookup: &ReverseLookup,
    app: &AppState,
) {
    let store = &app.store;

    // Collect all lookups: (record_idx, nested_idx, match_value)
    let mut lookups: Vec<(usize, usize, String)> = Vec::new();

    let match_field_path = if lookup.match_field == "$.uri" {
        "uri".to_string()
    } else {
        let raw = lookup.match_field.strip_prefix("$.").unwrap_or(&lookup.match_field);
        if raw.starts_with("value.") {
            raw.to_string()
        } else {
            format!("value.{}", raw)
        }
    };

    for (rec_idx, record) in records.iter().enumerate() {
        let nested_array = match record.get(parent_key) {
            Some(serde_json::Value::Array(arr)) => arr,
            _ => continue,
        };

        for (nested_idx, nested) in nested_array.iter().enumerate() {
            let match_value = if match_field_path == "uri" {
                nested.get("uri").and_then(|v| v.as_str())
            } else {
                get_nested_str(nested, &match_field_path)
            };

            if let Some(value) = match_value {
                lookups.push((rec_idx, nested_idx, value.to_string()));
            }
        }
    }

    let limit = lookup.limit.unwrap_or(10);

    // Parallel lookup
    let results: Vec<(usize, usize, Vec<serde_json::Value>)> = lookups
        .par_iter()
        .filter_map(|(rec_idx, nested_idx, value)| {
            store
                .get_by_field_value(&lookup.collection, &lookup.field, value, limit, None)
                .ok()
                .map(|related| {
                    let json: Vec<serde_json::Value> = related
                        .into_iter()
                        .map(|r| build_record_json(&r.uri, &r.cid, r.value, r.indexed_at, app))
                        .collect();
                    (*rec_idx, *nested_idx, json)
                })
        })
        .collect();

    // Attach results
    for (rec_idx, nested_idx, related) in results {
        if let Some(serde_json::Value::Array(arr)) = records[rec_idx].get_mut(parent_key) {
            if let Some(nested) = arr.get_mut(nested_idx) {
                nested[child_key] = serde_json::Value::Array(related);
            }
        }
    }
}
```

**Step 2: Commit**

```bash
git add src/server.rs
git commit -m "feat: add nested reverse hydration helper"
```

---

## Task 5: Integrate Nested Hydration into Main Function

**Files:**
- Modify: `src/server.rs` (refactor `hydrate_records`)

**Step 1: Refactor hydrate_records to use depth levels**

Replace the `hydrate_records` function with updated version that processes levels:

```rust
fn hydrate_records(
    records: &mut [serde_json::Value],
    hydrate: &HashMap<String, String>,
    blobs: &HashMap<String, String>,
    app: &AppState,
) {
    if hydrate.is_empty() {
        return;
    }

    // Validate and sort by depth
    let sorted_keys = match validate_and_sort_hydration_keys(hydrate) {
        Ok(keys) => keys,
        Err(e) => {
            tracing::warn!("Invalid hydration config: {}", e);
            return;
        }
    };

    // Collect all hydration keys for path normalization
    let all_hydration_keys: Vec<&str> = sorted_keys.iter().map(|(k, _, _)| *k).collect();

    // Process level 0 (top-level hydrations)
    let level_0: Vec<_> = sorted_keys.iter().filter(|(_, _, d)| *d == 0).collect();

    // Separate forward and reverse patterns for level 0
    let mut forward_patterns: Vec<(&str, &str, String, String)> = Vec::new();
    let mut reverse_patterns: Vec<(&str, ReverseLookup)> = Vec::new();

    for (key, pattern, _) in &level_0 {
        if pattern.contains('*') && pattern.contains('?') {
            if let Some(lookup) = parse_reverse_lookup(pattern) {
                reverse_patterns.push((*key, lookup));
            }
        } else if let Some((collection, rkey)) = parse_hydration_pattern(pattern) {
            forward_patterns.push((*key, *pattern, collection, rkey));
        }
    }

    // Execute level 0 forward hydration (existing logic)
    execute_forward_hydration(records, &forward_patterns, blobs, &all_hydration_keys, app);

    // Execute level 0 reverse hydration (existing logic)
    execute_reverse_hydration(records, &reverse_patterns, app);

    // Process level 1 (nested hydrations)
    let level_1: Vec<_> = sorted_keys.iter().filter(|(_, _, d)| *d == 1).collect();

    for (key, pattern, _) in level_1 {
        let parts: Vec<&str> = key.split('.').collect();
        let parent_key = parts[0];
        let child_key = parts[1];

        if pattern.contains('*') && pattern.contains('?') {
            // Nested reverse hydration
            if let Some(lookup) = parse_reverse_lookup(pattern) {
                hydrate_nested_reverse(records, parent_key, child_key, &lookup, app);
            }
        } else {
            // Nested forward hydration
            hydrate_nested_forward(
                records,
                parent_key,
                child_key,
                pattern,
                blobs,
                &all_hydration_keys,
                app,
            );
        }
    }
}
```

**Step 2: Extract existing logic into helpers**

Add these helper functions (extract from existing code):

```rust
fn execute_forward_hydration(
    records: &mut [serde_json::Value],
    forward_patterns: &[(&str, &str, String, String)],
    blobs: &HashMap<String, String>,
    hydration_keys: &[&str],
    app: &AppState,
) {
    // ... existing forward hydration logic from lines 259-331 ...
    // Update blob path handling to use normalize_field_path
}

fn execute_reverse_hydration(
    records: &mut [serde_json::Value],
    reverse_patterns: &[(&str, ReverseLookup)],
    app: &AppState,
) {
    // ... existing reverse hydration logic from lines 334-383 ...
}
```

**Step 3: Test end-to-end**

```bash
cargo build
# Test with nested hydration query
curl -X POST http://localhost:3000/query -H "Content-Type: application/json" -d '{
  "collection": "social.grain.gallery",
  "limit": 5,
  "hydrate": {
    "author": "at://$.did/social.grain.actor.profile/self",
    "items": "at://*/social.grain.gallery.item/*?gallery=$.uri&limit=10",
    "items.photo": "at://$.item"
  },
  "blobs": {
    "author.avatar": "avatar",
    "items.photo.photo": "feed_thumbnail"
  }
}'
```

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat: integrate nested hydration into main hydrate_records"
```

---

## Task 6: Update Sort Path Normalization

**Files:**
- Modify: `src/server.rs` (sort parsing logic)

**Step 1: Find and update sort path parsing**

Locate the sort parsing code and apply path normalization:

```rust
// When parsing sort like "createdAt:datetime:desc"
// Normalize to "value.createdAt:datetime:desc"
fn normalize_sort_path(sort: &str) -> String {
    let parts: Vec<&str> = sort.split(':').collect();
    if parts.is_empty() {
        return sort.to_string();
    }

    let field = parts[0];
    let normalized_field = if field.starts_with("value.") {
        field.to_string()
    } else {
        format!("value.{}", field)
    };

    std::iter::once(normalized_field.as_str())
        .chain(parts.iter().skip(1).copied())
        .collect::<Vec<_>>()
        .join(":")
}
```

**Step 2: Commit**

```bash
git add src/server.rs
git commit -m "feat: normalize sort paths to implicit value"
```

---

## Task 7: Add Integration Test

**Files:**
- Create: `tests/nested_hydration.rs`

**Step 1: Write integration test**

```rust
//! Integration tests for nested hydration

use serde_json::json;

#[test]
fn test_nested_hydration_validation() {
    // Test that child without parent fails
    let hydrate = serde_json::from_value::<std::collections::HashMap<String, String>>(json!({
        "items.photo": "at://$.item"
    }))
    .unwrap();

    // This should fail validation
    // (actual test would need server running or unit test the validation function)
}

#[test]
fn test_depth_limit() {
    // Test that 3+ levels fails
    let hydrate = serde_json::from_value::<std::collections::HashMap<String, String>>(json!({
        "a": "at://$.did/col/rkey",
        "a.b": "at://$.item",
        "a.b.c": "at://$.item"
    }))
    .unwrap();

    // This should fail with depth error
}
```

**Step 2: Run tests**

```bash
cargo test nested_hydration -- --nocapture
```

**Step 3: Commit**

```bash
git add tests/nested_hydration.rs
git commit -m "test: add nested hydration integration tests"
```

---

## Summary

| Task | Description | Files |
|------|-------------|-------|
| 1 | Depth validation | `src/server.rs` |
| 2 | Path normalization | `src/server.rs` |
| 3 | Nested forward hydration | `src/server.rs` |
| 4 | Nested reverse hydration | `src/server.rs` |
| 5 | Integrate into main function | `src/server.rs` |
| 6 | Sort path normalization | `src/server.rs` |
| 7 | Integration tests | `tests/nested_hydration.rs` |

**Estimated scope:** ~300 lines of new/modified code
