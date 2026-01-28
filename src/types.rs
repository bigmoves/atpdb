use std::fmt;
use std::str::FromStr;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum ParseError {
    #[error("invalid DID: {0}")]
    Did(String),
    #[error("invalid NSID: {0}")]
    Nsid(String),
    #[error("invalid rkey: {0}")]
    Rkey(String),
    #[error("invalid AT-URI: {0}")]
    AtUri(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Did(String);

impl Did {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Did {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with("did:plc:") || s.starts_with("did:web:") {
            Ok(Did(s.to_string()))
        } else {
            Err(ParseError::Did(s.to_string()))
        }
    }
}

impl fmt::Display for Did {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Nsid(String);

impl Nsid {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Nsid {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Basic validation: at least one dot, no spaces
        if s.contains('.') && !s.contains(' ') && !s.is_empty() {
            Ok(Nsid(s.to_string()))
        } else {
            Err(ParseError::Nsid(s.to_string()))
        }
    }
}

impl fmt::Display for Nsid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rkey(String);

impl Rkey {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Rkey {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if !s.is_empty() && !s.contains('/') {
            Ok(Rkey(s.to_string()))
        } else {
            Err(ParseError::Rkey(s.to_string()))
        }
    }
}

impl fmt::Display for Rkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AtUri {
    pub did: Did,
    pub collection: Nsid,
    pub rkey: Rkey,
}

impl AtUri {
    pub fn to_storage_key(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }
}

impl FromStr for AtUri {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s
            .strip_prefix("at://")
            .ok_or_else(|| ParseError::AtUri(s.to_string()))?;

        let parts: Vec<&str> = s.splitn(3, '/').collect();
        if parts.len() != 3 {
            return Err(ParseError::AtUri(s.to_string()));
        }

        let did: Did = parts[0].parse()?;
        let collection: Nsid = parts[1].parse()?;
        let rkey: Rkey = parts[2].parse()?;

        Ok(AtUri {
            did,
            collection,
            rkey,
        })
    }
}

impl fmt::Display for AtUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at://{}/{}/{}", self.did, self.collection, self.rkey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_did_parse_plc() {
        let did: Did = "did:plc:z72i7hdynmk6r22z27h6tvur".parse().unwrap();
        assert_eq!(did.as_str(), "did:plc:z72i7hdynmk6r22z27h6tvur");
    }

    #[test]
    fn test_did_parse_web() {
        let did: Did = "did:web:example.com".parse().unwrap();
        assert_eq!(did.as_str(), "did:web:example.com");
    }

    #[test]
    fn test_did_parse_invalid() {
        let result: Result<Did, _> = "not-a-did".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_nsid_parse() {
        let nsid: Nsid = "app.bsky.feed.post".parse().unwrap();
        assert_eq!(nsid.as_str(), "app.bsky.feed.post");
    }

    #[test]
    fn test_nsid_parse_invalid() {
        let result: Result<Nsid, _> = "noDots".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_rkey_parse() {
        let rkey: Rkey = "3k2a1b".parse().unwrap();
        assert_eq!(rkey.as_str(), "3k2a1b");
    }

    #[test]
    fn test_rkey_self() {
        let rkey: Rkey = "self".parse().unwrap();
        assert_eq!(rkey.as_str(), "self");
    }

    #[test]
    fn test_aturi_parse() {
        let uri: AtUri = "at://did:plc:xyz/app.bsky.feed.post/3k2a1b"
            .parse()
            .unwrap();
        assert_eq!(uri.did.as_str(), "did:plc:xyz");
        assert_eq!(uri.collection.as_str(), "app.bsky.feed.post");
        assert_eq!(uri.rkey.as_str(), "3k2a1b");
    }

    #[test]
    fn test_aturi_roundtrip() {
        let original = "at://did:plc:z72i7hdynmk6r22z27h6tvur/app.bsky.feed.post/3l2s5xxv2ze2c";
        let uri: AtUri = original.parse().unwrap();
        assert_eq!(uri.to_string(), original);
    }

    #[test]
    fn test_aturi_storage_key() {
        let uri: AtUri = "at://did:plc:xyz/app.bsky.feed.post/abc".parse().unwrap();
        let key = uri.to_storage_key();
        assert_eq!(key, b"at://did:plc:xyz/app.bsky.feed.post/abc");
    }
}
