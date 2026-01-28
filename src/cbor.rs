use libipld::Cid;

/// Convert DAG-CBOR bytes to JSON, handling CID links as {"$link": "..."}
pub fn dagcbor_to_json(data: &[u8]) -> Result<serde_json::Value, String> {
    let cbor_value: ciborium::Value =
        ciborium::from_reader(data).map_err(|e| e.to_string())?;
    cbor_to_json(cbor_value)
}

fn cbor_to_json(value: ciborium::Value) -> Result<serde_json::Value, String> {
    match value {
        ciborium::Value::Null => Ok(serde_json::Value::Null),
        ciborium::Value::Bool(b) => Ok(serde_json::Value::Bool(b)),
        ciborium::Value::Integer(i) => {
            let n: i128 = i.into();
            if let Ok(n) = i64::try_from(n) {
                Ok(serde_json::Value::Number(n.into()))
            } else {
                Ok(serde_json::Value::String(n.to_string()))
            }
        }
        ciborium::Value::Float(f) => {
            serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .ok_or_else(|| "invalid float".to_string())
        }
        ciborium::Value::Bytes(b) => {
            // Encode bytes as base64 with $bytes marker
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&b);
            Ok(serde_json::json!({"$bytes": encoded}))
        }
        ciborium::Value::Text(s) => Ok(serde_json::Value::String(s)),
        ciborium::Value::Array(arr) => {
            let items: Result<Vec<_>, _> = arr.into_iter().map(cbor_to_json).collect();
            Ok(serde_json::Value::Array(items?))
        }
        ciborium::Value::Map(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                let key = match k {
                    ciborium::Value::Text(s) => s,
                    _ => return Err("non-string map key".to_string()),
                };
                obj.insert(key, cbor_to_json(v)?);
            }
            Ok(serde_json::Value::Object(obj))
        }
        ciborium::Value::Tag(42, inner) => {
            // CID link - convert to {"$link": "..."}
            if let ciborium::Value::Bytes(bytes) = *inner {
                let cid_bytes = if bytes.first() == Some(&0x00) {
                    &bytes[1..]
                } else {
                    &bytes
                };
                if let Ok(cid) = Cid::try_from(cid_bytes) {
                    return Ok(serde_json::json!({"$link": cid.to_string()}));
                }
            }
            Err("invalid CID link".to_string())
        }
        ciborium::Value::Tag(_, inner) => {
            // Other tags - just unwrap
            cbor_to_json(*inner)
        }
        _ => Err("unsupported CBOR type".to_string()),
    }
}
