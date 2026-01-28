# Backfill System Design

Sync and resync capabilities for atpdb, inspired by indigo/tap.

## Overview

Track repos, backfill their history from PDS, stay in sync via firehose. Three modes of operation with collection filtering.

## Repo State Model

```rust
struct RepoState {
    did: String,
    status: RepoStatus,        // pending, syncing, synced, error, desync
    rev: Option<String>,       // latest known repo rev
    record_count: u64,
    last_sync: Option<u64>,    // unix timestamp
    error: Option<String>,
    retry_count: u32,
    next_retry: Option<u64>,   // unix timestamp for backoff
}

enum RepoStatus {
    Pending,      // added, waiting to sync
    Syncing,      // backfill in progress
    Synced,       // up to date
    Error,        // sync failed, will retry
    Desync,       // detected inconsistency, needs full resync
}
```

**State transitions:**
- `add repo` -> Pending
- `start backfill` -> Syncing
- `backfill complete` -> Synced
- `backfill fails` -> Error (schedule retry with backoff)
- `firehose event doesn't apply cleanly` -> Desync (queue for resync)
- `resync complete` -> Synced

**Storage:** `repos` keyspace in fjall, keyed by DID.

## Sync Worker & Queue

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│  HTTP API   │────>│ Sync Queue  │────>│ Sync Worker │
│  .sync cmd  │     │ (in-memory) │     │ (background)│
└─────────────┘     └─────────────┘     └─────────────┘
                           ^
                           │
                    ┌──────┴──────┐
                    │ Retry Timer │
                    │ (scans DB)  │
                    └─────────────┘
```

**Sync Queue:**
- Channel of DIDs to sync
- Configurable parallelism (default: 3 concurrent syncs)

**Sync Worker loop:**
1. Pull DID from queue
2. Set status -> Syncing
3. Resolve PDS, fetch CAR
4. Process records (with collection filter if set)
5. Update rev, record_count, last_sync
6. Set status -> Synced
7. On error: increment retry_count, calculate next_retry with backoff, set status -> Error

**Retry Timer:**
- Periodic scan (every 30s) for repos where `status = Error` and `next_retry < now`
- Re-queues them for sync
- Backoff: 1min -> 2min -> 4min -> 8min -> ... -> 1hr max

**Resync trigger:**
- Firehose event for a repo where commit doesn't apply to known rev
- Mark as Desync, queue for full resync

## Modes

### Manual (default)
Track only repos explicitly added via API/CLI.

### Signal Collection
Auto-track repos that post to a signal collection. When firehose sees a record in the signal collection from an unknown DID, add that repo and backfill.

Example: `signal_collection = "fm.teal.alpha.feed"` tracks all fm.teal users.

### Full Network
Enumerate all repos via `com.atproto.sync.listRepos` and track everything. Resource-intensive, takes days/weeks.

## Collection Filtering

Applies to all modes. Only store records matching the filter.

Wildcards supported at NSID segment boundaries:
- `fm.teal.*` matches `fm.teal.alpha.feed`, `fm.teal.alpha.like`
- `app.bsky.feed.post` matches exactly

Example config:
- Signal: `fm.teal.alpha.feed` (triggers discovery)
- Filter: `fm.teal.*` (what gets stored)

Result: discover users via their feed posts, store only their fm.teal data.

## Firehose Integration

```
on firehose event (did, rev, ops):

  # Collection filter
  if collection_filter set:
    ops = filter(ops, matches_collection_filter)
    if ops empty: return

  # Mode-specific handling
  match mode:
    Manual:
      if did not in tracked_repos: return

    Signal:
      if did not in tracked_repos:
        if any op matches signal_collection:
          add_repo(did, status=Pending)
        else:
          return

    FullNetwork:
      if did not in tracked_repos:
        add_repo(did, status=Pending)

  # Desync detection
  repo = get_repo_state(did)
  if repo.status == Syncing:
    buffer_event(did, event)  # hold until backfill done
    return

  if repo.rev and event.prev != repo.rev:
    mark_desync(did)
    return

  # Apply event
  apply_ops(ops)
  update_repo_rev(did, rev)
```

## HTTP API

```
POST /repos/add
  {"dids": ["did:plc:abc"], "collections": ["app.bsky.feed.post"]}
  -> {"queued": 1}

POST /repos/remove
  {"dids": ["did:plc:abc"]}
  -> {"removed": 1}

GET /repos
  -> {"repos": [{"did": "...", "status": "synced", ...}]}

GET /repos/:did
  -> {"did": "...", "status": "syncing", "record_count": 456, ...}

POST /repos/resync
  {"dids": ["did:plc:abc"]}
  -> {"queued": 1}

GET /repos/errors
  -> {"repos": [...]}

POST /config
  {"mode": "signal", "signal_collection": "fm.teal.alpha.feed", "collections": ["fm.teal.*"]}
  -> {"ok": true}

GET /config
  -> {"mode": "signal", "signal_collection": "...", "collections": [...]}
```

## CLI Commands

```
.config                       # show current config
.config mode signal
.config signal fm.teal.alpha.feed
.config collections fm.teal.*,app.bsky.feed.post

.repos                        # list tracked repos
.repos add did:plc:abc
.repos remove did:plc:abc
.repos status did:plc:abc
.repos errors
.repos resync did:plc:abc
.repos resync --errors

.connect                      # start firehose
.disconnect
```

## Configuration

```
# Mode: manual (default), signal, full-network
mode = "manual"

# Signal collection (for signal mode)
signal_collection = "fm.teal.alpha.feed"

# Collection filter (what gets stored)
collections = ["fm.teal.*", "app.bsky.feed.post"]

# Sync settings
sync_parallelism = 3
retry_backoff_min = "1m"
retry_backoff_max = "1h"
repo_fetch_timeout = "5m"

# Relay
relay = "bsky.network"
```

## Implementation Order

1. Repo state storage (RepoState struct, fjall keyspace)
2. Sync queue and worker (background thread, parallelism)
3. Retry timer (backoff logic)
4. HTTP endpoints for /repos/*
5. CLI commands for .repos
6. Collection filtering
7. Firehose integration (mode routing, desync detection)
8. Signal collection mode
9. Full network mode (listRepos enumeration)
