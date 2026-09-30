//! Play reports to Qobuz (`reporting` capability, setting
//! `report_playback`), sent the way the web player sends them:
//! - when a track starts: `track/reportStreamingStart` with
//!   `{track_id, date, user_id, format_id}`;
//! - when it ends: an event `{blob, track_context_uuid, start_stream,
//!   online, local, duration}` queued, then sent in batches to
//!   `track/reportStreamingEndJson`. `blob` comes from the
//!   `track/getFileUrl` answer; a play without it is not reported.
//!
//! Unsent end events are kept in `data_dir/play-reports.json`, so that a
//! restart or a network failure does not lose them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{now_unix_s, ApiClient};

/// End events per request, as the web player batches them.
const BATCH: usize = 20;
/// Oldest end events are dropped beyond this many.
const MAX_QUEUED: usize = 500;
/// Resolved streams remembered for a later start (preloads, queues).
const MAX_STREAMS: usize = 64;

/// What `track.resolve` learned about a stream.
#[derive(Clone, Debug)]
struct Stream {
    format_id: i32,
    blob: Option<String>,
    duration_s: Option<u64>,
    resolved_at: i64,
}

/// A play in progress.
#[derive(Clone, Debug)]
struct Play {
    track_id: u32,
    started_at: i64,
    duration_s: Option<u64>,
    blob: Option<String>,
}

#[derive(Default)]
struct Inner {
    streams: HashMap<u32, Stream>,
    playing: HashMap<String, Play>,
    queue: Vec<Value>,
    loaded: bool,
}

#[derive(Default)]
pub struct Reporter {
    inner: Mutex<Inner>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    events: Vec<Value>,
}

fn queue_path(data_dir: &Path) -> PathBuf {
    data_dir.join("play-reports.json")
}

fn track_id(reference: &str) -> Option<u32> {
    reference.strip_prefix("track/")?.parse().ok()
}

/// `2026-09-30T12:34:56.000Z`, as JavaScript's `toISOString`.
pub fn iso8601(unix_s: i64) -> String {
    let days = unix_s.div_euclid(86_400);
    let secs = unix_s.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

impl Reporter {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remember what `track.resolve` handed out, for the start of the play.
    pub fn resolved(&self, track_id: u32, format_id: i32, blob: Option<String>, duration_s: Option<u64>) {
        let mut inner = self.inner();
        let now = now_unix_s();
        inner.streams.insert(track_id, Stream { format_id, blob, duration_s, resolved_at: now });
        if inner.streams.len() > MAX_STREAMS {
            let oldest = inner.streams.iter().min_by_key(|(_, s)| s.resolved_at).map(|(id, _)| *id);
            if let Some(id) = oldest {
                inner.streams.remove(&id);
            }
        }
    }

    /// Forget everything not yet sent (reporting turned off, signed out).
    pub fn clear(&self, data_dir: Option<&Path>) {
        let mut inner = self.inner();
        inner.playing.clear();
        inner.queue.clear();
        inner.loaded = true;
        if let Some(dir) = data_dir {
            let _ = std::fs::remove_file(queue_path(dir));
        }
    }

    /// `playback.started {ref}`: the start event to send, if this is a
    /// Qobuz track.
    pub fn started(&self, reference: &str, user_id: u64) -> Option<Value> {
        let track_id = track_id(reference)?;
        let mut inner = self.inner();
        let stream = inner.streams.get(&track_id).cloned();
        let now = now_unix_s();
        inner.playing.insert(
            reference.to_string(),
            Play {
                track_id,
                started_at: now,
                duration_s: stream.as_ref().and_then(|s| s.duration_s),
                blob: stream.as_ref().and_then(|s| s.blob.clone()),
            },
        );
        let mut event = json!({"track_id": track_id, "date": now, "user_id": user_id});
        if let Some(s) = stream {
            event["format_id"] = s.format_id.into();
        }
        Some(json!([event]))
    }

    /// `playback.ended {ref, listened_ms}`: queue the end event. Returns
    /// whether one was queued.
    pub fn ended(&self, reference: &str, listened_ms: u64, data_dir: Option<&Path>) -> bool {
        let mut inner = self.inner();
        let Some(play) = inner.playing.remove(reference) else { return false };
        let Some(blob) = play.blob else {
            tracing::debug!("play of track {} not reported: no blob", play.track_id);
            return false;
        };
        let mut listened_s = listened_ms / 1000;
        if let Some(d) = play.duration_s {
            listened_s = listened_s.min(d);
        }
        if listened_s == 0 {
            return false;
        }
        Self::load(&mut inner, data_dir);
        inner.queue.push(json!({
            "blob": blob,
            "track_context_uuid": uuid::Uuid::new_v4().to_string(),
            "start_stream": iso8601(play.started_at),
            "online": true,
            "local": false,
            "duration": listened_s,
        }));
        let excess = inner.queue.len().saturating_sub(MAX_QUEUED);
        inner.queue.drain(..excess);
        Self::save(&inner, data_dir);
        true
    }

    fn load(inner: &mut Inner, data_dir: Option<&Path>) {
        if inner.loaded {
            return;
        }
        inner.loaded = true;
        let Some(dir) = data_dir else { return };
        if let Some(saved) =
            std::fs::read_to_string(queue_path(dir)).ok().and_then(|raw| serde_json::from_str::<Saved>(&raw).ok())
        {
            let mut events = saved.events;
            events.append(&mut inner.queue);
            inner.queue = events;
        }
    }

    fn save(inner: &Inner, data_dir: Option<&Path>) {
        let Some(dir) = data_dir else { return };
        let path = queue_path(dir);
        let result = if inner.queue.is_empty() {
            std::fs::remove_file(&path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
        } else {
            let raw = serde_json::to_string(&Saved { events: inner.queue.clone() }).expect("serialises");
            std::fs::write(&path, raw)
        };
        if let Err(e) = result {
            tracing::warn!("play reports: cannot update {}: {e}", path.display());
        }
    }

    /// Send the queued end events, a batch at a time. Stops at the first
    /// failure; what is left is sent next time.
    pub async fn flush(&self, api: &ApiClient, data_dir: Option<&Path>) {
        loop {
            let batch: Vec<Value> = {
                let mut inner = self.inner();
                Self::load(&mut inner, data_dir);
                inner.queue.iter().take(BATCH).cloned().collect()
            };
            if batch.is_empty() {
                return;
            }
            if let Err(e) = api.report_streaming_end(&Value::Array(batch.clone())).await {
                tracing::info!("play reports: {} kept for later ({e:#})", batch.len());
                return;
            }
            let mut inner = self.inner();
            // Only drop what was sent: `clear` may have run meanwhile.
            if inner.queue.starts_with(&batch) {
                inner.queue.drain(..batch.len());
            }
            Self::save(&inner, data_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_match_javascript() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_790_764_496), "2026-09-30T10:34:56.000Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn a_play_becomes_a_start_then_an_end_event() {
        let dir = std::env::temp_dir().join(format!("qconnect-report-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = Reporter::default();
        r.resolved(77, 27, Some("b10b".into()), Some(185));
        let start = r.started("track/77", 9).unwrap();
        assert_eq!(start[0]["track_id"], 77);
        assert_eq!(start[0]["user_id"], 9);
        assert_eq!(start[0]["format_id"], 27);
        assert!(r.started("album/abc", 9).is_none(), "only tracks");

        assert!(r.ended("track/77", 999_999, Some(&dir)), "queued");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("play-reports.json")).unwrap()).unwrap();
        let event = &saved["events"][0];
        assert_eq!(event["blob"], "b10b");
        assert_eq!(event["duration"], 185, "capped at the track's length");
        assert_eq!(event["local"], false);
        assert!(event["start_stream"].as_str().unwrap().ends_with(".000Z"));

        // No blob, or nothing listened: nothing to report.
        r.resolved(78, 6, None, None);
        r.started("track/78", 9);
        assert!(!r.ended("track/78", 30_000, Some(&dir)));
        r.started("track/77", 9);
        assert!(!r.ended("track/77", 400, Some(&dir)));

        // A new reporter picks the saved queue up.
        let again = Reporter::default();
        again.resolved(77, 27, Some("b2".into()), None);
        again.started("track/77", 9);
        assert!(again.ended("track/77", 10_000, Some(&dir)));
        assert_eq!(again.inner().queue.len(), 2);
        again.clear(Some(&dir));
        assert!(!dir.join("play-reports.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
