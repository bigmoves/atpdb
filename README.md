# ATPDB

A single executable AT Protocol indexer with backfill, search, and query support.

> **Warning:** API is still evolving and may change without notice.

## Quick Start

Index [teal.fm](https://teal.fm) music plays:

```bash
ATPDB_MODE=signal \
ATPDB_SIGNAL_COLLECTION=fm.teal.alpha.feed.play \
ATPDB_COLLECTIONS=fm.teal.alpha.feed.play,app.bsky.actor.profile \
cargo run -- serve
```

Server runs at `http://localhost:3000`. Query records:

```bash
# All plays
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/fm.teal.alpha.feed.play"}'

# Single user's plays
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://did:plc:xyz/fm.teal.alpha.feed.play"}'
```

See [examples/fm-teal](examples/fm-teal) for a full demo UI.

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
| `search.<field>` | Fuzzy text search on field (requires search field config) |
| `hydrate.<key>` | Attach related record at `<key>` using pattern `at://$.did/collection/rkey` |
| `blobs.<path>` | Transform blob ref to CDN URL (presets: `avatar`, `banner`, `feed_thumbnail`) |

```bash
# Paginated, sorted by playedAt
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/fm.teal.alpha.feed.play", "sort": "playedAt", "limit": 50}'

# Search for tracks by name (field must be configured in ATPDB_SEARCH_FIELDS)
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/fm.teal.alpha.feed.play", "search.track.name": "midnight"}'

# Hydrate with user's profile and transform avatar blob
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/fm.teal.alpha.feed.play", "hydrate.author": "at://$.did/app.bsky.actor.profile/self", "blobs.author.avatar": "avatar"}'
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

## Roadmap

- [ ] OAuth authentication
- [ ] Mutations (create/update/delete records)
- [ ] Moderation support
