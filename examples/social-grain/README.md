# Social Grain Gallery Example

A demo gallery UI for Social Grain photo galleries with nested hydration, powered by atpdb.

## Data Model

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

## Running atpdb

Start the server with Social Grain configuration:

```bash
RUST_LOG=debug \
ATPDB_MODE=signal \
ATPDB_RELAY=wss://relay1.us-east.bsky.network \
ATPDB_SIGNAL_COLLECTION=social.grain.gallery \
ATPDB_COLLECTIONS=social.grain.gallery,social.grain.gallery.item,social.grain.photo,social.grain.actor.profile,social.grain.favorite,social.grain.comment \
ATPDB_INDEXES=social.grain.gallery:createdAt:datetime,social.grain.gallery.item:gallery:at-uri,social.grain.favorite:subject:at-uri,social.grain.comment:subject:at-uri \
cargo run -- serve
```

This configures:
- **Signal mode** - Indexes social.grain.* collections
- **Collections** - Indexes galleries, items, photos, profiles, favorites, comments
- **Indexes** - Sort galleries by `createdAt`, index items by `gallery` for reverse lookups

## Running the UI

Open `index.html` in your browser. It connects to `http://localhost:3000` by default.

Features:
- Gallery grid with cover photos
- **Nested hydration** - gallery items with their photos hydrated in a single query
- Favorite and comment counts
- Hydrated user profiles with avatars

## Query Examples

### Gallery Feed with Nested Hydration

```bash
curl -X POST localhost:3000/query -H "Content-Type: application/json" \
  -d '{
    "collection": "social.grain.gallery",
    "sort": "createdAt:datetime:desc",
    "limit": 20,
    "hydrate": {
      "author": "at://$.did/social.grain.actor.profile/self",
      "items": "at://*/social.grain.gallery.item/*?gallery=$.uri&limit=10",
      "items.photo": "at://$.item"
    },
    "blobs": {
      "author.avatar": "avatar",
      "items.photo.photo": "feed_thumbnail"
    },
    "count": {
      "favoriteCount": "at://*/social.grain.favorite/*?subject=$.uri",
      "commentCount": "at://*/social.grain.comment/*?subject=$.uri"
    }
  }'
```

### How Nested Hydration Works

1. **Level 0 hydrations** execute first:
   - `author` - fetches the gallery creator's profile
   - `items` - fetches gallery items via reverse lookup on `gallery` field

2. **Level 1 hydrations** execute second:
   - `items.photo` - for each item in `items`, fetches the photo record via `at://$.item`

3. **Blob transforms** apply at both levels:
   - `author.avatar` - transforms the profile's avatar blob
   - `items.photo.photo` - transforms each nested photo's blob

### Result Structure

```json
{
  "uri": "at://did:plc:abc/social.grain.gallery/123",
  "value": {
    "title": "My Gallery",
    "createdAt": "2024-01-15T12:00:00Z"
  },
  "author": {
    "uri": "at://did:plc:abc/social.grain.actor.profile/self",
    "value": {
      "displayName": "Alice",
      "avatar": { "url": "https://cdn.example.com/..." }
    }
  },
  "items": [
    {
      "uri": "at://did:plc:abc/social.grain.gallery.item/item1",
      "value": { "item": "at://did:plc:abc/social.grain.photo/photo1" },
      "photo": {
        "uri": "at://did:plc:abc/social.grain.photo/photo1",
        "value": {
          "photo": { "url": "https://cdn.example.com/..." }
        }
      }
    }
  ],
  "favoriteCount": 42,
  "commentCount": 5
}
```

## Validation Rules

Nested hydration enforces:
- **Max depth of 2 levels** - `items.photo` is valid, `items.photo.something` would fail
- **Parent must exist** - `items.photo` requires `items` to be defined first
