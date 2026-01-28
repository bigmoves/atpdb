# atpdb - AT Protocol Native Database

A single-binary database purpose-built for AT Protocol with a query language based on AT-URI patterns.

---

## Problem

General-purpose databases treat AT Protocol data as generic JSON/strings:

- AT-URIs parsed on every query
- No automatic indexing of references between records
- Graph queries (follows, likes) require complex JOINs
- Counts require full table scans
- Query language doesn't understand AT Protocol concepts

## Solution

A database where AT-URIs, DIDs, NSIDs, and records are native types with a query language that extends AT-URI syntax.

---

## Query Language

The query language extends AT-URI patterns with wildcards, filters, and hydration.

### Core Syntax

```
pattern?filters{hydration}
```

### Patterns

```
# Single record
at://did:plc:xyz/app.bsky.feed.post/3k2a1b

# All records in collection for a user
at://did:plc:xyz/app.bsky.feed.post/*

# All records in collection across all users
at://*/app.bsky.feed.post/*

# Field projection
at://did:plc:me/app.bsky.graph.follow/*/subject
```

### Filters

Query-string style after the pattern:

```
# Field equals value
at://*/app.bsky.feed.post/*?reply=null

# Field equals another AT-URI (ref lookup)
at://*/app.bsky.feed.like/*?subject.uri=at://did:plc:xyz/app.bsky.feed.post/abc

# Subquery filter (posts from my follows)
at://*/app.bsky.feed.post/*?did=at://did:plc:me/app.bsky.graph.follow/*/subject

# Options
?limit=50
?since=24h
?since=cursor123
?sort=asc
?sort=desc
```

### Hydration

Curly braces define related data to fetch. `$` references the current record:

```
# Hydrate author profile
at://did:plc:xyz/app.bsky.feed.post/* {
  author: at://$.did/app.bsky.actor.profile/self
}

# Hydrate counts
at://*/app.bsky.feed.post/*?limit=50 {
  likeCount: count(at://*/app.bsky.feed.like/*?subject.uri=$.uri),
  replyCount: count(at://*/app.bsky.feed.post/*?reply.parent.uri=$.uri)
}

# Nested hydration
at://*/app.bsky.feed.post/*?limit=50 {
  author: at://$.did/app.bsky.actor.profile/self,
  quotedPost: at://$.value.embed.record.uri {
    author: at://$.did/app.bsky.actor.profile/self
  }
}
```

### Reference Syntax

- `$` - current record
- `$.uri` - current record's AT-URI
- `$.did` - current record's DID
- `$.value.X` - field X in record value
- `$.value.reply.parent.uri` - nested field access

---

## Data Model

### Lexicon Registry

The database is lexicon-aware. Lexicons (AT Protocol's schema definitions) inform indexing, validation, and query optimization.

**Loading Lexicons:**
- Load from a directory of `.json` lexicon files
- Fetch from network (resolve NSID → lexicon)
- Built-in lexicons for `app.bsky.*` and `com.atproto.*`
- Hot-reload when lexicons change

**What Lexicons Enable:**
- **Smarter indexing:** Know which fields are refs (`ref`, `blob`, `cid-link`) vs primitives
- **Validation:** Optionally validate records against schema on write
- **Query hints:** Know field types for query optimization
- **Count patterns:** Identify countable relationships from `ref` fields

**Schema Evolution:**
- Lexicons are generally additive (new optional fields)
- No data migration needed - old records remain valid
- Database handles records from before/after schema changes
- Unknown lexicons accepted gracefully (treated as opaque JSON, refs still extracted)

**CLI:**
```
atp> .lexicons
Loaded: 47 lexicons
  app.bsky.feed.post (v1)
  app.bsky.feed.like (v1)
  app.bsky.graph.follow (v1)
  ...

atp> .lexicon app.bsky.feed.post
{
  "id": "app.bsky.feed.post",
  "defs": {
    "main": {
      "type": "record",
      "record": {
        "type": "object",
        "required": ["text", "createdAt"],
        "properties": {
          "text": { "type": "string", "maxLength": 3000 },
          "reply": { "type": "ref", "ref": "#replyRef" },
          ...
        }
      }
    }
  }
}

atp> .lexicon reload
Reloaded 47 lexicons

atp> .lexicon fetch app.bsky.feed.threadgate
Fetched and registered app.bsky.feed.threadgate
```

**Configuration:**
```
# Directory of lexicon JSON files
lexicon_path = "./lexicons"

# Fetch unknown lexicons from network
lexicon_fetch = true

# Validate records against lexicons on write
lexicon_validate = false  # off by default for performance

# Strict mode: reject records for unknown lexicons
lexicon_strict = false
```

### Storage Tables

**Records** - Primary record storage
- Key: AT-URI (did + collection + rkey)
- Value: Full record (CID, value bytes, indexed timestamp)
- Records for same user stored contiguously

**Timeline** - Collection-wide chronological index
- Key: (collection, timestamp, uri)
- Enables: "latest posts" queries

**Refs** - Reverse reference index
- Key: (collection, field_path, target_uri, source_uri)
- Enables: "who liked this post" queries
- Auto-populated by scanning record values for AT-URIs and DIDs

**Edges** - Graph relationships
- Key: (source_did, edge_type, target_did)
- Edge types: follow, block, mute
- Enables: fast graph traversal without scanning records

**Counts** - Pre-computed counts
- Key: (collection, field_path, target_uri)
- Value: count
- Enables: O(1) like/reply/repost counts
- Incremented/decremented on record create/delete
- Which fields to count is derived from lexicons (any `ref` field pointing to another record) plus explicit configuration for common patterns

**Count Configuration:**
```
# Explicit count patterns (in addition to lexicon-derived)
[counts]
"app.bsky.feed.like.subject" = true
"app.bsky.feed.repost.subject" = true
"app.bsky.feed.post.reply.parent" = true
"app.bsky.feed.post.reply.root" = true
"app.bsky.graph.follow.subject" = true

# Or: auto-count all ref fields (may be expensive)
auto_count_refs = false
```

### Native Types

**DID** - Decentralized identifier
- Fixed-size representation
- Methods: plc, web
- Byte-comparable for efficient indexing

**NSID** - Namespaced identifier (collection names)
- Example: `app.bsky.feed.post`

**Rkey** - Record key
- Usually a TID (timestamp-based ID) or "self"

**AT-URI** - Full record address
- Composite of DID + NSID + Rkey
- Example: `at://did:plc:xyz/app.bsky.feed.post/3k2a1b`

---

## Components

### Type System
Native types for DID, NSID, Rkey, AT-URI that parse from strings, format back to strings, and compare as bytes.

### Storage Engine
Embedded key-value storage with:
- ACID transactions
- Prefix/range scans
- Batch writes for throughput
- Memory-mapped reads for speed
- Proven at 100GB+ scale
- Active maintenance

**Candidates to evaluate:**
- **RocksDB** - LSM-tree, excellent write throughput, used by CockroachDB/TiKV. Battle-tested at massive scale. Good for write-heavy firehose ingest.
- **LMDB/libmdbx** - B+tree, memory-mapped, single-writer/multi-reader. libmdbx is actively maintained fork used by Erigon. Great read performance, simpler than RocksDB.
- **SQLite** - Use as key-value store. Extremely battle-tested but adds SQL overhead.
- **fjall** - Pure Rust LSM-tree, actively developed, newer.

Workload is write-heavy during firehose sync, read-heavy during queries. RocksDB or libmdbx are likely the safest choices. Start with one and abstract the storage layer so it can be swapped.

Tables: Records, Timeline, Refs, Edges, Counts, Meta.

### Indexer
On every record write: consult lexicon for field types, extract refs from known ref fields (more precise than walking JSON), detect graph edges, update Counts for ref fields. Falls back to JSON walking for unknown lexicons. Reverse operations on delete.

### Query Parser
Parse the AT-URI pattern query language into an AST representing patterns, filters, and hydration.

### Query Planner
Convert AST to execution plan: direct lookup, timeline scan, refs index lookup, subquery execution, hydration plan.

### Query Executor
Execute plans: fetch records, apply filters, run hydration (parallel where possible), build response.

### Firehose Client
Connect to AT Protocol relay via WebSocket, decode CBOR/CAR, process commit/identity/handle/tombstone events.

### Backfill / CAR Import
Import repository CAR files for bulk historical data loading (similar to [indigo/tap](https://github.com/bluesky-social/indigo/tree/main/cmd/tap)):
- Read CAR files from disk or URLs
- Parse MST (Merkle Search Tree) structure to extract all records
- Support single repo CARs and multi-repo archives
- Parallel import for throughput
- Progress reporting for large imports
- Deduplicate against existing records

This enables:
- Bootstrapping a new database from exported repos
- Importing specific users' complete history
- Disaster recovery from CAR backups
- Testing with known datasets

### Ingest Pipeline
Buffer events, batch write records, run indexer, update cursor, handle deletes.

### CLI / REPL
Interactive query execution with timing, firehose connection, stats display.

### HTTP Server
REST API for queries, returns JSON results.

---

## Example Queries

```
# Get a single post
at://did:plc:z72i7hdynmk6r22z27h6tvur/app.bsky.feed.post/3l2s5xxv2ze2c

# Get a user's recent posts
at://did:plc:z72i7hdynmk6r22z27h6tvur/app.bsky.feed.post/*?limit=20

# Get posts from people I follow (home feed)
at://*/app.bsky.feed.post/*?did=at://did:plc:me/app.bsky.graph.follow/*/subject&reply=null&limit=50 {
  author: at://$.did/app.bsky.actor.profile/self,
  likeCount: count(at://*/app.bsky.feed.like/*?subject.uri=$.uri)
}

# Who liked this post?
at://*/app.bsky.feed.like/*?subject.uri=at://did:plc:xyz/app.bsky.feed.post/abc&limit=100 {
  profile: at://$.did/app.bsky.actor.profile/self
}

# Get replies to a post
at://*/app.bsky.feed.post/*?reply.parent.uri=at://did:plc:xyz/app.bsky.feed.post/abc {
  author: at://$.did/app.bsky.actor.profile/self
}

# Get a thread (all posts with same root)
at://*/app.bsky.feed.post/*?reply.root.uri=at://did:plc:xyz/app.bsky.feed.post/abc&sort=asc {
  author: at://$.did/app.bsky.actor.profile/self
}

# Get followers
at://*/app.bsky.graph.follow/*?subject=did:plc:xyz&limit=100 {
  profile: at://$.did/app.bsky.actor.profile/self
}

# Get who I follow
at://did:plc:me/app.bsky.graph.follow/* {
  profile: at://$.value.subject/app.bsky.actor.profile/self
}
```

---

## Performance Goals

| Operation | Target |
|-----------|--------|
| Single record lookup | <0.1ms |
| Get count (likes on post) | <0.1ms |
| User's posts (100) | <5ms |
| Posts from follows (50) | <20ms |
| Who liked this (100) | <10ms |
| Firehose ingest | >500 events/sec sustained |

---

## CLI Interface

```
$ atpdb

   ___  ______ ___  ___  ___
  / _ |/_  __// _ \/ _ \/ _ )
 / __ | / /  / ___/ // / _  |
/_/ |_|/_/  /_/  /____/____/

atp> .connect bsky.network
Connected. Cursor: 1234567

atp> at://did:plc:xyz/app.bsky.feed.post/*?limit=5
[results...]
(2.3ms)

atp> .stats
Records: 1,234,567
Storage: 2.3 GB
Uptime: 4h 23m

atp> .help
Commands:
  .connect <relay>  Connect to firehose
  .disconnect       Stop firehose
  .import <path>    Import CAR file(s)
  .import <url>     Import CAR from URL
  .stats            Show statistics
  .quit             Exit
```

### Backfill Examples

```bash
# Import a single repo CAR
$ atpdb import ./did:plc:xyz.car

# Import from URL
$ atpdb import https://bsky.social/xrpc/com.atproto.sync.getRepo?did=did:plc:xyz

# Import directory of CARs
$ atpdb import ./backups/

# Import with progress
$ atpdb import ./large-archive.car --progress
Importing: 1,234,567 records [=====>    ] 45% (12,345 rec/s)
```

---

## HTTP API

```
POST /query
Content-Type: application/json

{
  "q": "at://*/app.bsky.feed.post/*?limit=10"
}

Response:
{
  "records": [...],
  "cursor": "..."
}
```

```
POST /import
Content-Type: application/json

{
  "did": "did:plc:xyz",
  "source": "https://bsky.social/xrpc/com.atproto.sync.getRepo?did=did:plc:xyz"
}

Response:
{
  "imported": 1234,
  "duration_ms": 567
}
```

```
GET /health
→ 200 OK
```

```
GET /lexicons
→ { "lexicons": ["app.bsky.feed.post", ...], "count": 47 }

GET /lexicons/app.bsky.feed.post
→ { full lexicon JSON }

POST /lexicons/reload
→ { "reloaded": 47 }
```
