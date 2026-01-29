# Unified Query Endpoint Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Consolidate `/search` into `/query` endpoint with full query string support, enabling REPL-style queries like `at://*/collection/*?search.field=value&sort=field:type:dir`.

**Architecture:** Parse `q` parameter to extract base AT URI and query string params. Merge with explicit JSON fields (JSON takes precedence). Support `search.*`, `sort`, `hydrate.*` in query string. When search fields present, use Tantivy for relevance ranking. Remove separate `/search` endpoint.

**Field path convention:** All field references (sort, search) assume fields are within the record's `value` object. The `value.` prefix is implicit and not required. Exception: `indexedAt` is a known top-level field.

**Tech Stack:** Rust, Axum, Tantivy, serde, url crate for query string parsing

---

### Task 1: Add Query String Parsing to `q` Parameter

**Files:**
- Modify: `src/server.rs` (add helper function)

**Step 1: Add helper to parse AT URI with query string**

```rust
/// Parse an AT URI that may contain query string params
/// e.g. "at://*/collection/*?search.trackName=value&sort=field:type:desc"
/// Returns (base_uri, params)
fn parse_query_uri(uri: &str) -> (String, std::collections::HashMap<String, String>) {
    let mut params = std::collections::HashMap::new();

    // Split on '?' to separate base URI from query string
    let (base, query_string) = match uri.split_once('?') {
        Some((base, qs)) => (base.to_string(), Some(qs)),
        None => (uri.to_string(), None),
    };

    // Parse query string params
    if let Some(qs) = query_string {
        for pair in qs.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                // URL decode the value
                let decoded = urlencoding::decode(value).unwrap_or_else(|_| value.into());
                params.insert(key.to_string(), decoded.into_owned());
            }
        }
    }

    (base, params)
}
```

**Step 2: Add helper to normalize field paths**

```rust
/// Normalize a field path - adds "value." prefix unless it's a known top-level field
fn normalize_field_path(field: &str) -> String {
    const TOP_LEVEL_FIELDS: &[&str] = &["indexedAt", "uri", "cid", "did"];

    if TOP_LEVEL_FIELDS.contains(&field.split(':').next().unwrap_or(field)) {
        field.to_string()
    } else if field.starts_with("value.") {
        field.to_string()  // Already has prefix
    } else {
        format!("value.{}", field)
    }
}
```

**Step 3: Add urlencoding dependency to Cargo.toml**

```toml
urlencoding = "2"
```

**Step 4: Commit**

```bash
git add src/server.rs Cargo.toml
git commit -m "feat: add query string parsing for AT URIs"
```

---

### Task 2: Extend QueryRequest to Accept Search Fields

**Files:**
- Modify: `src/server.rs:233-248` (QueryRequest struct)

**Step 1: Add search_fields to QueryRequest**

Add flattened search fields map and optional collection shorthand to QueryRequest:

```rust
#[derive(Deserialize)]
pub struct QueryRequest {
    #[serde(default)]
    q: Option<String>,  // Made optional - can use collection shorthand instead
    #[serde(default)]
    collection: Option<String>,  // Shorthand: expands to at://*/collection/*
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    hydrate: std::collections::HashMap<String, String>,
    #[serde(default)]
    blobs: std::collections::HashMap<String, String>,
    #[serde(default)]
    include_count: bool,
    #[serde(flatten)]
    search_fields: std::collections::HashMap<String, serde_json::Value>,  // Captures search.* fields
}
```

**Step 2: Commit**

```bash
git add src/server.rs
git commit -m "feat: extend QueryRequest for search fields"
```

---

### Task 3: Add Search Logic to Query Handler

**Files:**
- Modify: `src/server.rs:419-578` (query_handler function)

**Step 1: Add helper to extract search fields from both sources**

Add this helper function before `query_handler`:

```rust
/// Extract search.* fields from the flattened map
fn extract_search_fields(fields: &std::collections::HashMap<String, serde_json::Value>) -> std::collections::HashMap<String, String> {
    fields
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("search.")
                .and_then(|field| v.as_str().map(|query| (field.to_string(), query.to_string())))
        })
        .collect()
}
```

**Step 2: Modify query_handler to parse query string and merge params**

At the start of `query_handler`, parse `q` for embedded params and merge with JSON fields:

```rust
async fn query_handler(
    State((app, _)): State<AppStateHandle>,
    Json(payload): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Resolve query: use q if provided, otherwise expand collection shorthand
    let raw_query = match (&payload.q, &payload.collection) {
        (Some(q), _) => q.clone(),
        (None, Some(collection)) => format!("at://*/{}/*", collection),
        (None, None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Either 'q' or 'collection' is required".to_string(),
                }),
            ));
        }
    };

    // Parse query string params from the URI (e.g., at://...?search.field=value&sort=...)
    let (base_uri, uri_params) = parse_query_uri(&raw_query);

    let parsed = Query::parse(&base_uri).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;

    // Merge sort: JSON field takes precedence over URI param
    // Normalize field path (adds value. prefix unless it's indexedAt or already prefixed)
    let sort = payload.sort.clone()
        .or_else(|| uri_params.get("sort").cloned())
        .map(|s| {
            // Parse sort spec: field:type:direction
            let parts: Vec<&str> = s.split(':').collect();
            if parts.is_empty() {
                return s;
            }
            let normalized_field = normalize_field_path(parts[0]);
            let rest: Vec<&str> = parts.into_iter().skip(1).collect();
            if rest.is_empty() {
                normalized_field
            } else {
                format!("{}:{}", normalized_field, rest.join(":"))
            }
        });

    // Merge hydrate: JSON field takes precedence, then check URI params for hydrate.*
    let mut hydrate = payload.hydrate.clone();
    for (key, value) in &uri_params {
        if let Some(field) = key.strip_prefix("hydrate.") {
            hydrate.entry(field.to_string()).or_insert_with(|| value.clone());
        }
    }

    // ... rest of handler
```

**Step 3: Extract and merge search fields**

After resolving params, merge search fields from both sources:

```rust
    let limit = payload
        .limit
        .unwrap_or(DEFAULT_QUERY_LIMIT)
        .min(MAX_QUERY_LIMIT);
    let config = app.config();

    // Extract search fields from JSON body
    let mut search_fields = extract_search_fields(&payload.search_fields);

    // Merge search fields from URI params (JSON takes precedence)
    for (key, value) in &uri_params {
        if let Some(field) = key.strip_prefix("search.") {
            search_fields.entry(field.to_string()).or_insert_with(|| value.clone());
        }
    }

    // If search fields present, use Tantivy search
    if !search_fields.is_empty() {
        return execute_search_query(
            &app,
            &parsed,
            &search_fields,
            limit,
            sort.as_deref(),  // Use merged sort
            &hydrate,         // Use merged hydrate
            &payload.blobs,
        )
        .await;
    }

    // Handle sorted queries using indexes (existing code continues...)
    // Note: Update existing code to use `sort` and `hydrate` instead of payload.sort/payload.hydrate
```

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat: add query string parsing and search detection to query handler"
```

---

### Task 4: Implement execute_search_query Function

**Files:**
- Modify: `src/server.rs` (add new function after `execute_unsorted_query`)

**Step 1: Add the search query execution function**

```rust
async fn execute_search_query(
    app: &AppState,
    parsed: &Query,
    search_fields: &std::collections::HashMap<String, String>,
    limit: usize,
    sort: Option<&str>,
    hydrate: &std::collections::HashMap<String, String>,
    blobs: &std::collections::HashMap<String, String>,
) -> Result<Json<QueryResponse>, (StatusCode, Json<ErrorResponse>)> {
    let search = app.search.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Search is not enabled".to_string(),
            }),
        )
    })?;

    let config = app.config();

    // Get collection from query
    let collection = match parsed {
        Query::AllCollection { collection } => collection.as_str(),
        Query::Collection { collection, .. } => collection.as_str(),
        Query::Exact(uri) => uri.collection.as_str(),
    };

    // Overfetch when sorting to ensure enough records after re-ranking
    let fetch_limit = if sort.is_some() {
        (limit * 3).min(MAX_QUERY_LIMIT)
    } else {
        limit
    };

    // Search each field and collect URIs
    let mut uris: Vec<String> = Vec::new();
    for (field, query) in search_fields {
        // Verify field is configured
        if !config.is_field_searchable(collection, field) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!(
                        "Field '{}' is not searchable for collection '{}'. Configure ATPDB_SEARCH_FIELDS={}:{}",
                        field, collection, collection, field
                    ),
                }),
            ));
        }

        let results = search
            .search(collection, field, query, fetch_limit, 0)
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

    // Deduplicate URIs while preserving relevance order
    let mut seen = std::collections::HashSet::new();
    uris.retain(|uri| seen.insert(uri.clone()));
    if sort.is_none() {
        uris.truncate(limit);
    }

    // Hydrate records from Fjall
    let mut records: Vec<serde_json::Value> = Vec::with_capacity(uris.len());
    for uri in &uris {
        if let Ok(Some(record)) = app.store.get_by_uri_string(uri) {
            let mut record_json = build_record_json(
                &record.uri,
                &record.cid,
                record.value,
                record.indexed_at,
                app,
            );

            // Transform blobs
            let did = extract_did_from_uri(uri).unwrap_or_default();
            for (path, preset) in blobs {
                transform_blob_at_path(&mut record_json, path, &did, preset);
            }

            records.push(record_json);
        }
    }

    // Sort records if requested
    if let Some(sort_spec) = sort {
        let parts: Vec<&str> = sort_spec.split(':').collect();
        let field_path = parts.first().unwrap_or(&"indexedAt");
        let field_type = parts.get(1).unwrap_or(&"string");
        let order = parts.get(2).unwrap_or(&"desc");
        let descending = *order != "asc";

        records.sort_by(|a, b| {
            let val_a = get_json_field(a, field_path);
            let val_b = get_json_field(b, field_path);

            let cmp = match *field_type {
                "datetime" => {
                    let time_a = val_a.and_then(|v| v.as_str()).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                    let time_b = val_b.and_then(|v| v.as_str()).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                    time_a.cmp(&time_b)
                }
                "number" => {
                    let num_a = val_a.and_then(|v| v.as_f64());
                    let num_b = val_b.and_then(|v| v.as_f64());
                    num_a.partial_cmp(&num_b).unwrap_or(std::cmp::Ordering::Equal)
                }
                _ => {
                    let str_a = val_a.and_then(|v| v.as_str()).unwrap_or("");
                    let str_b = val_b.and_then(|v| v.as_str()).unwrap_or("");
                    str_a.cmp(str_b)
                }
            };

            if descending { cmp.reverse() } else { cmp }
        });

        records.truncate(limit);
    }

    // Apply hydration if requested
    if !hydrate.is_empty() || !blobs.is_empty() {
        hydrate_records(&mut records, hydrate, blobs, app);
    }

    Ok(Json(QueryResponse {
        records,
        cursor: None,  // Search doesn't support cursor pagination yet
        total: Some(uris.len()),
    }))
}
```

**Step 2: Commit**

```bash
git add src/server.rs
git commit -m "feat: implement execute_search_query function"
```

---

### Task 5: Remove /search Endpoint

**Files:**
- Modify: `src/server.rs:287-303` (remove SearchRequest struct)
- Modify: `src/server.rs:305-310` (remove SearchResponse struct)
- Modify: `src/server.rs:671-803` (remove search_handler function)
- Modify: `src/server.rs:1259` (remove /search route)

**Step 1: Remove SearchRequest and SearchResponse structs**

Delete these structs (lines 287-310):

```rust
// DELETE THIS:
#[derive(Deserialize)]
pub struct SearchRequest {
    collection: String,
    #[serde(flatten)]
    search_fields: std::collections::HashMap<String, String>,
    // ... rest
}

#[derive(Serialize)]
pub struct SearchResponse {
    records: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
}
```

**Step 2: Remove search_handler function**

Delete the entire `search_handler` function (lines 671-803).

**Step 3: Remove /search route**

In the router definition, remove:

```rust
.route("/search", post(search_handler))
```

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat: remove deprecated /search endpoint"
```

---

### Task 6: Update Frontend to Use Unified /query and Show Full Query String

**Files:**
- Modify: `examples/fm-teal/index.html` (add search-filter element, helper function, update loadFeed and doSearch)

**Step 1: Add helper function to build query display string**

Add after `escapeHtml` function:

```javascript
function buildQueryDisplayString(baseUri, params) {
    // Build a query string representation for display
    // params is an object like { sort: '...', 'search.field': '...', 'hydrate.key': '...' }
    const parts = [];
    for (const [key, value] of Object.entries(params)) {
        if (value !== undefined && value !== null) {
            parts.push(`${key}=${encodeURIComponent(value)}`);
        }
    }
    return parts.length > 0 ? `${baseUri}?${parts.join('&')}` : baseUri;
}
```

**Step 2: Add search-filter element in HTML**

After the query-box div, add a hidden search-filter div:

```html
<div class="query-box" id="query-display">
    at://*/fm.teal.alpha.feed.play/*
</div>

<div class="search-result-info" id="search-filter" style="display: none;">
    <span>Filtering: <span class="query" id="search-filter-text"></span></span>
    <button onclick="clearSearch()">Clear</button>
</div>
```

**Step 3: Update loadFeed to show full query string**

In `fetchPage` function, after building the request body, update the query display:

```javascript
async function fetchPage(isFirstPage) {
    const feedEl = document.getElementById('feed');
    const statsEl = document.getElementById('record-count');
    const timeEl = document.getElementById('query-time');
    const loadMoreContainer = document.getElementById('load-more-container');
    const queryDisplay = document.getElementById('query-display');
    const apiUrl = document.getElementById('api-url').value;

    const startTime = performance.now();
    const baseUri = 'at://*/fm.teal.alpha.feed.play/*';

    try {
        const body = {
            q: baseUri,
            limit: PAGE_SIZE,
            sort: 'playedTime:datetime:desc',
            hydrate: {
                author: 'at://$.did/app.bsky.actor.profile/self'
            },
            blobs: {
                'author.value.avatar': 'avatar'
            },
            include_count: isFirstPage
        };
        if (currentCursor) {
            body.cursor = currentCursor;
        }

        // Update query display to show full query (on first page only)
        if (isFirstPage) {
            queryDisplay.textContent = buildQueryDisplayString(baseUri, {
                sort: body.sort,
                'hydrate.author': body.hydrate.author
            });
        }

        const response = await fetch(`${apiUrl}/query`, {
            // ... rest unchanged
```

**Step 4: Update doSearch to use /query endpoint and show full query**

```javascript
async function doSearch() {
    const searchInput = document.getElementById('search-input');
    const searchField = document.getElementById('search-field');
    const query = searchInput.value.trim();

    if (!query) {
        return;
    }

    isSearchMode = true;
    const feedEl = document.getElementById('feed');
    const statsEl = document.getElementById('record-count');
    const timeEl = document.getElementById('query-time');
    const loadMoreContainer = document.getElementById('load-more-container');
    const queryDisplay = document.getElementById('query-display');
    const searchFilter = document.getElementById('search-filter');
    const searchFilterText = document.getElementById('search-filter-text');
    const apiUrl = document.getElementById('api-url').value;

    feedEl.innerHTML = '<div class="loading">Searching...</div>';
    loadMoreContainer.style.display = 'none';

    const fieldName = searchField.value;
    const fieldLabel = searchField.options[searchField.selectedIndex].text;
    const baseUri = 'at://*/fm.teal.alpha.feed.play/*';

    // Update the query-box to show full query string
    queryDisplay.textContent = buildQueryDisplayString(baseUri, {
        [`search.${fieldName}`]: query,
        'hydrate.author': 'at://$.did/app.bsky.actor.profile/self'
    });

    // Show the search filter info with Clear button
    searchFilterText.textContent = `${fieldLabel} = "${query}"`;
    searchFilter.style.display = 'flex';

    const startTime = performance.now();

    try {
        const body = {
            q: baseUri,
            [`search.${fieldName}`]: query,
            limit: 50,
            hydrate: {
                author: 'at://$.did/app.bsky.actor.profile/self'
            },
            blobs: {
                'author.value.avatar': 'avatar'
            }
        };

        const response = await fetch(`${apiUrl}/query`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify(body)
        });

        const endTime = performance.now();

        if (!response.ok) {
            const error = await response.json();
            throw new Error(error.error || 'Search failed');
        }

        const data = await response.json();

        statsEl.textContent = `${data.total || data.records.length} results`;
        timeEl.textContent = `(${(endTime - startTime).toFixed(0)}ms)`;

        if (data.records.length === 0) {
            feedEl.innerHTML = `<div class="loading">No results found for "${escapeHtml(query)}"</div>`;
            return;
        }

        feedEl.innerHTML = data.records.map(renderPlay).join('');

    } catch (err) {
        feedEl.innerHTML = `<div class="error">
            <strong>Search Error:</strong> ${escapeHtml(err.message)}<br><br>
            Make sure search is configured: <code>ATPDB_SEARCH_FIELDS=fm.teal.alpha.feed.play:trackName,fm.teal.alpha.feed.play:releaseName,fm.teal.alpha.feed.play:artists.*.artistName</code>
        </div>`;
        statsEl.textContent = 'Error';
        timeEl.textContent = '';
    }
}
```

**Step 5: Update clearSearch to hide filter and reset query display**

```javascript
function clearSearch() {
    isSearchMode = false;
    document.getElementById('search-input').value = '';
    document.getElementById('search-filter').style.display = 'none';
    // Query display will be updated by loadFeed
    loadFeed();
}
```

**Step 6: Commit**

```bash
git add examples/fm-teal/index.html
git commit -m "feat: update frontend to use unified /query endpoint with full query display"
```

---

### Task 7: Final Commit and Cleanup

**Step 1: Run clippy to check for warnings**

```bash
cargo clippy --all-targets
```

**Step 2: Build and verify**

```bash
cargo build --release
```

**Step 3: Manual test**

Start server and test API:
- Regular query: `curl -X POST http://localhost:3000/query -H 'Content-Type: application/json' -d '{"q":"at://*/fm.teal.alpha.feed.play/*","limit":5}'`
- Search via JSON fields: `curl -X POST http://localhost:3000/query -H 'Content-Type: application/json' -d '{"q":"at://*/fm.teal.alpha.feed.play/*","search.trackName":"oh sees","limit":5}'`
- **Search via query string (REPL style)**: `curl -X POST http://localhost:3000/query -H 'Content-Type: application/json' -d '{"q":"at://*/fm.teal.alpha.feed.play/*?search.trackName=oh%20sees","limit":5}'`
- **Query string with sort**: `curl -X POST http://localhost:3000/query -H 'Content-Type: application/json' -d '{"q":"at://*/fm.teal.alpha.feed.play/*?sort=playedTime:datetime:desc","limit":5}'`
- Verify /search returns 404

Test frontend (open examples/fm-teal/index.html):
- On load, query-box shows: `at://*/fm.teal.alpha.feed.play/*?sort=playedTime:datetime:desc&hydrate.author=at://$.did/app.bsky.actor.profile/self`
- Search for "oh sees", query-box shows: `at://*/fm.teal.alpha.feed.play/*?search.trackName=oh%20sees&hydrate.author=at://$.did/app.bsky.actor.profile/self`
- Search filter info box appears with "Track = oh sees" and Clear button
- Click Clear, filter box hides, query-box resets to feed query

**Step 4: Final commit if any fixes needed**

```bash
git add -A
git commit -m "chore: cleanup after unified query endpoint"
```
