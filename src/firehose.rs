use crate::cbor::dagcbor_to_json;
use crate::types::{AtUri, Did};
use metrics::counter;
use serde::Deserialize;
use std::io::Cursor;
use std::time::Duration;
use thiserror::Error;
use tungstenite::{connect, Message};

/// Convert HTTP/HTTPS URL to WebSocket URL
fn to_websocket_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{}", rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{}", rest)
    } else if url.starts_with("wss://") || url.starts_with("ws://") {
        url.to_string()
    } else {
        // Assume https if no scheme
        format!("wss://{}", url)
    }
}

#[derive(Error, Debug)]
pub enum FirehoseError {
    #[error("websocket error: {0}")]
    WebSocket(#[from] Box<tungstenite::Error>),
    #[error("cbor decode error: {0}")]
    Cbor(String),
    #[error("invalid message")]
    InvalidMessage,
}

#[derive(Debug)]
pub enum Event {
    Commit {
        seq: i64,
        did: Did,
        rev: String,
        operations: Vec<Operation>,
    },
    Identity {
        seq: i64,
        did: Did,
        handle: Option<String>,
    },
    Unknown {
        seq: Option<i64>,
    },
}

#[derive(Debug)]
pub enum Operation {
    Create {
        uri: AtUri,
        cid: String,
        value: serde_json::Value,
    },
    Update {
        uri: AtUri,
        cid: String,
        value: serde_json::Value,
    },
    Delete {
        uri: AtUri,
    },
}

#[derive(Debug, Deserialize)]
struct Header {
    #[allow(dead_code)]
    op: i32,
    t: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommitBody {
    seq: i64,
    repo: String,
    rev: String,
    ops: Vec<RepoOp>,
    #[serde(with = "serde_bytes")]
    blocks: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct IdentityBody {
    seq: i64,
    did: String,
    handle: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RepoOp {
    action: String,
    path: String,
    cid: Option<CidLink>,
}

// CBOR CID link (tag 42 with bytes)
#[derive(Debug)]
struct CidLink(libipld::Cid);

impl<'de> serde::Deserialize<'de> for CidLink {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let value = ciborium::Value::deserialize(deserializer)?;

        // CID links are CBOR tag 42 containing bytes
        if let ciborium::Value::Tag(42, inner) = value {
            if let ciborium::Value::Bytes(bytes) = *inner {
                // Skip the first byte (0x00 multibase prefix)
                let cid_bytes = if bytes.first() == Some(&0x00) {
                    &bytes[1..]
                } else {
                    &bytes
                };
                let cid = libipld::Cid::try_from(cid_bytes)
                    .map_err(|e| D::Error::custom(format!("invalid CID: {}", e)))?;
                return Ok(CidLink(cid));
            }
        }
        Err(D::Error::custom("expected CBOR tag 42 with bytes"))
    }
}

pub struct FirehoseClient {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

impl FirehoseClient {
    pub fn connect(relay: &str, cursor: Option<i64>) -> Result<Self, FirehoseError> {
        let base = to_websocket_url(relay);
        let url = match cursor {
            Some(seq) => format!("{}/xrpc/com.atproto.sync.subscribeRepos?cursor={}", base.trim_end_matches('/'), seq),
            None => format!("{}/xrpc/com.atproto.sync.subscribeRepos", base.trim_end_matches('/')),
        };
        let (socket, _response) = connect(&url).map_err(Box::new)?;

        // Set read timeout so we can check shutdown flag periodically
        if let tungstenite::stream::MaybeTlsStream::NativeTls(ref stream) = socket.get_ref() {
            let _ = stream.get_ref().set_read_timeout(Some(Duration::from_secs(1)));
        } else if let tungstenite::stream::MaybeTlsStream::Plain(ref stream) = socket.get_ref() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        }

        Ok(FirehoseClient { socket })
    }

    pub fn next_event(&mut self) -> Result<Option<Event>, FirehoseError> {
        loop {
            match self.socket.read() {
                Ok(Message::Binary(data)) => {
                    return self.decode_message(&data);
                }
                Ok(Message::Close(_)) => return Ok(None),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                    // Timeout - return None to let caller check shutdown flag
                    return Ok(None);
                }
                Err(e) => return Err(Box::new(e).into()),
            }
        }
    }

    fn decode_message(&self, data: &[u8]) -> Result<Option<Event>, FirehoseError> {
        let mut cursor = Cursor::new(data);

        // Decode header
        let header: Header =
            ciborium::from_reader(&mut cursor).map_err(|e| FirehoseError::Cbor(e.to_string()))?;

        // Handle different event types
        match header.t.as_deref() {
            Some("#identity") => {
                let body: IdentityBody = ciborium::from_reader(&mut cursor)
                    .map_err(|e| FirehoseError::Cbor(e.to_string()))?;
                let did: Did = body.did.parse().map_err(|_| FirehoseError::InvalidMessage)?;
                return Ok(Some(Event::Identity {
                    seq: body.seq,
                    did,
                    handle: body.handle,
                }));
            }
            Some("#commit") => {
                // Continue to commit handling below
            }
            _ => {
                return Ok(Some(Event::Unknown { seq: None }));
            }
        }

        // Decode commit body
        let body: CommitBody =
            ciborium::from_reader(&mut cursor).map_err(|e| FirehoseError::Cbor(e.to_string()))?;

        let did: Did = body
            .repo
            .parse()
            .map_err(|_| FirehoseError::InvalidMessage)?;

        // Parse CAR blocks to get record data
        let blocks = self.parse_car_blocks(&body.blocks)?;

        let mut operations = Vec::new();
        for op in body.ops {
            match op.action.as_str() {
                "create" => {
                    counter!("firehose_operations_total", "op" => "create").increment(1);
                    if let Some(cid_link) = &op.cid {
                        let cid = &cid_link.0;
                        if let Some(value) = blocks.get(&cid.to_string()) {
                            let parts: Vec<&str> = op.path.splitn(2, '/').collect();
                            if parts.len() == 2 {
                                let uri_str = format!("at://{}/{}", did, op.path);
                                if let Ok(uri) = uri_str.parse() {
                                    operations.push(Operation::Create {
                                        uri,
                                        cid: cid.to_string(),
                                        value: value.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
                "update" => {
                    counter!("firehose_operations_total", "op" => "update").increment(1);
                    if let Some(cid_link) = &op.cid {
                        let cid = &cid_link.0;
                        if let Some(value) = blocks.get(&cid.to_string()) {
                            let parts: Vec<&str> = op.path.splitn(2, '/').collect();
                            if parts.len() == 2 {
                                let uri_str = format!("at://{}/{}", did, op.path);
                                if let Ok(uri) = uri_str.parse() {
                                    operations.push(Operation::Update {
                                        uri,
                                        cid: cid.to_string(),
                                        value: value.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
                "delete" => {
                    counter!("firehose_operations_total", "op" => "delete").increment(1);
                    let uri_str = format!("at://{}/{}", did, op.path);
                    if let Ok(uri) = uri_str.parse() {
                        operations.push(Operation::Delete { uri });
                    }
                }
                _ => {}
            }
        }

        Ok(Some(Event::Commit {
            seq: body.seq,
            did,
            rev: body.rev,
            operations,
        }))
    }

    fn parse_car_blocks(
        &self,
        car_bytes: &[u8],
    ) -> Result<std::collections::HashMap<String, serde_json::Value>, FirehoseError> {
        let mut blocks = std::collections::HashMap::new();

        if car_bytes.is_empty() {
            return Ok(blocks);
        }

        let mut cursor = Cursor::new(car_bytes);

        // Read CAR header (varint length + dag-cbor header)
        let header_len =
            read_varint(&mut cursor).map_err(|e| FirehoseError::Cbor(e.to_string()))?;

        // Skip header bytes
        let pos = cursor.position() as usize;
        if pos + header_len > car_bytes.len() {
            return Ok(blocks);
        }
        cursor.set_position((pos + header_len) as u64);

        // Read blocks
        while (cursor.position() as usize) < car_bytes.len() {
            let block_start = cursor.position() as usize;

            let block_len = match read_varint(&mut cursor) {
                Ok(len) => len,
                Err(_) => break,
            };

            let cid_start = cursor.position() as usize;
            if cid_start + block_len > car_bytes.len() {
                break;
            }

            // Parse CID
            let cid = match parse_cid(&car_bytes[cid_start..]) {
                Ok((cid, cid_len)) => {
                    cursor.set_position((cid_start + cid_len) as u64);
                    cid
                }
                Err(_) => break,
            };

            // Read block data
            let data_start = cursor.position() as usize;
            let data_end = block_start + block_len + varint_len(block_len);

            if data_end > car_bytes.len() {
                break;
            }

            let block_data = &car_bytes[data_start..data_end];
            cursor.set_position(data_end as u64);

            // Decode dag-cbor to JSON with proper CID link handling
            if let Ok(value) = dagcbor_to_json(block_data) {
                blocks.insert(cid, value);
            }
        }

        Ok(blocks)
    }
}

fn read_varint<R: std::io::Read>(reader: &mut R) -> Result<usize, std::io::Error> {
    let mut result: usize = 0;
    let mut shift = 0;

    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;

        result |= ((byte[0] & 0x7f) as usize) << shift;
        if byte[0] & 0x80 == 0 {
            break;
        }
        shift += 7;

        if shift > 63 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "varint too long",
            ));
        }
    }

    Ok(result)
}

fn varint_len(n: usize) -> usize {
    let mut len = 1;
    let mut n = n;
    while n >= 0x80 {
        len += 1;
        n >>= 7;
    }
    len
}

fn parse_cid(data: &[u8]) -> Result<(String, usize), &'static str> {
    if data.is_empty() {
        return Err("empty cid");
    }

    // CIDv1: version (1) + codec + multihash
    if data[0] == 0x01 {
        // Read codec varint
        let mut pos = 1;
        while pos < data.len() && data[pos] & 0x80 != 0 {
            pos += 1;
        }
        pos += 1; // Include last byte of codec

        if pos >= data.len() {
            return Err("truncated cid");
        }

        // Read multihash: hash_type + hash_len + hash_bytes
        while pos < data.len() && data[pos] & 0x80 != 0 {
            pos += 1;
        }
        pos += 1; // hash type

        if pos >= data.len() {
            return Err("truncated cid");
        }

        let hash_len = data[pos] as usize;
        pos += 1;
        pos += hash_len;

        let cid_bytes = &data[..pos];
        let cid = libipld::Cid::try_from(cid_bytes).map_err(|_| "invalid cid")?;

        Ok((cid.to_string(), pos))
    } else {
        Err("unsupported cid version")
    }
}
