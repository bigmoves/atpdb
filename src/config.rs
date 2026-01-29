use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("storage error: {0}")]
    Storage(#[from] fjall::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Manual,
    Signal,
    FullNetwork,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Manual => write!(f, "manual"),
            Mode::Signal => write!(f, "signal"),
            Mode::FullNetwork => write!(f, "full-network"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum IndexDirection {
    Asc,
    #[default]
    Desc,
}

impl std::fmt::Display for IndexDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexDirection::Asc => write!(f, "asc"),
            IndexDirection::Desc => write!(f, "desc"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexConfig {
    pub collection: String,
    pub field: String,
    pub field_type: IndexFieldType,
    #[serde(default)]
    pub direction: IndexDirection,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IndexFieldType {
    Datetime,
    Integer,
}

impl std::fmt::Display for IndexFieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexFieldType::Datetime => write!(f, "datetime"),
            IndexFieldType::Integer => write!(f, "integer"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchFieldConfig {
    pub collection: String,
    pub field: String,
}

impl SearchFieldConfig {
    /// Parse from string format: "collection:field"
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.splitn(2, ':').collect();
        if parts.len() != 2 {
            return None;
        }
        Some(SearchFieldConfig {
            collection: parts[0].to_string(),
            field: parts[1].to_string(),
        })
    }

    /// Format as string: "collection:field"
    pub fn to_config_string(&self) -> String {
        format!("{}:{}", self.collection, self.field)
    }
}

impl IndexConfig {
    /// Parse from string format: "collection:field:type[:direction]"
    /// Direction is optional, defaults to "desc"
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() < 3 || parts.len() > 4 {
            return None;
        }
        let field_type = match parts[2] {
            "datetime" => IndexFieldType::Datetime,
            "integer" => IndexFieldType::Integer,
            _ => return None,
        };
        let direction = if parts.len() == 4 {
            match parts[3] {
                "asc" => IndexDirection::Asc,
                "desc" => IndexDirection::Desc,
                _ => return None,
            }
        } else {
            IndexDirection::Desc // default
        };
        Some(IndexConfig {
            collection: parts[0].to_string(),
            field: parts[1].to_string(),
            field_type,
            direction,
        })
    }

    /// Format as string: "collection:field:type:direction"
    pub fn to_config_string(&self) -> String {
        format!("{}:{}:{}:{}", self.collection, self.field, self.field_type, self.direction)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub mode: Mode,
    pub signal_collection: Option<String>,
    pub collections: Vec<String>,
    pub relay: String,
    pub sync_parallelism: u32,
    #[serde(default)]
    pub indexes: Vec<IndexConfig>,
    #[serde(default)]
    pub search_fields: Vec<SearchFieldConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: Mode::Manual,
            signal_collection: None,
            collections: Vec::new(),
            relay: "bsky.network".to_string(),
            sync_parallelism: 3,
            indexes: Vec::new(),
            search_fields: Vec::new(),
        }
    }
}

impl Config {
    /// Apply environment variable overrides
    pub fn apply_env_overrides(&mut self) {
        if let Ok(mode) = std::env::var("ATPDB_MODE") {
            self.mode = match mode.as_str() {
                "manual" => Mode::Manual,
                "signal" => Mode::Signal,
                "full-network" => Mode::FullNetwork,
                _ => self.mode,
            };
        }

        if let Ok(relay) = std::env::var("ATPDB_RELAY") {
            self.relay = relay;
        }

        if let Ok(signal) = std::env::var("ATPDB_SIGNAL_COLLECTION") {
            self.signal_collection = Some(signal);
        }

        if let Ok(collections) = std::env::var("ATPDB_COLLECTIONS") {
            self.collections = collections.split(',').map(|s| s.trim().to_string()).collect();
        }

        if let Ok(parallelism) = std::env::var("ATPDB_SYNC_PARALLELISM") {
            if let Ok(n) = parallelism.parse() {
                self.sync_parallelism = n;
            }
        }

        if let Ok(indexes_str) = std::env::var("ATPDB_INDEXES") {
            self.indexes = indexes_str
                .split(',')
                .filter_map(|s| IndexConfig::parse(s.trim()))
                .collect();
        }

        if let Ok(search_str) = std::env::var("ATPDB_SEARCH_FIELDS") {
            self.search_fields = search_str
                .split(',')
                .filter_map(|s| SearchFieldConfig::parse(s.trim()))
                .collect();
        }
    }

    pub fn is_field_indexed(&self, collection: &str, field: &str, direction: IndexDirection) -> bool {
        self.indexes
            .iter()
            .any(|idx| idx.collection == collection && idx.field == field && idx.direction == direction)
    }

    pub fn is_field_searchable(&self, collection: &str, field: &str) -> bool {
        self.search_fields
            .iter()
            .any(|sf| sf.collection == collection && sf.field == field)
    }
}

/// Check if a collection matches a filter pattern
/// Supports wildcards at NSID segment boundaries: "fm.teal.*" matches "fm.teal.alpha.feed"
pub fn matches_collection_filter(collection: &str, filter: &str) -> bool {
    if filter == "*" {
        return true;
    }
    if let Some(prefix) = filter.strip_suffix(".*") {
        collection.starts_with(prefix)
            && collection.len() > prefix.len()
            && collection.as_bytes()[prefix.len()] == b'.'
    } else {
        collection == filter
    }
}

/// Check if collection matches any of the filters (empty filters = match all)
pub fn matches_any_filter(collection: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters
        .iter()
        .any(|f| matches_collection_filter(collection, f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fjall::{Database, KeyspaceCreateOptions};
    use tempfile::tempdir;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.mode, Mode::Manual);
        assert_eq!(config.relay, "bsky.network");
        assert!(config.collections.is_empty());
    }

    #[test]
    fn test_config_store() {
        let dir = tempdir().unwrap();
        let db = Database::builder(dir.path()).open().unwrap();
        let partition = db.keyspace("config", KeyspaceCreateOptions::default).unwrap();

        const CONFIG_KEY: &[u8] = b"config";

        // Should return default initially
        let config: Config = match partition.get(CONFIG_KEY).unwrap() {
            Some(bytes) => serde_json::from_slice(&bytes).unwrap(),
            None => Config::default(),
        };
        assert_eq!(config.mode, Mode::Manual);

        // Update and read back
        let config = Config {
            mode: Mode::Signal,
            signal_collection: Some("fm.teal.alpha.feed".to_string()),
            collections: vec!["fm.teal.*".to_string()],
            ..Default::default()
        };
        let value = serde_json::to_vec(&config).unwrap();
        partition.insert(CONFIG_KEY, &value).unwrap();

        let loaded: Config = serde_json::from_slice(&partition.get(CONFIG_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(loaded.mode, Mode::Signal);
        assert_eq!(
            loaded.signal_collection,
            Some("fm.teal.alpha.feed".to_string())
        );
    }

    #[test]
    fn test_matches_collection_filter_exact() {
        assert!(matches_collection_filter(
            "app.bsky.feed.post",
            "app.bsky.feed.post"
        ));
        assert!(!matches_collection_filter(
            "app.bsky.feed.like",
            "app.bsky.feed.post"
        ));
    }

    #[test]
    fn test_matches_collection_filter_wildcard() {
        assert!(matches_collection_filter(
            "fm.teal.alpha.feed",
            "fm.teal.*"
        ));
        assert!(matches_collection_filter(
            "fm.teal.alpha.like",
            "fm.teal.*"
        ));
        assert!(matches_collection_filter("fm.teal.beta", "fm.teal.*"));
        assert!(!matches_collection_filter("fm.teal", "fm.teal.*")); // Must have segment after
        assert!(!matches_collection_filter(
            "app.bsky.feed.post",
            "fm.teal.*"
        ));
    }

    #[test]
    fn test_matches_any_filter() {
        let filters = vec!["fm.teal.*".to_string(), "app.bsky.feed.post".to_string()];
        assert!(matches_any_filter("fm.teal.alpha.feed", &filters));
        assert!(matches_any_filter("app.bsky.feed.post", &filters));
        assert!(!matches_any_filter("app.bsky.feed.like", &filters));

        // Empty filters match all
        assert!(matches_any_filter("anything", &[]));
    }

    #[test]
    fn test_search_field_config_parse() {
        let config = SearchFieldConfig::parse("app.bsky.feed.post:text").unwrap();
        assert_eq!(config.collection, "app.bsky.feed.post");
        assert_eq!(config.field, "text");

        // Nested field
        let config2 = SearchFieldConfig::parse("app.bsky.actor.profile:description").unwrap();
        assert_eq!(config2.collection, "app.bsky.actor.profile");
        assert_eq!(config2.field, "description");

        // Invalid format
        assert!(SearchFieldConfig::parse("invalid").is_none());
    }

    #[test]
    fn test_search_field_config_to_string() {
        let config = SearchFieldConfig {
            collection: "app.bsky.feed.post".to_string(),
            field: "text".to_string(),
        };
        assert_eq!(config.to_config_string(), "app.bsky.feed.post:text");
    }
}
