# fm.teal Example

A demo feed UI for [teal.fm](https://teal.fm) music plays, powered by atpdb.

## Running atpdb

Start the server with fm.teal configuration:

```bash
ATPDB_MODE=signal \
ATPDB_RELAY=wss://relay1.us-east.bsky.network \
ATPDB_SIGNAL_COLLECTION=fm.teal.alpha.feed.play \
ATPDB_COLLECTIONS=fm.teal.alpha.feed.play,app.bsky.actor.profile \
ATPDB_INDEXES=fm.teal.alpha.feed.play:playedTime:datetime \
ATPDB_SEARCH_FIELDS=fm.teal.alpha.feed.play:trackName,fm.teal.alpha.feed.play:releaseName,fm.teal.alpha.feed.play:artists.*.artistName \
cargo run -- serve
```

This configures:
- **Signal mode** - Auto-discovers users who have fm.teal plays
- **Collections** - Indexes plays and profiles (for hydration)
- **Index** - Sorts by `playedTime` for chronological feed
- **Search** - Fuzzy search on track name, album, and artist

## Running the UI

Open `index.html` in your browser. It connects to `http://localhost:3000` by default.

Features:
- Chronological feed of fm.teal plays
- Search by track, artist, or album
- Hydrated user profiles with avatars
- Pagination with "Load More"

## Query Examples

```bash
# Latest plays, sorted by time
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{
    "q": "at://*/fm.teal.alpha.feed.play/*",
    "sort": "playedTime:datetime:desc",
    "limit": 50
  }'

# Search for tracks
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{
    "q": "at://*/fm.teal.alpha.feed.play/*",
    "search": {"trackName": "midnight"}
  }'

# With hydrated profiles
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{
    "q": "at://*/fm.teal.alpha.feed.play/*",
    "hydrate": {"author": "at://$.did/app.bsky.actor.profile/self"},
    "blobs": {"author.avatar": "avatar"}
  }'
```
