//! `lyrics.get {ref}` (capability `lyrics`): `track/lyricsUrl` hands out a
//! temporary URL of a JSON document:
//!
//! ```json
//! {"track_id": "287336643",
//!  "original": {"type": "lsync", "lang": "en",
//!               "lines": [{"line": "…", "start": "5400", "end": "10400"}]}}
//! ```
//!
//! `lsync` lines (times in ms) become `synced`; any other type, or lines
//! without times, becomes `plain`. Tracks without lyrics answer 404; they
//! are remembered for a while so that the host's retries stay local.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::items::Ref;
use super::rpc::{RpcError, RpcResult};
use crate::api::{ApiClient, HttpError};

/// How long a track without lyrics is not asked about again.
const MISSING_TTL: Duration = Duration::from_secs(6 * 3600);
const MAX_MISSING: usize = 2000;

static MISSING: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

fn known_missing(id: &str) -> bool {
    let mut m = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    m.retain(|_, at| at.elapsed() < MISSING_TTL);
    m.contains_key(id)
}

fn note_missing(id: &str) {
    let mut m = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    if m.len() >= MAX_MISSING {
        m.clear();
    }
    m.insert(id.to_string(), Instant::now());
}

fn not_found(id: &str) -> RpcError {
    RpcError::not_found(format!("no lyrics for track {id}"))
}

/// Time of a line: a number or a numeric string, in ms.
fn millis(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The protocol answer from the lyrics document, `None` when it holds none.
pub fn convert(doc: &Value) -> Option<Value> {
    let original = doc.get("original")?;
    let lines = original.get("lines").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let text = |l: &Value| l.get("line").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let synced_type = original.get("type").and_then(Value::as_str).is_some_and(|t| t == "lsync");
    let timed: Vec<Value> = lines
        .iter()
        .filter_map(|l| Some(json!({"time_ms": millis(l.get("start"))?, "text": text(l)})))
        .collect();
    if synced_type && !timed.is_empty() && timed.len() == lines.len() {
        return Some(json!({"synced": timed}));
    }
    let plain = lines.iter().map(text).collect::<Vec<_>>().join("\n");
    let plain = plain.trim();
    (!plain.is_empty()).then(|| json!({"plain": plain}))
}

pub async fn get(api: &ApiClient, reference: &str) -> RpcResult {
    let Some(Ref::Track(id)) = Ref::parse(reference) else {
        return Err(RpcError::not_found(format!("{reference} has no lyrics")));
    };
    if known_missing(&id) {
        return Err(not_found(&id));
    }
    let answer = match api.lyrics_url(&id).await {
        Ok(v) => v,
        Err(e) if e.chain().any(|c| c.downcast_ref::<HttpError>().is_some_and(|h| h.status == 404)) => {
            note_missing(&id);
            return Err(not_found(&id));
        }
        Err(e) => return Err(e.into()),
    };
    // The URL is signed for this account: never logged.
    let url = answer.get("lyrics_url").and_then(Value::as_str).ok_or_else(|| not_found(&id))?;
    let doc = api.get_document(url).await?;
    let doc_track = doc.get("track_id").map(|t| t.to_string().trim_matches('"').to_string());
    if doc_track.as_deref().is_some_and(|t| t != id) {
        return Err(RpcError::not_found(format!("lyrics document of another track than {id}")));
    }
    match convert(&doc) {
        Some(lyrics) => Ok(lyrics),
        None => {
            note_missing(&id);
            Err(not_found(&id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_and_plain_documents() {
        let doc = json!({"track_id": "1", "original": {"type": "lsync", "lines": [
            {"line": "Your blades", "start": "5400", "end": "10400"},
            {"line": "", "start": 10400},
            {"line": " are sharpened ", "start": "12000"}
        ]}});
        assert_eq!(convert(&doc), Some(json!({"synced": [
            {"time_ms": 5400, "text": "Your blades"},
            {"time_ms": 10400, "text": ""},
            {"time_ms": 12000, "text": "are sharpened"}
        ]})));
        let unsynced = json!({"original": {"type": "unsync", "lines": [{"line": "a"}, {"line": "b"}]}});
        assert_eq!(convert(&unsynced), Some(json!({"plain": "a\nb"})));
        let untimed = json!({"original": {"type": "lsync", "lines": [{"line": "a", "start": "1"}, {"line": "b"}]}});
        assert_eq!(convert(&untimed), Some(json!({"plain": "a\nb"})), "a line without a time: plain text");
        assert_eq!(convert(&json!({"original": {"type": "lsync", "lines": []}})), None);
        assert_eq!(convert(&json!({})), None);
    }
}
