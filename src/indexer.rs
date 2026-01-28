use crate::types::AtUri;
use serde_json::Value;

#[allow(dead_code)]
pub fn extract_refs(value: &Value) -> Vec<AtUri> {
    let mut refs = Vec::new();
    walk_json(value, &mut refs);
    refs
}

fn walk_json(value: &Value, refs: &mut Vec<AtUri>) {
    match value {
        Value::String(s) if s.starts_with("at://") => {
            if let Ok(uri) = s.parse::<AtUri>() {
                refs.push(uri);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                walk_json(v, refs);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                walk_json(v, refs);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_no_refs() {
        let value = json!({"text": "hello world"});
        let refs = extract_refs(&value);
        assert!(refs.is_empty());
    }

    #[test]
    fn test_extract_single_ref() {
        let value = json!({
            "subject": {
                "uri": "at://did:plc:xyz/app.bsky.feed.post/abc"
            }
        });
        let refs = extract_refs(&value);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].to_string(),
            "at://did:plc:xyz/app.bsky.feed.post/abc"
        );
    }

    #[test]
    fn test_extract_multiple_refs() {
        let value = json!({
            "reply": {
                "root": {"uri": "at://did:plc:a/app.bsky.feed.post/1"},
                "parent": {"uri": "at://did:plc:b/app.bsky.feed.post/2"}
            }
        });
        let refs = extract_refs(&value);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_extract_refs_in_array() {
        let value = json!({
            "items": [
                {"uri": "at://did:plc:a/app.bsky.feed.post/1"},
                {"uri": "at://did:plc:b/app.bsky.feed.post/2"}
            ]
        });
        let refs = extract_refs(&value);
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_ignore_invalid_at_uri() {
        let value = json!({"text": "at://not-valid"});
        let refs = extract_refs(&value);
        assert!(refs.is_empty());
    }
}
