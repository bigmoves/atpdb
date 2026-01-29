use fjall::{Database, Keyspace};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(test)]
use fjall::KeyspaceCreateOptions;
#[cfg(test)]
use std::path::Path;

#[derive(Error, Debug)]
pub enum RepoError {
    #[error("storage error: {0}")]
    Storage(#[from] fjall::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoStatus {
    Pending,
    Syncing,
    Synced,
    Error,
    Desync,
}

impl std::fmt::Display for RepoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoStatus::Pending => write!(f, "pending"),
            RepoStatus::Syncing => write!(f, "syncing"),
            RepoStatus::Synced => write!(f, "synced"),
            RepoStatus::Error => write!(f, "error"),
            RepoStatus::Desync => write!(f, "desync"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    pub did: String,
    pub status: RepoStatus,
    pub rev: Option<String>,
    pub record_count: u64,
    pub last_sync: Option<u64>,
    pub error: Option<String>,
    pub retry_count: u32,
    pub next_retry: Option<u64>,
}

impl RepoState {
    pub fn new(did: String) -> Self {
        RepoState {
            did,
            status: RepoStatus::Pending,
            rev: None,
            record_count: 0,
            last_sync: None,
            error: None,
            retry_count: 0,
            next_retry: None,
        }
    }
}

pub struct RepoStore {
    #[allow(dead_code)]
    db: Option<Database>,
    repos: Keyspace,
}

impl RepoStore {
    /// Key prefix for main repo data
    const REPO_PREFIX: &'static [u8] = b"r:";
    /// Key prefix for status index
    const STATUS_PREFIX: &'static str = "s:";

    fn repo_key(did: &str) -> Vec<u8> {
        let mut key = Self::REPO_PREFIX.to_vec();
        key.extend_from_slice(did.as_bytes());
        key
    }

    fn status_key(status: RepoStatus, did: &str) -> Vec<u8> {
        format!("{}{}:{}", Self::STATUS_PREFIX, status, did).into_bytes()
    }

    fn status_prefix(status: RepoStatus) -> Vec<u8> {
        format!("{}{}:", Self::STATUS_PREFIX, status).into_bytes()
    }

    #[cfg(test)]
    pub fn open(path: &Path) -> Result<Self, RepoError> {
        let db = Database::builder(path).open()?;
        let repos = db.keyspace("repos", KeyspaceCreateOptions::default)?;
        Ok(RepoStore {
            db: Some(db),
            repos,
        })
    }

    pub fn from_keyspace(repos: Keyspace) -> Self {
        RepoStore { db: None, repos }
    }

    pub fn put(&self, state: &RepoState) -> Result<(), RepoError> {
        // Get old state to update status index
        if let Some(old_state) = self.get(&state.did)? {
            if old_state.status != state.status {
                // Remove old status index entry
                self.repos.remove(&Self::status_key(old_state.status, &state.did))?;
            }
        }

        // Store the repo data
        let value = serde_json::to_vec(state)?;
        self.repos.insert(&Self::repo_key(&state.did), &value)?;

        // Add status index entry (value is empty, just need the key for lookup)
        self.repos.insert(&Self::status_key(state.status, &state.did), b"")?;

        Ok(())
    }

    pub fn get(&self, did: &str) -> Result<Option<RepoState>, RepoError> {
        match self.repos.get(&Self::repo_key(did))? {
            Some(bytes) => {
                let state: RepoState = serde_json::from_slice(&bytes)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    pub fn delete(&self, did: &str) -> Result<(), RepoError> {
        // Remove status index entry if exists
        if let Some(state) = self.get(did)? {
            self.repos.remove(&Self::status_key(state.status, did))?;
        }
        self.repos.remove(&Self::repo_key(did))?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<RepoState>, RepoError> {
        let mut results = Vec::new();
        for item in self.repos.prefix(Self::REPO_PREFIX) {
            let value = item.value()?;
            let state: RepoState = serde_json::from_slice(&value)?;
            results.push(state);
        }
        Ok(results)
    }

    pub fn list_by_status(&self, status: RepoStatus) -> Result<Vec<RepoState>, RepoError> {
        let mut results = Vec::new();
        let prefix = Self::status_prefix(status);

        for item in self.repos.prefix(&prefix) {
            // Extract DID from key: "s:{status}:{did}"
            let key = item.key()?;
            let key_str = String::from_utf8_lossy(&key);
            if let Some(did) = key_str.strip_prefix(&format!("{}{}:", Self::STATUS_PREFIX, status)) {
                if let Some(state) = self.get(did)? {
                    results.push(state);
                }
            }
        }
        Ok(results)
    }

    pub fn contains(&self, did: &str) -> Result<bool, RepoError> {
        Ok(self.repos.contains_key(&Self::repo_key(did))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_repo_store_put_get() {
        let dir = tempdir().unwrap();
        let store = RepoStore::open(dir.path()).unwrap();

        let did = "did:plc:test123";
        let state = RepoState::new(did.to_string());

        store.put(&state).unwrap();
        let retrieved = store.get(did).unwrap().unwrap();

        assert_eq!(retrieved.did, did);
        assert_eq!(retrieved.status, RepoStatus::Pending);
    }

    #[test]
    fn test_repo_store_list() {
        let dir = tempdir().unwrap();
        let store = RepoStore::open(dir.path()).unwrap();

        store
            .put(&RepoState::new("did:plc:aaa".to_string()))
            .unwrap();
        store
            .put(&RepoState::new("did:plc:bbb".to_string()))
            .unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_repo_store_delete() {
        let dir = tempdir().unwrap();
        let store = RepoStore::open(dir.path()).unwrap();

        let did = "did:plc:test";
        store.put(&RepoState::new(did.to_string())).unwrap();
        store.delete(did).unwrap();

        assert!(store.get(did).unwrap().is_none());
    }
}
