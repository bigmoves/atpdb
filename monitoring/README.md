# ATPDB Monitoring

## Quick Start with Docker Compose

The easiest way to get Prometheus + Grafana running:

```bash
# Start ATPDB first (in another terminal)
./target/release/atpdb serve

# Start Prometheus + Grafana
cd monitoring
docker-compose up -d
```

Then open:
- **Grafana**: http://localhost:3001 (login: admin/admin)
- **Prometheus**: http://localhost:9090

The ATPDB dashboard is auto-provisioned and ready to use.

To stop:
```bash
docker-compose down
```

## Manual Setup

### Prometheus Only

```bash
# Native install
prometheus --config.file=monitoring/prometheus.yml

# Or Docker (standalone)
docker run -d --name prometheus \
  -p 9090:9090 \
  -v $(pwd)/monitoring/prometheus-docker.yml:/etc/prometheus/prometheus.yml \
  --add-host=host.docker.internal:host-gateway \
  prom/prometheus
```

### Grafana Only

1. Import `grafana-dashboard.json` via Grafana UI (Dashboards -> Import)
2. Select your Prometheus data source
3. Dashboard will be available as "ATPDB Overview"

## Quick Smoke Test

Verify metrics are being emitted without any external tools:

```bash
# Generate some traffic
curl http://localhost:3000/health
curl http://localhost:3000/stats

# Check metrics
curl -s http://localhost:3000/metrics | grep -E "^(http_|firehose_|storage_|search_|sync_)" | head -30
```

## Available Metrics

### HTTP
- `http_requests_total{method, path, status}` - Request count
- `http_request_duration_seconds{method, path}` - Request latency

### Firehose
- `firehose_connected` - Connection state (1/0)
- `firehose_last_seq` - Last sequence number
- `firehose_operations_total{op}` - Operations (create/update/delete)
- `firehose_errors_total{error}` - Connection errors

### Storage
- `storage_records_total` - Total record count
- `storage_collections_total` - Number of collections
- `storage_disk_bytes` - Database disk usage
- `storage_repos_total{status}` - Repos by status
- `storage_operation_seconds{op}` - Operation latency

### Search
- `search_queries_total` - Total searches
- `search_query_seconds` - Search latency
- `search_reindex_records_total` - Records reindexed
- `search_disk_bytes` - Search index disk usage

### Sync
- `sync_queue_depth` - Pending syncs
- `sync_workers_active` - Active sync workers
- `sync_completed_total{status}` - Completed syncs
- `sync_records_total` - Records synced
- `sync_duration_seconds` - Sync duration
- `sync_repo_size_bytes` - CAR file sizes
