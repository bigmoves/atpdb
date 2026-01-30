# ATPDB

A single executable AT Protocol indexer with backfill, search, and query support.

## Quick Start

```bash
cargo install --path .
atpdb serve
```

Server runs at `http://localhost:3000`. Query records:

```bash
# All records in a collection
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/app.bsky.feed.post"}'

# Single user's records
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://did:plc:xyz/app.bsky.feed.post"}'

# Exact record
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://did:plc:xyz/app.bsky.feed.post/abc123"}'
```

## Configuration

Configure via environment variables or the REPL.

### Collections

Only store records from collections you specify:

```bash
ATPDB_COLLECTIONS="fm.teal.alpha.feed.play,app.bsky.feed.post" atpdb serve
```

Supports wildcards: `fm.teal.*` matches all `fm.teal.` prefixed collections.

### Modes

| Mode | Description |
|------|-------------|
| `manual` | Only sync repos you explicitly add |
| `signal` | Auto-discover repos that have records in a signal collection |
| `full-network` | Crawl all repos from the relay |

```bash
ATPDB_MODE=signal ATPDB_SIGNAL_COLLECTION=fm.teal.alpha.feed.play atpdb serve
```

### Indexes

Sort query results by a field:

```bash
ATPDB_INDEXES="fm.teal.alpha.feed.play:playedAt:datetime:desc" atpdb serve
```

### Search

Enable fuzzy text search on specific fields:

```bash
ATPDB_SEARCH_FIELDS="fm.teal.alpha.feed.play:track.name" atpdb serve
```

## Query Syntax

Queries use AT-URI patterns:

| Pattern | Description |
|---------|-------------|
| `at://did:plc:xyz/collection/rkey` | Exact record |
| `at://did:plc:xyz/collection/*` | All records in collection for user |
| `at://did:plc:xyz/collection` | Same as above (shorthand) |
| `at://*/collection/*` | All records in collection across all users |
| `at://*/collection` | Same as above (shorthand) |
| `at://did:plc:xyz/*` | All records for user |
| `at://did:plc:xyz` | Same as above (shorthand) |

### Query Body (JSON)

| Field | Description |
|-------|-------------|
| `q` | The AT-URI query pattern |
| `limit` | Max records to return (default 100, max 1000) |
| `cursor` | Pagination cursor from previous response |
| `sort` | Field to sort by (requires index) |
| `search` | Fuzzy text search (requires search field config) |

```bash
# Paginated, sorted by playedAt
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/fm.teal.alpha.feed.play", "sort": "playedAt", "limit": 50}'

# Search for tracks
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/fm.teal.alpha.feed.play", "search": "midnight"}'
```

## HTTP API

| Endpoint | Description |
|----------|-------------|
| `POST /query` | Query records (JSON body) |
| `GET /health` | Health check |
| `GET /stats` | Database statistics |
| `GET /metrics` | Prometheus metrics |

## CLI REPL

Interactive mode for exploration:

```bash
atpdb repl

atp> at://*/fm.teal.alpha.feed.play
[records...]
(42 records, 12.3ms)

atp> .stats
Records: 15234
Collections: 3

atp> .help
```

## Monitoring

See [monitoring/README.md](monitoring/README.md) for Prometheus + Grafana setup.
