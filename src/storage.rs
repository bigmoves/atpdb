use crate::types::{AtUri, Did, Nsid};
use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("fjall error: {0}")]
    Fjall(#[from] fjall::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub uri: String,
    pub cid: String,
    pub value: serde_json::Value,
    pub indexed_at: u64,
}

pub struct Store {
    #[allow(dead_code)]
    db: Database,
    records: Keyspace,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let db = Database::builder(path).open()?;
        let records = db.keyspace("records", KeyspaceCreateOptions::default)?;
        Ok(Store { db, records })
    }

    pub fn put(&self, uri: &AtUri, record: &Record) -> Result<(), StorageError> {
        let key = uri.to_storage_key();
        let value = serde_json::to_vec(record)?;
        self.records.insert(key, value)?;
        Ok(())
    }

    pub fn get(&self, uri: &AtUri) -> Result<Option<Record>, StorageError> {
        let key = uri.to_storage_key();
        match self.records.get(&key)? {
            Some(bytes) => {
                let record: Record = serde_json::from_slice(&bytes)?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    pub fn delete(&self, uri: &AtUri) -> Result<(), StorageError> {
        let key = uri.to_storage_key();
        self.records.remove(key)?;
        Ok(())
    }

    pub fn scan_collection(
        &self,
        did: &Did,
        collection: &Nsid,
    ) -> Result<Vec<Record>, StorageError> {
        let prefix = format!("at://{}/{}/", did, collection);
        let mut results = Vec::new();

        for item in self.records.prefix(prefix.as_bytes()) {
            let value = item.value()?;
            let record: Record = serde_json::from_slice(&value)?;
            results.push(record);
        }

        Ok(results)
    }

    pub fn count(&self) -> Result<usize, StorageError> {
        Ok(self.records.len()?)
    }

    pub fn unique_dids(&self) -> Result<Vec<String>, StorageError> {
        use std::collections::HashSet;
        let mut dids = HashSet::new();

        for item in self.records.prefix(b"at://") {
            let key = item.key()?;
            if let Ok(key_str) = std::str::from_utf8(&key) {
                // at://did:plc:xxx/collection/rkey
                if let Some(rest) = key_str.strip_prefix("at://") {
                    if let Some(slash_pos) = rest.find('/') {
                        dids.insert(rest[..slash_pos].to_string());
                    }
                }
            }
        }

        let mut result: Vec<_> = dids.into_iter().collect();
        result.sort();
        Ok(result)
    }

    pub fn unique_collections(&self) -> Result<Vec<String>, StorageError> {
        use std::collections::HashSet;
        let mut collections = HashSet::new();

        for item in self.records.prefix(b"at://") {
            let key = item.key()?;
            if let Ok(key_str) = std::str::from_utf8(&key) {
                // at://did:plc:xxx/collection/rkey
                if let Some(rest) = key_str.strip_prefix("at://") {
                    let parts: Vec<&str> = rest.splitn(3, '/').collect();
                    if parts.len() >= 2 {
                        collections.insert(parts[1].to_string());
                    }
                }
            }
        }

        let mut result: Vec<_> = collections.into_iter().collect();
        result.sort();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_store_put_get() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let uri: AtUri = "at://did:plc:xyz/app.bsky.feed.post/abc".parse().unwrap();
        let record = Record {
            uri: uri.to_string(),
            cid: "bafytest".to_string(),
            value: serde_json::json!({"text": "hello"}),
            indexed_at: 1234567890,
        };

        store.put(&uri, &record).unwrap();
        let retrieved = store.get(&uri).unwrap().unwrap();

        assert_eq!(retrieved.cid, "bafytest");
        assert_eq!(retrieved.value["text"], "hello");
    }

    #[test]
    fn test_store_delete() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let uri: AtUri = "at://did:plc:xyz/app.bsky.feed.post/abc".parse().unwrap();
        let record = Record {
            uri: uri.to_string(),
            cid: "bafytest".to_string(),
            value: serde_json::json!({}),
            indexed_at: 0,
        };

        store.put(&uri, &record).unwrap();
        store.delete(&uri).unwrap();
        assert!(store.get(&uri).unwrap().is_none());
    }

    #[test]
    fn test_store_scan_collection() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let did: Did = "did:plc:xyz".parse().unwrap();
        let collection: Nsid = "app.bsky.feed.post".parse().unwrap();

        // Insert 3 posts
        for rkey in ["a", "b", "c"] {
            let uri: AtUri = format!("at://did:plc:xyz/app.bsky.feed.post/{}", rkey)
                .parse()
                .unwrap();
            let record = Record {
                uri: uri.to_string(),
                cid: format!("cid-{}", rkey),
                value: serde_json::json!({"rkey": rkey}),
                indexed_at: 0,
            };
            store.put(&uri, &record).unwrap();
        }

        // Insert 1 like (different collection)
        let like_uri: AtUri = "at://did:plc:xyz/app.bsky.feed.like/d".parse().unwrap();
        let like_record = Record {
            uri: like_uri.to_string(),
            cid: "cid-like".to_string(),
            value: serde_json::json!({}),
            indexed_at: 0,
        };
        store.put(&like_uri, &like_record).unwrap();

        let posts = store.scan_collection(&did, &collection).unwrap();
        assert_eq!(posts.len(), 3);
    }
}
