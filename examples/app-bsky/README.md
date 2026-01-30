# Bluesky Timeline Example

A demo timeline UI for Bluesky posts with like/repost counts, powered by atpdb.

## Running atpdb

Start the server with Bluesky configuration:

```bash
ATPDB_MODE=full-network \
ATPDB_RELAY=wss://relay1.us-east.bsky.network \
ATPDB_COLLECTIONS=app.bsky.feed.post,app.bsky.actor.profile,app.bsky.feed.like,app.bsky.feed.repost \
ATPDB_INDEXES=app.bsky.feed.post:createdAt:datetime,app.bsky.feed.like:subject.uri:at-uri,app.bsky.feed.repost:subject.uri:at-uri \
ATPDB_SEARCH_FIELDS=app.bsky.feed.post:text \
cargo run -- serve
```

This configures:
- **Full-network mode** - Crawls all repos from the relay
- **Collections** - Indexes posts, profiles (for hydration), likes, and reposts
- **Indexes** - Sort posts by `createdAt`, index likes/reposts by `subject.uri` for reverse lookups
- **Search** - Fuzzy search on post text

## Running the UI

Open `index.html` in your browser. It connects to `http://localhost:3000` by default.

Features:
- Chronological feed of Bluesky posts
- Like and repost counts (using reverse lookups)
- Hydrated user profiles with avatars
- Search by post text
- Pagination with "Load More"

## Query Examples

```bash
# Latest posts with like/repost counts
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{
    "q": "at://*/app.bsky.feed.post/*",
    "sort": "createdAt:datetime:desc",
    "hydrate.author": "at://$.did/app.bsky.actor.profile/self",
    "count.likes": "at://*/app.bsky.feed.like/*?subject.uri=$.uri",
    "count.reposts": "at://*/app.bsky.feed.repost/*?subject.uri=$.uri"
  }'

# Search posts
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{"q": "at://*/app.bsky.feed.post/*", "search.text": "hello"}'

# Get likes on a specific post (reverse hydration)
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{
    "q": "at://did:plc:example/app.bsky.feed.post/abc123",
    "hydrate.likers": "at://*/app.bsky.feed.like/*?subject.uri=$.uri&limit=10"
  }'
```

## How Reverse Lookups Work

The `count.*` and reverse `hydrate.*` patterns use the new `at-uri` index type:

1. **Index Configuration**: `app.bsky.feed.like:subject.uri:at-uri` creates an index on the `subject.uri` field
2. **Count Pattern**: `at://*/app.bsky.feed.like/*?subject.uri=$.uri` counts likes where `subject.uri` matches the post's URI
3. **Hydrate Pattern**: Same pattern with `&limit=N` fetches the actual like records

This enables efficient "who liked this post?" queries without scanning all likes.
