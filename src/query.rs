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
    AllCollection { collection: Nsid },
    AllForDid { did: Did },
}

impl Query {
    pub fn parse(input: &str) -> Result<Self, QueryError> {
        let input = input.trim();

        if !input.starts_with("at://") {
            return Err(QueryError::Invalid("must start with at://".to_string()));
        }

        let without_scheme = &input[5..]; // Remove "at://"
        let parts: Vec<&str> = without_scheme.splitn(3, '/').collect();

        match parts.len() {
            // at://did or at://did/*
            1 => {
                let did_str = parts[0];
                if did_str == "*" {
                    return Err(QueryError::Invalid(
                        "at://* is not supported, use at://*/collection/*".to_string(),
                    ));
                }
                let did: Did = did_str.parse()?;
                Ok(Query::AllForDid { did })
            }
            // at://did/collection (treat as at://did/collection/*)
            2 => {
                let did_str = parts[0];
                let collection_str = parts[1];

                // at://did/* - all collections for DID
                if collection_str == "*" {
                    if did_str == "*" {
                        return Err(QueryError::Invalid(
                            "at://*/* is not supported".to_string(),
                        ));
                    }
                    let did: Did = did_str.parse()?;
                    return Ok(Query::AllForDid { did });
                }

                let collection: Nsid = collection_str.parse()?;

                if did_str == "*" {
                    // at://*/collection - treat as at://*/collection/*
                    return Ok(Query::AllCollection { collection });
                }

                let did: Did = did_str.parse()?;
                Ok(Query::Collection { did, collection })
            }
            // at://did/collection/rkey
            3 => {
                let did_str = parts[0];
                let collection: Nsid = parts[1].parse()?;
                let rkey = parts[2];

                // Handle wildcard DID: at://*/collection/*
                if did_str == "*" {
                    if rkey != "*" {
                        return Err(QueryError::Invalid(
                            "wildcard DID requires wildcard rkey (at://*/collection/*)".to_string(),
                        ));
                    }
                    return Ok(Query::AllCollection { collection });
                }

                let did: Did = did_str.parse()?;

                if rkey == "*" {
                    Ok(Query::Collection { did, collection })
                } else {
                    let uri: AtUri = input.parse()?;
                    Ok(Query::Exact(uri))
                }
            }
            _ => Err(QueryError::Invalid("invalid query format".to_string())),
        }
    }
}

pub fn execute(query: &Query, store: &Store) -> Result<Vec<Record>, QueryError> {
    execute_paginated(query, store, None, usize::MAX)
}

pub fn execute_paginated(
    query: &Query,
    store: &Store,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Vec<Record>, QueryError> {
    match query {
        Query::Exact(uri) => match store.get(uri)? {
            Some(record) => Ok(vec![record]),
            None => Ok(vec![]),
        },
        Query::Collection { did, collection } => {
            // For collection query, cursor is just the rkey
            Ok(store.scan_collection_paginated(did, collection, cursor, limit)?)
        }
        Query::AllCollection { collection } => {
            // For all collection query, cursor is the full URI
            Ok(store.scan_all_collection_paginated(collection, cursor, limit)?)
        }
        Query::AllForDid { did } => {
            // Scan all collections for this DID
            Ok(store.scan_did_paginated(did, cursor, limit)?)
        }
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
    fn test_parse_all_collection() {
        let q = Query::parse("at://*/app.bsky.feed.post/*").unwrap();
        match q {
            Query::AllCollection { collection } => {
                assert_eq!(collection.as_str(), "app.bsky.feed.post");
            }
            _ => panic!("expected AllCollection"),
        }
    }

    #[test]
    fn test_parse_wildcard_did_requires_wildcard_rkey() {
        assert!(Query::parse("at://*/app.bsky.feed.post/specific-rkey").is_err());
    }

    #[test]
    fn test_parse_all_for_did() {
        // at://did returns all records for that DID
        let q = Query::parse("at://did:plc:xyz").unwrap();
        match q {
            Query::AllForDid { did } => {
                assert_eq!(did.as_str(), "did:plc:xyz");
            }
            _ => panic!("expected AllForDid"),
        }
    }

    #[test]
    fn test_parse_all_for_did_with_wildcard() {
        // at://did/* also returns all records for that DID
        let q = Query::parse("at://did:plc:xyz/*").unwrap();
        match q {
            Query::AllForDid { did } => {
                assert_eq!(did.as_str(), "did:plc:xyz");
            }
            _ => panic!("expected AllForDid"),
        }
    }

    #[test]
    fn test_parse_collection_shorthand() {
        // at://did/collection (without /*) is shorthand for at://did/collection/*
        let q = Query::parse("at://did:plc:xyz/app.bsky.feed.post").unwrap();
        match q {
            Query::Collection { did, collection } => {
                assert_eq!(did.as_str(), "did:plc:xyz");
                assert_eq!(collection.as_str(), "app.bsky.feed.post");
            }
            _ => panic!("expected Collection"),
        }
    }

    #[test]
    fn test_parse_all_collection_shorthand() {
        // at://*/collection (without /*) is shorthand for at://*/collection/*
        let q = Query::parse("at://*/app.bsky.feed.post").unwrap();
        match q {
            Query::AllCollection { collection } => {
                assert_eq!(collection.as_str(), "app.bsky.feed.post");
            }
            _ => panic!("expected AllCollection"),
        }
    }

    #[test]
    fn test_parse_invalid() {
        assert!(Query::parse("not-a-uri").is_err());
        assert!(Query::parse("at://*").is_err()); // wildcard-only DID not allowed
        assert!(Query::parse("at://*/*").is_err()); // double wildcard not allowed
    }
}
