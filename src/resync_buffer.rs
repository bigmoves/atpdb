use fjall::Keyspace;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ResyncBufferError {
    #[error("storage error: {0}")]
    Storage(#[from] fjall::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// A buffered operation from the firehose
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BufferedOperation {
    Create {
        uri: String,
        cid: String,
        value: serde_json::Value,
    },
    Update {
        uri: String,
        cid: String,
        value: serde_json::Value,
    },
    Delete {
        uri: String,
    },
}

/// A buffered commit from the firehose
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferedCommit {
    pub seq: i64,
    pub did: String,
    pub rev: String,
    pub operations: Vec<BufferedOperation>,
}

/// Buffer for firehose events that arrive while a repo is being synced
pub struct ResyncBuffer {
    keyspace: Keyspace,
}

impl ResyncBuffer {
    /// Key format: "b:{did}:{seq:020}" - zero-padded seq for ordering
    fn buffer_key(did: &str, seq: i64) -> Vec<u8> {
        format!("b:{}:{:020}", did, seq).into_bytes()
    }

    /// Prefix for all commits for a DID
    fn did_prefix(did: &str) -> Vec<u8> {
        format!("b:{}:", did).into_bytes()
    }

    pub fn from_keyspace(keyspace: Keyspace) -> Self {
        ResyncBuffer { keyspace }
    }

    /// Add a commit to the buffer
    pub fn add(&self, commit: &BufferedCommit) -> Result<(), ResyncBufferError> {
        let key = Self::buffer_key(&commit.did, commit.seq);
        let value = serde_json::to_vec(commit)?;
        self.keyspace.insert(key, value)?;
        Ok(())
    }

    /// Get and remove all buffered commits for a DID, in seq order
    pub fn drain(&self, did: &str) -> Result<Vec<BufferedCommit>, ResyncBufferError> {
        let prefix = Self::did_prefix(did);
        let mut commits = Vec::new();
        let mut keys_to_delete = Vec::new();

        // First pass: collect keys
        for item in self.keyspace.prefix(&prefix) {
            let key = item.key()?;
            keys_to_delete.push(key.to_vec());
        }

        // Second pass: read values and delete
        for key in &keys_to_delete {
            if let Some(value) = self.keyspace.get(key)? {
                if let Ok(commit) = serde_json::from_slice::<BufferedCommit>(&value) {
                    commits.push(commit);
                }
            }
            self.keyspace.remove(key)?;
        }

        // Already sorted by seq due to key format
        Ok(commits)
    }

    /// Check if there are any buffered commits for a DID
    #[allow(dead_code)]
    pub fn has_buffered(&self, did: &str) -> bool {
        let prefix = Self::did_prefix(did);
        self.keyspace.prefix(&prefix).next().is_some()
    }

    /// Count buffered commits for a DID (useful for debugging/monitoring)
    #[allow(dead_code)]
    pub fn count(&self, did: &str) -> usize {
        let prefix = Self::did_prefix(did);
        self.keyspace.prefix(&prefix).count()
    }
}
