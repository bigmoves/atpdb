# Prometheus Metrics Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add Prometheus metrics for production monitoring and performance profiling across HTTP, firehose, storage, search, and sync components.

**Architecture:** Use the `metrics` facade crate with `metrics-exporter-prometheus` to expose a `/metrics` endpoint on the same port as the API. Instrument all layers: HTTP middleware for request metrics, inline instrumentation for firehose/storage/search/sync operations.

**Tech Stack:** `metrics 0.24`, `metrics-exporter-prometheus 0.16`, Grafana dashboard JSON

---

## Task 1: Add Dependencies

**Files:**
- Modify: `Cargo.toml`

**Step 1: Add metrics crates**

Add to `[dependencies]` section in `Cargo.toml`:

```toml
metrics = "0.24"
metrics-exporter-prometheus = "0.16"
```

**Step 2: Verify compilation**

Run: `cargo check`
Expected: Compiles successfully with new dependencies

**Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add prometheus metrics dependencies"
```

---

## Task 2: Initialize Prometheus Exporter and /metrics Endpoint

**Files:**
- Modify: `src/server.rs`

**Step 1: Add imports at top of server.rs**

Add after existing imports:

```rust
use metrics_exporter_prometheus::PrometheusHandle;
```

**Step 2: Create metrics handler function**

Add before `pub async fn run()`:

```rust
async fn metrics_handler(
    State(handle): State<PrometheusHandle>,
) -> String {
    handle.render()
}
```

**Step 3: Initialize exporter in run() function**

In `pub async fn run()`, add at the very beginning (before sync worker start):

```rust
pub async fn run(app: Arc<AppState>, port: u16, relay: Option<String>) {
    // Initialize Prometheus metrics exporter
    let prometheus_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder");
```

**Step 4: Add /metrics route**

Modify the router building section. After `.route("/config", post(config_update))`, add:

```rust
        .route("/config", post(config_update))
        .route("/metrics", get(metrics_handler))
        .layer(CompressionLayer::new())
```

And change the router to pass the prometheus handle. Replace:

```rust
    let state: AppStateHandle = (app, sync_handle);

    let router = Router::new()
```

With:

```rust
    let state: AppStateHandle = (app, sync_handle);

    let metrics_router = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(prometheus_handle);

    let router = Router::new()
```

Then merge at the end before `.layer(CompressionLayer::new())`:

```rust
        .route("/config", post(config_update))
        .merge(metrics_router)
        .layer(CompressionLayer::new())
```

**Step 5: Verify compilation and test endpoint**

Run: `cargo build --release`
Expected: Compiles successfully

Run: `./target/release/atpdb serve &` then `curl http://localhost:3000/metrics`
Expected: Returns empty Prometheus text format (just comments, no metrics yet)

**Step 6: Commit**

```bash
git add src/server.rs
git commit -m "feat(metrics): add /metrics endpoint with prometheus exporter"
```

---

## Task 3: Add HTTP Request Metrics Middleware

**Files:**
- Modify: `src/server.rs`

**Step 1: Add imports**

Add to imports:

```rust
use axum::middleware::{self, Next};
use axum::extract::Request;
use std::time::Instant;
use metrics::{counter, histogram};
```

**Step 2: Create metrics middleware function**

Add after the `metrics_handler` function:

```rust
async fn http_metrics_middleware(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = normalize_path(req.uri().path());
    let start = Instant::now();

    let response = next.run(req).await;

    let status = response.status().as_u16().to_string();
    let duration = start.elapsed().as_secs_f64();

    counter!("http_requests_total", "method" => method.clone(), "path" => path.clone(), "status" => status).increment(1);
    histogram!("http_request_duration_seconds", "method" => method, "path" => path).record(duration);

    response
}

/// Normalize paths to avoid label cardinality explosion
/// e.g., /repos/did:plc:abc123 -> /repos/{did}
fn normalize_path(path: &str) -> String {
    if path.starts_with("/repos/") && path.len() > 7 && !path.ends_with("/errors") {
        return "/repos/{did}".to_string();
    }
    path.to_string()
}
```

**Step 3: Apply middleware to router**

In the router building, add the middleware layer. After `.merge(metrics_router)`:

```rust
        .merge(metrics_router)
        .layer(middleware::from_fn(http_metrics_middleware))
        .layer(CompressionLayer::new())
```

**Step 4: Test metrics appear**

Run: `cargo build --release && ./target/release/atpdb serve &`
Run: `curl http://localhost:3000/health && curl http://localhost:3000/metrics | grep http_`
Expected: See `http_requests_total` and `http_request_duration_seconds` metrics

**Step 5: Commit**

```bash
git add src/server.rs
git commit -m "feat(metrics): add HTTP request metrics middleware"
```

---

## Task 4: Add Operation::Update to Firehose

**Files:**
- Modify: `src/firehose.rs`

ATProto sends both "create" and "update" actions. Currently we collapse them. Let's distinguish them for better metrics.

**Step 1: Add Update variant to Operation enum**

In `src/firehose.rs`, find the `Operation` enum and add `Update`:

```rust
pub enum Operation {
    Create {
        uri: AtUri,
        cid: String,
        value: serde_json::Value,
    },
    Update {
        uri: AtUri,
        cid: String,
        value: serde_json::Value,
    },
    Delete {
        uri: AtUri,
    },
}
```

**Step 2: Parse "update" action separately**

Find the match on action (around line 206). Change from:

```rust
"create" | "update" => {
```

To:

```rust
"create" => {
    // ... existing create logic ...
    operations.push(Operation::Create {
        uri,
        cid,
        value,
    });
}
"update" => {
    // Same logic as create, but push Update variant
    if let (Some(cid), Some(record)) = (op.cid, op.record) {
        let cid = cid.to_string();
        let value = crate::cbor::dagcbor_to_json(&record)
            .unwrap_or(serde_json::Value::Null);
        operations.push(Operation::Update {
            uri,
            cid,
            value,
        });
    }
}
```

**Step 3: Verify compilation**

Run: `cargo check`
Expected: Compiles (may have warnings about unhandled Update in match arms)

**Step 4: Commit**

```bash
git add src/firehose.rs
git commit -m "feat: distinguish create vs update operations in firehose"
```

---

## Task 5: Handle Operation::Update in Consumers

**Files:**
- Modify: `src/server.rs`
- Modify: `src/main.rs`

**Step 1: Update server.rs firehose handler**

Find where `Operation::Create` is matched in `start_firehose`. Add handling for `Update` (same logic as Create):

```rust
Operation::Create { uri, cid, value } | Operation::Update { uri, cid, value } => {
    // ... existing create/update logic stays the same
}
```

**Step 2: Update main.rs firehose handler**

Same pattern in `start_streaming` function in main.rs.

**Step 3: Verify compilation**

Run: `cargo build --release`
Expected: Compiles successfully

**Step 4: Commit**

```bash
git add src/server.rs src/main.rs
git commit -m "feat: handle Operation::Update in firehose consumers"
```

---

## Task 6: Add Firehose Metrics

**Files:**
- Modify: `src/server.rs` (firehose event loop is in start_firehose)

**Step 1: Add metrics imports if not present**

Ensure these are at top of server.rs:

```rust
use metrics::{counter, gauge, histogram};
```

**Step 2: Add metrics in start_firehose function**

Find the `start_firehose` function. Add metrics at key points:

After successful connection (after `FirehoseClient::connect` Ok branch):

```rust
Ok(mut client) => {
    gauge!("firehose_connected").set(1.0);
    tracing::info!("Connected to firehose: {}", relay);
```

In the event loop, after processing commits (inside `Event::Commit` handling):

```rust
Event::Commit { seq, operations, .. } => {
    gauge!("firehose_last_seq").set(seq as f64);
    counter!("firehose_events_total", "type" => "commit").increment(1);
```

After processing each operation, add operation counters:

```rust
Operation::Create { .. } => {
    counter!("firehose_operations_total", "op" => "create").increment(1);
    // ... existing code
}
Operation::Update { .. } => {
    counter!("firehose_operations_total", "op" => "update").increment(1);
    // ... existing code (same as create)
}
Operation::Delete { .. } => {
    counter!("firehose_operations_total", "op" => "delete").increment(1);
    // ... existing code
}
```

For Identity events:

```rust
Event::Identity { .. } => {
    counter!("firehose_events_total", "type" => "identity").increment(1);
    // ... existing code
}
```

On disconnect/error, set connected to 0:

```rust
Err(e) => {
    gauge!("firehose_connected").set(0.0);
    counter!("firehose_errors_total", "error" => "connection").increment(1);
    tracing::error!("Firehose error: {}", e);
    break;
}
```

**Step 3: Verify compilation**

Run: `cargo build --release`
Expected: Compiles successfully

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat(metrics): add firehose connection and event metrics"
```

---

## Task 7: Add Storage Operation Metrics

**Files:**
- Modify: `src/storage.rs`

**Step 1: Add imports at top of storage.rs**

```rust
use metrics::histogram;
use std::time::Instant;
```

**Step 2: Instrument get() method**

Find `pub fn get(&self, uri: &AtUri)`. Wrap the body:

```rust
pub fn get(&self, uri: &AtUri) -> Result<Option<Record>, StorageError> {
    let start = Instant::now();
    let key = Self::storage_key(uri.collection.as_str(), uri.did.as_str(), uri.rkey.as_str());
    let result = match self.records.get(&key)? {
        Some(bytes) => {
            let record: Record = serde_json::from_slice(&bytes)?;
            Ok(Some(record))
        }
        None => Ok(None),
    };
    histogram!("storage_operation_seconds", "op" => "get").record(start.elapsed().as_secs_f64());
    result
}
```

**Step 3: Instrument put_with_indexes() method**

At the start of `put_with_indexes`:

```rust
pub fn put_with_indexes(
    &self,
    uri: &AtUri,
    record: &Record,
    indexes: &[IndexConfig],
) -> Result<(), StorageError> {
    let start = Instant::now();
```

At the end, before `Ok(())`:

```rust
    histogram!("storage_operation_seconds", "op" => "put").record(start.elapsed().as_secs_f64());
    Ok(())
}
```

**Step 4: Instrument delete_with_indexes() method**

Same pattern - add `let start = Instant::now();` at start, and record histogram before return.

**Step 5: Verify compilation**

Run: `cargo build --release`
Expected: Compiles successfully

**Step 6: Commit**

```bash
git add src/storage.rs
git commit -m "feat(metrics): add storage operation latency metrics"
```

---

## Task 8: Add Search Metrics

**Files:**
- Modify: `src/search.rs`

**Step 1: Add imports**

```rust
use metrics::{counter, histogram};
use std::time::Instant;
```

**Step 2: Instrument search() method**

Find the `search` method. Add timing:

```rust
pub fn search(...) -> Result<Vec<String>, SearchError> {
    let start = Instant::now();
    // ... existing code ...
    let result = // existing return value
    counter!("search_queries_total").increment(1);
    histogram!("search_query_seconds").record(start.elapsed().as_secs_f64());
    result
}
```

**Step 3: Instrument reindex_from_store() method**

Add counter for records reindexed:

```rust
// After successfully indexing each record:
counter!("search_reindex_records_total").increment(1);
```

**Step 4: Verify compilation**

Run: `cargo build --release`
Expected: Compiles successfully

**Step 5: Commit**

```bash
git add src/search.rs
git commit -m "feat(metrics): add search query and reindex metrics"
```

---

## Task 9: Add Sync Worker Metrics

**Files:**
- Modify: `src/sync_worker.rs`

**Step 1: Add imports**

```rust
use metrics::{counter, gauge, histogram};
use std::time::Instant;
```

**Step 2: Add queue depth tracking**

In the `run()` method, after receiving from queue:

```rust
while let Some(did) = self.queue_rx.recv().await {
    // Track queue depth (approximate - items waiting)
    gauge!("sync_queue_depth").set(self.queue_rx.len() as f64);
```

**Step 3: Add active worker tracking**

When sync starts (after acquiring semaphore):

```rust
let permit = semaphore.clone().acquire_owned().await.unwrap();
gauge!("sync_workers_active").increment(1.0);
```

**Step 4: Add completion metrics**

Find the sync completion handling. In the success case:

```rust
Ok(Ok(sync_result)) => {
    counter!("sync_completed_total", "status" => "success").increment(1);
    counter!("sync_records_total").increment(sync_result.record_count as u64);
    // ... existing code
}
```

In the error cases:

```rust
Ok(Err(e)) => {
    counter!("sync_completed_total", "status" => "error").increment(1);
    // ... existing code
}
Err(e) => {
    counter!("sync_completed_total", "status" => "panic").increment(1);
    // ... existing code
}
```

**Step 5: Add duration tracking in sync.rs**

In `src/sync.rs`, instrument `sync_repo()`:

```rust
pub fn sync_repo(...) -> Result<SyncResult, SyncError> {
    let start = Instant::now();
    // ... existing code ...
    // Before Ok(result):
    histogram!("sync_duration_seconds").record(start.elapsed().as_secs_f64());
    histogram!("sync_repo_size_bytes").record(car_bytes.len() as f64);
    result.handle = resolved.handle;
    Ok(result)
}
```

Add import at top of sync.rs:

```rust
use metrics::histogram;
use std::time::Instant;
```

**Step 6: Decrement active workers on completion**

After the sync task completes (in the spawned task, after updating repo state):

```rust
gauge!("sync_workers_active").decrement(1.0);
drop(permit);
```

**Step 7: Verify compilation**

Run: `cargo build --release`
Expected: Compiles successfully

**Step 8: Commit**

```bash
git add src/sync_worker.rs src/sync.rs
git commit -m "feat(metrics): add sync worker queue and duration metrics"
```

---

## Task 10: Add Application Gauges

**Files:**
- Modify: `src/server.rs`

**Step 1: Create gauge update function**

Add a function to update application-level gauges:

```rust
fn update_app_gauges(app: &AppState) {
    // Total records
    if let Ok(count) = app.store.count() {
        gauge!("storage_records_total").set(count as f64);
    }

    // Repos by status
    if let Ok(repos) = app.repos.list() {
        let mut synced = 0;
        let mut pending = 0;
        let mut error = 0;
        for repo in repos {
            match repo.status {
                crate::repos::RepoStatus::Synced => synced += 1,
                crate::repos::RepoStatus::Pending => pending += 1,
                crate::repos::RepoStatus::Error => error += 1,
                _ => {}
            }
        }
        gauge!("storage_repos_total", "status" => "synced").set(synced as f64);
        gauge!("storage_repos_total", "status" => "pending").set(pending as f64);
        gauge!("storage_repos_total", "status" => "error").set(error as f64);
    }
}
```

**Step 2: Call on /stats endpoint**

In the `stats` handler, call the update function:

```rust
async fn stats(State((app, _)): State<AppStateHandle>) -> Result<Json<StatsResponse>, StatusCode> {
    update_app_gauges(&app);
    // ... existing code
}
```

**Step 3: Verify compilation**

Run: `cargo build --release`
Expected: Compiles successfully

**Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat(metrics): add application-level gauge metrics"
```

---

## Task 11: Create Monitoring Directory and Prometheus Config

**Files:**
- Create: `monitoring/prometheus.yml`
- Create: `monitoring/README.md`

**Step 1: Create monitoring directory**

```bash
mkdir -p monitoring
```

**Step 2: Create prometheus.yml**

```yaml
global:
  scrape_interval: 15s
  evaluation_interval: 15s

scrape_configs:
  - job_name: 'atpdb'
    static_configs:
      - targets: ['localhost:3000']
    metrics_path: /metrics
```

**Step 3: Create README.md**

```markdown
# ATPDB Monitoring

## Quick Start

### Prometheus

```bash
# Run Prometheus with the provided config
prometheus --config.file=monitoring/prometheus.yml
```

Or with Docker:

```bash
docker run -d --name prometheus \
  -p 9090:9090 \
  -v $(pwd)/monitoring/prometheus.yml:/etc/prometheus/prometheus.yml \
  --add-host=host.docker.internal:host-gateway \
  prom/prometheus
```

Note: If using Docker, change `localhost:3000` to `host.docker.internal:3000` in prometheus.yml.

### Grafana

1. Import `grafana-dashboard.json` via Grafana UI (Dashboards → Import)
2. Select your Prometheus data source
3. Dashboard will be available as "ATPDB Overview"

## Available Metrics

### HTTP
- `http_requests_total{method, path, status}` - Request count
- `http_request_duration_seconds{method, path}` - Request latency

### Firehose
- `firehose_connected` - Connection state (1/0)
- `firehose_last_seq` - Last sequence number
- `firehose_events_total{type}` - Events by type
- `firehose_operations_total{op}` - Operations (create/delete)
- `firehose_errors_total{error}` - Connection errors

### Storage
- `storage_records_total` - Total record count
- `storage_repos_total{status}` - Repos by status
- `storage_operation_seconds{op}` - Operation latency

### Search
- `search_queries_total` - Total searches
- `search_query_seconds` - Search latency
- `search_reindex_records_total` - Records reindexed

### Sync
- `sync_queue_depth` - Pending syncs
- `sync_workers_active` - Active sync workers
- `sync_completed_total{status}` - Completed syncs
- `sync_records_total` - Records synced
- `sync_duration_seconds` - Sync duration
- `sync_repo_size_bytes` - CAR file sizes
```

**Step 4: Commit**

```bash
git add monitoring/
git commit -m "docs: add Prometheus config and monitoring README"
```

---

## Task 12: Create Grafana Dashboard

**Files:**
- Create: `monitoring/grafana-dashboard.json`

**Step 1: Create the dashboard JSON**

Create `monitoring/grafana-dashboard.json` with the full dashboard definition. This is a large JSON file - key panels:

- Row 1: Overview (request rate, error rate, firehose status, total records)
- Row 2: HTTP Performance (latency percentiles, status codes)
- Row 3: Ingestion (firehose events/sec, sync queue, sync duration)
- Row 4: Storage (records by collection, operation latency, repos by status)

```json
{
  "annotations": {
    "list": []
  },
  "editable": true,
  "fiscalYearStartMonth": 0,
  "graphTooltip": 0,
  "id": null,
  "links": [],
  "liveNow": false,
  "panels": [
    {
      "gridPos": { "h": 4, "w": 6, "x": 0, "y": 0 },
      "id": 1,
      "options": { "colorMode": "value", "graphMode": "area", "justifyMode": "auto" },
      "targets": [
        {
          "expr": "rate(http_requests_total[5m])",
          "legendFormat": "req/s"
        }
      ],
      "title": "Request Rate",
      "type": "stat"
    },
    {
      "gridPos": { "h": 4, "w": 6, "x": 6, "y": 0 },
      "id": 2,
      "options": { "colorMode": "value", "graphMode": "area" },
      "targets": [
        {
          "expr": "rate(http_requests_total{status=~\"5..\"}[5m])",
          "legendFormat": "errors/s"
        }
      ],
      "title": "Error Rate",
      "type": "stat"
    },
    {
      "gridPos": { "h": 4, "w": 6, "x": 12, "y": 0 },
      "id": 3,
      "fieldConfig": {
        "defaults": {
          "mappings": [
            { "options": { "0": { "text": "Disconnected", "color": "red" }, "1": { "text": "Connected", "color": "green" } }, "type": "value" }
          ]
        }
      },
      "targets": [
        { "expr": "firehose_connected", "legendFormat": "status" }
      ],
      "title": "Firehose",
      "type": "stat"
    },
    {
      "gridPos": { "h": 4, "w": 6, "x": 18, "y": 0 },
      "id": 4,
      "targets": [
        { "expr": "storage_records_total", "legendFormat": "records" }
      ],
      "title": "Total Records",
      "type": "stat"
    },
    {
      "gridPos": { "h": 8, "w": 12, "x": 0, "y": 4 },
      "id": 5,
      "targets": [
        { "expr": "histogram_quantile(0.50, rate(http_request_duration_seconds_bucket[5m]))", "legendFormat": "p50" },
        { "expr": "histogram_quantile(0.95, rate(http_request_duration_seconds_bucket[5m]))", "legendFormat": "p95" },
        { "expr": "histogram_quantile(0.99, rate(http_request_duration_seconds_bucket[5m]))", "legendFormat": "p99" }
      ],
      "title": "HTTP Latency",
      "type": "timeseries"
    },
    {
      "gridPos": { "h": 8, "w": 12, "x": 12, "y": 4 },
      "id": 6,
      "targets": [
        { "expr": "rate(http_requests_total[5m])", "legendFormat": "{{status}}" }
      ],
      "title": "Requests by Status",
      "type": "timeseries"
    },
    {
      "gridPos": { "h": 8, "w": 8, "x": 0, "y": 12 },
      "id": 7,
      "targets": [
        { "expr": "rate(firehose_events_total[1m])", "legendFormat": "{{type}}" }
      ],
      "title": "Firehose Events/min",
      "type": "timeseries"
    },
    {
      "gridPos": { "h": 8, "w": 8, "x": 8, "y": 12 },
      "id": 8,
      "targets": [
        { "expr": "sync_queue_depth", "legendFormat": "queue" },
        { "expr": "sync_workers_active", "legendFormat": "active" }
      ],
      "title": "Sync Workers",
      "type": "timeseries"
    },
    {
      "gridPos": { "h": 8, "w": 8, "x": 16, "y": 12 },
      "id": 9,
      "targets": [
        { "expr": "histogram_quantile(0.95, rate(sync_duration_seconds_bucket[5m]))", "legendFormat": "p95" }
      ],
      "title": "Sync Duration p95",
      "type": "timeseries"
    },
    {
      "gridPos": { "h": 8, "w": 12, "x": 0, "y": 20 },
      "id": 10,
      "targets": [
        { "expr": "histogram_quantile(0.95, rate(storage_operation_seconds_bucket[5m]))", "legendFormat": "{{op}}" }
      ],
      "title": "Storage Latency p95",
      "type": "timeseries"
    },
    {
      "gridPos": { "h": 8, "w": 12, "x": 12, "y": 20 },
      "id": 11,
      "targets": [
        { "expr": "storage_repos_total", "legendFormat": "{{status}}" }
      ],
      "title": "Repos by Status",
      "type": "timeseries"
    }
  ],
  "refresh": "10s",
  "schemaVersion": 38,
  "style": "dark",
  "tags": ["atpdb"],
  "templating": { "list": [] },
  "time": { "from": "now-1h", "to": "now" },
  "timepicker": {},
  "timezone": "",
  "title": "ATPDB Overview",
  "uid": "atpdb-overview",
  "version": 1
}
```

**Step 2: Commit**

```bash
git add monitoring/grafana-dashboard.json
git commit -m "feat: add Grafana dashboard for ATPDB metrics"
```

---

## Task 13: Final Integration Test

**Step 1: Build and run**

```bash
cargo build --release
./target/release/atpdb serve --connect &
```

**Step 2: Generate some traffic**

```bash
curl http://localhost:3000/health
curl http://localhost:3000/stats
curl -X POST http://localhost:3000/query -H 'Content-Type: application/json' -d '{"collection": "app.bsky.feed.post"}'
```

**Step 3: Verify all metric types appear**

```bash
curl -s http://localhost:3000/metrics | grep -E "^(http_|firehose_|storage_|search_|sync_)" | head -30
```

Expected: See metrics from all categories

**Step 4: Final commit**

```bash
git add -A
git commit -m "feat: complete Prometheus metrics implementation"
```

---

## Summary of Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `http_requests_total` | Counter | method, path, status | HTTP requests |
| `http_request_duration_seconds` | Histogram | method, path | HTTP latency |
| `firehose_connected` | Gauge | - | Connection state |
| `firehose_last_seq` | Gauge | - | Last sequence |
| `firehose_events_total` | Counter | type | Events received |
| `firehose_operations_total` | Counter | op (create/update/delete) | Operations processed |
| `firehose_errors_total` | Counter | error | Errors |
| `storage_records_total` | Gauge | - | Total records |
| `storage_repos_total` | Gauge | status | Repos by status |
| `storage_operation_seconds` | Histogram | op | Storage latency |
| `search_queries_total` | Counter | - | Search count |
| `search_query_seconds` | Histogram | - | Search latency |
| `search_reindex_records_total` | Counter | - | Reindex count |
| `sync_queue_depth` | Gauge | - | Queue size |
| `sync_workers_active` | Gauge | - | Active workers |
| `sync_completed_total` | Counter | status | Completions |
| `sync_records_total` | Counter | - | Records synced |
| `sync_duration_seconds` | Histogram | - | Sync time |
| `sync_repo_size_bytes` | Histogram | - | CAR size |
