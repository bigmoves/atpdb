use crate::config::{IndexConfig, IndexDirection, IndexFieldType};
use crate::types::{AtUri, Did, Nsid};
use fjall::{Database, Keyspace};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(test)]
use fjall::KeyspaceCreateOptions;
#[cfg(test)]
use std::path::Path;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDirection {
    Ascending,
    Descending,
}

#[derive(Clone)]
pub struct Store {
    #[allow(dead_code)]
    db: Option<Database>,
    /// Records keyed by: {collection}\0{did}\0{rkey}
    /// This enables fast prefix scans for both collection-wide and per-user queries
    records: Keyspace,
}

impl Store {
    #[cfg(test)]
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let db = Database::builder(path).open()?;
        let records = db.keyspace("records", KeyspaceCreateOptions::default)?;
        Ok(Store {
            db: Some(db),
            records,
        })
    }

    pub fn from_keyspace(records: Keyspace) -> Self {
        Store { db: None, records }
    }

    /// Build storage key: {collection}\0{did}\0{rkey}
    fn storage_key(collection: &str, did: &str, rkey: &str) -> Vec<u8> {
        format!("{}\0{}\0{}", collection, did, rkey).into_bytes()
    }

    /// Build storage key from AtUri
    fn storage_key_from_uri(uri: &AtUri) -> Vec<u8> {
        Self::storage_key(uri.collection.as_str(), uri.did.as_str(), uri.rkey.as_str())
    }

    /// Encode integer for lexicographic sorting (handles negative numbers)
    fn encode_integer_for_sort(value: i64) -> String {
        // Prefix: 0 for negative, 1 for zero/positive
        // For negative: invert to sort correctly (more negative = smaller)
        if value >= 0 {
            format!("1{:020}", value)
        } else {
            // For negative numbers, invert so -1 > -2 in sort order
            format!("0{:020}", (i64::MAX as u64).wrapping_add(value as u64 + 1))
        }
    }

    /// Sanitize a value for use in index keys by escaping null bytes
    fn sanitize_key_value(s: &str) -> String {
        s.replace('\0', "\\0")
    }

    /// Build index key for ascending order
    fn index_key_asc(
        collection: &str,
        field: &str,
        sort_value: &str,
        did: &str,
        rkey: &str,
    ) -> Vec<u8> {
        format!(
            "idx:a:{}:{}\0{}\0{}\0{}",
            Self::sanitize_key_value(collection),
            Self::sanitize_key_value(field),
            Self::sanitize_key_value(sort_value),
            did,
            rkey
        )
        .into_bytes()
    }

    /// Build index key for descending order (inverted sort value)
    fn index_key_desc(
        collection: &str,
        field: &str,
        sort_value: &str,
        did: &str,
        rkey: &str,
    ) -> Vec<u8> {
        let inverted = Self::invert_sort_value(sort_value);
        format!(
            "idx:d:{}:{}\0{}\0{}\0{}",
            Self::sanitize_key_value(collection),
            Self::sanitize_key_value(field),
            Self::sanitize_key_value(&inverted),
            did,
            rkey
        )
        .into_bytes()
    }

    fn index_prefix_asc(collection: &str, field: &str) -> Vec<u8> {
        format!("idx:a:{}:{}\0", Self::sanitize_key_value(collection), Self::sanitize_key_value(field)).into_bytes()
    }

    fn index_prefix_desc(collection: &str, field: &str) -> Vec<u8> {
        format!("idx:d:{}:{}\0", Self::sanitize_key_value(collection), Self::sanitize_key_value(field)).into_bytes()
    }

    /// Build indexedAt keys (both directions)
    fn indexed_at_key_asc(collection: &str, timestamp: u64, did: &str, rkey: &str) -> Vec<u8> {
        format!(
            "idx:a:__ts__:{}\0{:020}\0{}\0{}",
            collection, timestamp, did, rkey
        )
        .into_bytes()
    }

    fn indexed_at_key_desc(collection: &str, timestamp: u64, did: &str, rkey: &str) -> Vec<u8> {
        let inverted = u64::MAX - timestamp;
        format!(
            "idx:d:__ts__:{}\0{:020}\0{}\0{}",
            collection, inverted, did, rkey
        )
        .into_bytes()
    }

    fn indexed_at_prefix_asc(collection: &str) -> Vec<u8> {
        format!("idx:a:__ts__:{}\0", collection).into_bytes()
    }

    fn indexed_at_prefix_desc(collection: &str) -> Vec<u8> {
        format!("idx:d:__ts__:{}\0", collection).into_bytes()
    }

    /// Invert sort value for descending index (complement each byte)
    fn invert_sort_value(s: &str) -> String {
        s.bytes().map(|b| (255 - b) as char).collect()
    }

    /// Write index entry for a record (only for configured direction)
    /// Stores the full record in the index value for fast reads (no secondary lookup)
    pub fn write_index_entry(
        &self,
        index: &IndexConfig,
        record: &Record,
        did: &str,
        rkey: &str,
    ) -> Result<(), StorageError> {
        let sort_value = self.extract_sort_value(&record.value, &index.field, index.field_type)?;
        if let Some(sv) = sort_value {
            let record_bytes = serde_json::to_vec(record)?;
            let key = match index.direction {
                IndexDirection::Asc => {
                    Self::index_key_asc(&index.collection, &index.field, &sv, did, rkey)
                }
                IndexDirection::Desc => {
                    Self::index_key_desc(&index.collection, &index.field, &sv, did, rkey)
                }
            };
            self.records.insert(&key, &record_bytes)?;
        }
        Ok(())
    }

    /// Write indexedAt index entries (both asc and desc)
    pub fn write_indexed_at_entry(
        &self,
        collection: &str,
        timestamp: u64,
        did: &str,
        rkey: &str,
    ) -> Result<(), StorageError> {
        let key_asc = Self::indexed_at_key_asc(collection, timestamp, did, rkey);
        self.records.insert(&key_asc, b"")?;
        let key_desc = Self::indexed_at_key_desc(collection, timestamp, did, rkey);
        self.records.insert(&key_desc, b"")?;
        Ok(())
    }

    /// Extract and encode sort value from record
    fn extract_sort_value(
        &self,
        value: &serde_json::Value,
        field_path: &str,
        field_type: IndexFieldType,
    ) -> Result<Option<String>, StorageError> {
        let mut current = value;
        for part in field_path.split('.') {
            current = match current.get(part) {
                Some(v) => v,
                None => return Ok(None),
            };
        }

        match field_type {
            IndexFieldType::Datetime => Ok(current.as_str().map(|s| s.to_string())),
            IndexFieldType::Integer => {
                if let Some(n) = current.as_i64() {
                    Ok(Some(Self::encode_integer_for_sort(n)))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Delete index entry for a record (only for configured direction)
    pub fn delete_index_entry(
        &self,
        index: &IndexConfig,
        record: &Record,
        did: &str,
        rkey: &str,
    ) -> Result<(), StorageError> {
        let sort_value = self.extract_sort_value(&record.value, &index.field, index.field_type)?;
        if let Some(sv) = sort_value {
            let key = match index.direction {
                IndexDirection::Asc => {
                    Self::index_key_asc(&index.collection, &index.field, &sv, did, rkey)
                }
                IndexDirection::Desc => {
                    Self::index_key_desc(&index.collection, &index.field, &sv, did, rkey)
                }
            };
            self.records.remove(&key)?;
        }
        Ok(())
    }

    /// Delete indexedAt index entries (both asc and desc)
    pub fn delete_indexed_at_entry(
        &self,
        collection: &str,
        timestamp: u64,
        did: &str,
        rkey: &str,
    ) -> Result<(), StorageError> {
        let key_asc = Self::indexed_at_key_asc(collection, timestamp, did, rkey);
        self.records.remove(&key_asc)?;
        let key_desc = Self::indexed_at_key_desc(collection, timestamp, did, rkey);
        self.records.remove(&key_desc)?;
        Ok(())
    }

    pub fn put(&self, uri: &AtUri, record: &Record) -> Result<(), StorageError> {
        let key = Self::storage_key_from_uri(uri);
        let value = serde_json::to_vec(record)?;
        self.records.insert(&key, &value)?;
        Ok(())
    }

    pub fn get(&self, uri: &AtUri) -> Result<Option<Record>, StorageError> {
        let key = Self::storage_key_from_uri(uri);
        match self.records.get(&key)? {
            Some(bytes) => {
                let record: Record = serde_json::from_slice(&bytes)?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Get record by string URI (parses the URI first)
    pub fn get_by_uri_string(&self, uri: &str) -> Result<Option<Record>, StorageError> {
        match uri.parse::<AtUri>() {
            Ok(parsed) => self.get(&parsed),
            Err(_) => Ok(None),
        }
    }

    pub fn delete(&self, uri: &AtUri) -> Result<(), StorageError> {
        let key = Self::storage_key_from_uri(uri);
        self.records.remove(key)?;
        Ok(())
    }

    /// Write record with indexes
    pub fn put_with_indexes(
        &self,
        uri: &AtUri,
        record: &Record,
        indexes: &[IndexConfig],
    ) -> Result<(), StorageError> {
        // Check if this is an update (record already exists)
        let is_new = self.get(uri)?.is_none();

        // Write primary record
        self.put(uri, record)?;

        // Write indexedAt index
        self.write_indexed_at_entry(
            uri.collection.as_str(),
            record.indexed_at,
            uri.did.as_str(),
            uri.rkey.as_str(),
        )?;

        // Write configured indexes
        for index in indexes {
            if index.collection == uri.collection.as_str() {
                self.write_index_entry(index, record, uri.did.as_str(), uri.rkey.as_str())?;
            }
        }

        // Increment counter only for new records
        if is_new {
            self.increment_count(uri.collection.as_str())?;
        }

        Ok(())
    }

    /// Delete record with indexes
    pub fn delete_with_indexes(
        &self,
        uri: &AtUri,
        indexes: &[IndexConfig],
    ) -> Result<(), StorageError> {
        // Get record first to know index values
        let existed = if let Some(record) = self.get(uri)? {
            // Delete indexes
            self.delete_indexed_at_entry(
                uri.collection.as_str(),
                record.indexed_at,
                uri.did.as_str(),
                uri.rkey.as_str(),
            )?;

            for index in indexes {
                if index.collection == uri.collection.as_str() {
                    self.delete_index_entry(index, &record, uri.did.as_str(), uri.rkey.as_str())?;
                }
            }
            true
        } else {
            false
        };

        // Delete primary record
        self.delete(uri)?;

        // Decrement counter only if record existed
        if existed {
            self.decrement_count(uri.collection.as_str())?;
        }

        Ok(())
    }

    /// Scan all records for a specific user in a specific collection
    /// Key prefix: {collection}\0{did}\0
    #[allow(dead_code)]
    pub fn scan_collection(
        &self,
        did: &Did,
        collection: &Nsid,
    ) -> Result<Vec<Record>, StorageError> {
        self.scan_collection_paginated(did, collection, None, usize::MAX)
    }

    /// Scan with pagination - cursor is the rkey of the last record seen
    pub fn scan_collection_paginated(
        &self,
        did: &Did,
        collection: &Nsid,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Record>, StorageError> {
        let prefix = format!("{}\0{}\0", collection, did);
        let start_key = cursor
            .map(|rkey| Self::storage_key(collection.as_str(), did.as_str(), rkey))
            .unwrap_or_else(|| prefix.as_bytes().to_vec());

        let mut results = Vec::new();
        let mut skip_first = cursor.is_some();

        let uri_prefix = format!("at://{}/{}/", did.as_str(), collection.as_str());

        for item in self.records.range(start_key..) {
            // Get value first (consumes item)
            let value = item.value()?;
            let record: Record = serde_json::from_slice(&value)?;

            // Check if still in correct DID and collection
            if !record.uri.starts_with(&uri_prefix) {
                break;
            }

            // Skip the cursor record itself
            if skip_first {
                skip_first = false;
                continue;
            }

            results.push(record);
            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }

    /// Scan all records in a collection across all users
    /// Key prefix: {collection}\0
    #[allow(dead_code)]
    pub fn scan_all_collection(&self, collection: &Nsid) -> Result<Vec<Record>, StorageError> {
        self.scan_all_collection_paginated(collection, None, usize::MAX)
    }

    /// Scan all collection with pagination - cursor is the last URI seen
    pub fn scan_all_collection_paginated(
        &self,
        collection: &Nsid,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Record>, StorageError> {
        let prefix = format!("{}\0", collection);
        let _collection_path = format!("/{}/", collection.as_str());

        // Parse cursor URI to get did and rkey for the start key
        let _start_key = if let Some(cursor_uri) = cursor {
            if let Ok(uri) = cursor_uri.parse::<AtUri>() {
                Self::storage_key(collection.as_str(), uri.did.as_str(), uri.rkey.as_str())
            } else {
                prefix.as_bytes().to_vec()
            }
        } else {
            prefix.as_bytes().to_vec()
        };

        let mut results = Vec::new();
        let mut past_cursor = cursor.is_none(); // If no cursor, we're already past it

        // Use prefix() to only iterate over keys starting with our collection
        for item in self.records.prefix(prefix.as_bytes()) {
            // Get value
            let value = match item.value() {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Skip empty values
            if value.is_empty() {
                continue;
            }

            let record: Record = match serde_json::from_slice(&value) {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Skip records until we're past the cursor
            if !past_cursor {
                if cursor.is_some_and(|c| record.uri == c) {
                    past_cursor = true; // Found cursor, skip it and start collecting after
                }
                continue;
            }

            results.push(record);
            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }

    /// Scan all records for a specific DID across all collections
    /// Note: Less efficient than collection-based queries due to key structure
    pub fn scan_did_paginated(
        &self,
        did: &Did,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Record>, StorageError> {
        let did_pattern = format!("at://{}/", did.as_str());
        let mut results = Vec::new();
        let mut past_cursor = cursor.is_none();

        // Iterate all records and filter by DID in the record's URI
        for item in self.records.prefix(b"") {
            let value = match item.value() {
                Ok(v) => v,
                Err(_) => continue,
            };

            if value.is_empty() {
                continue;
            }

            let record: Record = match serde_json::from_slice(&value) {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Check if this record belongs to our DID
            if !record.uri.starts_with(&did_pattern) {
                continue;
            }

            // Skip records until we're past the cursor
            if !past_cursor {
                if cursor.is_some_and(|c| record.uri == c) {
                    past_cursor = true;
                }
                continue;
            }

            results.push(record);
            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }

    pub fn count(&self) -> Result<usize, StorageError> {
        Ok(self.records.len()?)
    }

    /// Key for collection counters
    fn count_key(collection: &str) -> Vec<u8> {
        format!("__count__:{}", collection).into_bytes()
    }

    /// Increment collection counter
    fn increment_count(&self, collection: &str) -> Result<(), StorageError> {
        let key = Self::count_key(collection);
        let current = self.get_count_raw(&key)?;
        self.records.insert(&key, (current + 1).to_le_bytes())?;
        Ok(())
    }

    /// Decrement collection counter
    fn decrement_count(&self, collection: &str) -> Result<(), StorageError> {
        let key = Self::count_key(collection);
        let current = self.get_count_raw(&key)?;
        if current > 0 {
            self.records.insert(&key, (current - 1).to_le_bytes())?;
        }
        Ok(())
    }

    /// Get raw count value from key
    fn get_count_raw(&self, key: &[u8]) -> Result<u64, StorageError> {
        match self.records.get(key)? {
            Some(bytes) if bytes.len() == 8 => {
                let arr: [u8; 8] = bytes.as_ref().try_into().unwrap_or([0; 8]);
                Ok(u64::from_le_bytes(arr))
            }
            _ => Ok(0),
        }
    }

    /// Count records in a specific collection (O(1) using maintained counter)
    pub fn count_collection(&self, collection: &str) -> Result<usize, StorageError> {
        let key = Self::count_key(collection);
        Ok(self.get_count_raw(&key)? as usize)
    }

    /// Rebuild collection count by scanning the index (use during migration or if counts are wrong)
    pub fn rebuild_count(&self, collection: &str) -> Result<usize, StorageError> {
        let prefix = format!("idx:a:__ts__:{}\0", collection);
        let count = self.records.prefix(prefix.as_bytes()).count();
        let key = Self::count_key(collection);
        self.records.insert(&key, (count as u64).to_le_bytes())?;
        Ok(count)
    }

    pub fn unique_dids(&self) -> Result<Vec<String>, StorageError> {
        use std::collections::HashSet;
        let mut dids = HashSet::new();

        for item in self.records.prefix(b"") {
            let key = item.key()?;
            if let Ok(key_str) = std::str::from_utf8(&key) {
                // Key format: {collection}\0{did}\0{rkey}
                let parts: Vec<&str> = key_str.splitn(3, '\0').collect();
                if parts.len() >= 2 {
                    dids.insert(parts[1].to_string());
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

        for item in self.records.prefix(b"") {
            let key = item.key()?;
            if let Ok(key_str) = std::str::from_utf8(&key) {
                // Skip internal keys (indexes, counters, cursors, etc.)
                if key_str.starts_with("idx:")
                    || key_str.starts_with("__")
                    || key_str.starts_with("index:")
                    || key_str.starts_with("cursor:")
                {
                    continue;
                }
                // Key format: {collection}\0{did}\0{rkey}
                let parts: Vec<&str> = key_str.splitn(3, '\0').collect();
                if !parts.is_empty() {
                    collections.insert(parts[0].to_string());
                }
            }
        }

        let mut result: Vec<_> = collections.into_iter().collect();
        result.sort();
        Ok(result)
    }

    /// Scan an index with pagination - O(limit) for both directions
    pub fn scan_index(
        &self,
        collection: &str,
        field: &str,
        cursor_sort_value: Option<&str>,
        cursor_uri: Option<&str>,
        limit: usize,
        direction: ScanDirection,
    ) -> Result<Vec<Record>, StorageError> {
        // Use appropriate index based on direction
        let prefix = match direction {
            ScanDirection::Ascending => Self::index_prefix_asc(collection, field),
            ScanDirection::Descending => Self::index_prefix_desc(collection, field),
        };

        let start_key = match (cursor_sort_value, cursor_uri) {
            (Some(sv), Some(uri)) => {
                if let Some((did, rkey)) = parse_did_rkey_from_uri(uri) {
                    match direction {
                        ScanDirection::Ascending => {
                            Self::index_key_asc(collection, field, sv, &did, &rkey)
                        }
                        ScanDirection::Descending => {
                            Self::index_key_desc(collection, field, sv, &did, &rkey)
                        }
                    }
                } else {
                    prefix.clone()
                }
            }
            _ => prefix.clone(),
        };

        let mut results = Vec::new();
        let skip_first = cursor_sort_value.is_some();
        let mut past_cursor = !skip_first;

        for item in self.records.range(start_key..) {
            // Get value (consumes item)
            let value_bytes = item.value()?;

            // Skip cursor position
            if !past_cursor {
                past_cursor = true;
                continue;
            }

            // Try to deserialize record from index value
            match serde_json::from_slice::<Record>(&value_bytes) {
                Ok(record) => {
                    // Check if still within our collection (prefix boundary check)
                    let uri_parts: Vec<&str> = record.uri.split('/').collect();
                    if uri_parts.len() >= 4 && uri_parts[3] != collection {
                        break;
                    }
                    results.push(record);
                    if results.len() >= limit {
                        break;
                    }
                }
                Err(_) => {
                    // Empty/invalid value means we've left the index range
                    break;
                }
            }
        }

        Ok(results)
    }

    /// Scan indexedAt index with pagination - O(limit) for both directions
    pub fn scan_indexed_at(
        &self,
        collection: &str,
        cursor_timestamp: Option<u64>,
        cursor_uri: Option<&str>,
        limit: usize,
        direction: ScanDirection,
    ) -> Result<Vec<Record>, StorageError> {
        let prefix = match direction {
            ScanDirection::Ascending => Self::indexed_at_prefix_asc(collection),
            ScanDirection::Descending => Self::indexed_at_prefix_desc(collection),
        };

        let start_key = match (cursor_timestamp, cursor_uri) {
            (Some(ts), Some(uri)) => {
                if let Some((did, rkey)) = parse_did_rkey_from_uri(uri) {
                    match direction {
                        ScanDirection::Ascending => {
                            Self::indexed_at_key_asc(collection, ts, &did, &rkey)
                        }
                        ScanDirection::Descending => {
                            Self::indexed_at_key_desc(collection, ts, &did, &rkey)
                        }
                    }
                } else {
                    prefix.clone()
                }
            }
            _ => prefix.clone(),
        };

        let mut results = Vec::new();
        let mut skip_first = cursor_timestamp.is_some();
        let prefix_str = String::from_utf8_lossy(&prefix);

        for item in self.records.range(start_key..) {
            let key_bytes = item.key()?;
            let key_str = std::str::from_utf8(&key_bytes).unwrap_or("");

            if !key_str.starts_with(&*prefix_str) {
                break;
            }

            if skip_first {
                skip_first = false;
                continue;
            }

            if let Some(uri) = parse_uri_from_indexed_at_key(key_str) {
                if let Ok(parsed_uri) = uri.parse() {
                    if let Ok(Some(record)) = self.get(&parsed_uri) {
                        results.push(record);
                        if results.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    /// Get records keyspace for direct prefix scan access
    pub fn records_keyspace(&self) -> &Keyspace {
        &self.records
    }

    /// Rebuild a specific index from existing records
    pub fn rebuild_index(&self, index: &IndexConfig) -> Result<usize, StorageError> {
        let collection_prefix = format!("{}\0", index.collection);
        let mut count = 0;

        for item in self.records.prefix(collection_prefix.as_bytes()) {
            // Get value first (consumes item)
            let value = item.value()?;

            // Try to parse as Record - skip if it fails (index entries have empty values)
            let record: Record = match serde_json::from_slice(&value) {
                Ok(r) => r,
                Err(_) => continue, // Skip index entries
            };

            // Parse did and rkey from record URI
            if let Some((did, rkey)) = parse_did_rkey_from_uri(&record.uri) {
                self.write_index_entry(index, &record, &did, &rkey)?;
                count += 1;
            }
        }

        Ok(count)
    }

    /// Rebuild __ts__ (indexedAt) indexes for a collection
    pub fn rebuild_indexed_at(&self, collection: &str) -> Result<usize, StorageError> {
        let collection_prefix = format!("{}\0", collection);
        let mut count = 0;

        for item in self.records.prefix(collection_prefix.as_bytes()) {
            let value = item.value()?;

            let record: Record = match serde_json::from_slice(&value) {
                Ok(r) => r,
                Err(_) => continue,
            };

            if let Some((did, rkey)) = parse_did_rkey_from_uri(&record.uri) {
                self.write_indexed_at_entry(collection, record.indexed_at, &did, &rkey)?;
                count += 1;
            }
        }

        Ok(count)
    }
}

/// Parse did and rkey from AT URI
fn parse_did_rkey_from_uri(uri: &str) -> Option<(String, String)> {
    // at://did:plc:xxx/collection/rkey
    let without_scheme = uri.strip_prefix("at://")?;
    let parts: Vec<&str> = without_scheme.splitn(3, '/').collect();
    if parts.len() == 3 {
        Some((parts[0].to_string(), parts[2].to_string()))
    } else {
        None
    }
}

/// Parse URI from indexedAt key
/// Key format: idx:{a|d}:__ts__:{collection}\0{timestamp}\0{did}\0{rkey}
fn parse_uri_from_indexed_at_key(key: &str) -> Option<String> {
    let parts: Vec<&str> = key.splitn(2, '\0').collect();
    if parts.len() != 2 {
        return None;
    }
    let remainder = parts[1];
    let subparts: Vec<&str> = remainder.splitn(3, '\0').collect();
    if subparts.len() == 3 {
        let _timestamp = subparts[0];
        let did = subparts[1];
        let rkey = subparts[2];
        // Extract collection from prefix: idx:{a|d}:__ts__:{collection}
        let prefix = parts[0];
        // Handle both ascending and descending prefixes
        let collection = prefix
            .strip_prefix("idx:a:__ts__:")
            .or_else(|| prefix.strip_prefix("idx:d:__ts__:"))?;
        return Some(format!("at://{}/{}/{}", did, collection, rkey));
    }
    None
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

    #[test]
    fn test_scan_all_collection_with_index() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        // Insert posts from different DIDs
        for (did, rkey) in [
            ("did:plc:aaa", "1"),
            ("did:plc:bbb", "2"),
            ("did:plc:ccc", "3"),
        ] {
            let uri: AtUri = format!("at://{}/app.bsky.feed.post/{}", did, rkey)
                .parse()
                .unwrap();
            let record = Record {
                uri: uri.to_string(),
                cid: format!("cid-{}", rkey),
                value: serde_json::json!({"text": "hello"}),
                indexed_at: 1000 + rkey.parse::<u64>().unwrap(),
            };
            store.put(&uri, &record).unwrap();
        }

        // Insert a like (different collection)
        let like_uri: AtUri = "at://did:plc:aaa/app.bsky.feed.like/x".parse().unwrap();
        let like_record = Record {
            uri: like_uri.to_string(),
            cid: "cid-like".to_string(),
            value: serde_json::json!({}),
            indexed_at: 2000,
        };
        store.put(&like_uri, &like_record).unwrap();

        // Query all posts (should use timeline index)
        let collection: Nsid = "app.bsky.feed.post".parse().unwrap();
        let posts = store.scan_all_collection(&collection).unwrap();
        assert_eq!(posts.len(), 3);

        // Query all likes
        let like_collection: Nsid = "app.bsky.feed.like".parse().unwrap();
        let likes = store.scan_all_collection(&like_collection).unwrap();
        assert_eq!(likes.len(), 1);
    }
}
