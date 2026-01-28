use crate::storage::{Record, StorageError, Store};
use crate::types::{AtUri, Did, Nsid, ParseError};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum QueryError {
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("invalid query: {0}")]
    Invalid(String),
}

#[derive(Debug, PartialEq)]
pub enum Query {
    Exact(AtUri),
    Collection { did: Did, collection: Nsid },
}

impl Query {
    pub fn parse(input: &str) -> Result<Self, QueryError> {
        let input = input.trim();

        if !input.starts_with("at://") {
            return Err(QueryError::Invalid("must start with at://".to_string()));
        }

        let without_scheme = &input[5..]; // Remove "at://"
        let parts: Vec<&str> = without_scheme.splitn(3, '/').collect();

        if parts.len() != 3 {
            return Err(QueryError::Invalid(
                "expected did/collection/rkey".to_string(),
            ));
        }

        let did: Did = parts[0].parse()?;
        let collection: Nsid = parts[1].parse()?;
        let rkey = parts[2];

        if rkey == "*" {
            Ok(Query::Collection { did, collection })
        } else {
            let uri: AtUri = input.parse()?;
            Ok(Query::Exact(uri))
        }
    }
}

pub fn execute(query: &Query, store: &Store) -> Result<Vec<Record>, QueryError> {
    match query {
        Query::Exact(uri) => match store.get(uri)? {
            Some(record) => Ok(vec![record]),
            None => Ok(vec![]),
        },
        Query::Collection { did, collection } => Ok(store.scan_collection(did, collection)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_exact() {
        let q = Query::parse("at://did:plc:xyz/app.bsky.feed.post/abc").unwrap();
        match q {
            Query::Exact(uri) => {
                assert_eq!(uri.rkey.as_str(), "abc");
            }
            _ => panic!("expected Exact"),
        }
    }

    #[test]
    fn test_parse_collection() {
        let q = Query::parse("at://did:plc:xyz/app.bsky.feed.post/*").unwrap();
        match q {
            Query::Collection { did, collection } => {
                assert_eq!(did.as_str(), "did:plc:xyz");
                assert_eq!(collection.as_str(), "app.bsky.feed.post");
            }
            _ => panic!("expected Collection"),
        }
    }

    #[test]
    fn test_parse_invalid() {
        assert!(Query::parse("not-a-uri").is_err());
        assert!(Query::parse("at://did:plc:xyz").is_err());
    }
}
