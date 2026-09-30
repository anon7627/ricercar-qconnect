//! Local relay for CMAF streams: serves each registered track as one FLAC
//! file on `http://127.0.0.1:<port>/<token>/<track>.flac`.
//!
//! - `Content-Length` is known from the init segment (FLAC header + every
//!   segment's audio bytes), so the host sees an ordinary file.
//! - `Range: bytes=a-b` is honoured (`206`): only the segments covering the
//!   range are fetched, then decrypted; the last few are kept.
//! - Tokens are random and forgotten when the stream URL expires; an unknown
//!   token answers `410 Gone`, and the host resolves the track again.
//!
//! The FLAC bytes are the original ones: nothing is decoded or re-encoded.
//! Segment URLs carry the account's id: they are never logged.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, ensure, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::stream;
use tokio::sync::OnceCell;

use crate::api::now_unix_s;
use crate::cmaf::{self, Init};

/// Decrypted segments kept per stream.
const CACHED_SEGMENTS: usize = 3;
/// Streams registered at once; the oldest go first.
const MAX_STREAMS: usize = 32;

/// A registered CMAF stream.
pub struct Stream {
    pub track_id: u32,
    /// `file/url`'s `url_template`, with `$SEGMENT$`.
    pub template: String,
    pub key: [u8; 16],
    pub init: Init,
    pub expires_at: i64,
}

struct Entry {
    stream: Stream,
    offsets: Vec<u64>,
    size: u64,
    registered: i64,
    cache: Mutex<VecDeque<(usize, Bytes)>>,
}

#[derive(Default)]
struct Shared {
    streams: Mutex<HashMap<String, Arc<Entry>>>,
    http: reqwest::Client,
}

/// The relay; its HTTP server starts with the first stream.
#[derive(Default, Clone)]
pub struct Relay {
    shared: Arc<Shared>,
    port: Arc<OnceCell<u16>>,
}

impl Relay {
    /// Register `stream`; returns its URL.
    pub async fn register(&self, stream: Stream) -> Result<String> {
        let port = self.port.get_or_try_init(|| self.start()).await?;
        let offsets = stream.init.offsets();
        let size = stream.init.file_size();
        let track_id = stream.track_id;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let entry = Arc::new(Entry { stream, offsets, size, registered: now_unix_s(), cache: Mutex::default() });
        let mut streams = self.shared.streams.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_unix_s();
        streams.retain(|_, e| e.stream.expires_at > now);
        while streams.len() >= MAX_STREAMS {
            let oldest = streams.iter().min_by_key(|(_, e)| e.registered).map(|(t, _)| t.clone());
            if let Some(t) = oldest {
                streams.remove(&t);
            }
        }
        streams.insert(token.clone(), entry);
        Ok(format!("http://127.0.0.1:{port}/{token}/{track_id}.flac"))
    }

    async fn start(&self) -> Result<u16> {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.context("relay: bind")?;
        let port = listener.local_addr()?.port();
        let app = Router::new().route("/{token}/{file}", get(serve).head(serve)).with_state(self.shared.clone());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!("relay stopped: {e}");
            }
        });
        tracing::info!("CMAF relay listening on 127.0.0.1:{port}");
        Ok(port)
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against a file of `size` bytes:
/// the inclusive range, `Err` when it cannot be satisfied, `None` for the
/// whole file (no header, or one that is not a single byte range).
fn byte_range(headers: &HeaderMap, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = headers.get(header::RANGE)?.to_str().ok()?.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let range = match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(a), Some(b)) if a <= b => (a, b.min(size.saturating_sub(1))),
        (Some(a), None) if b.is_empty() => (a, size.saturating_sub(1)),
        (None, Some(n)) if a.is_empty() && n > 0 => (size.saturating_sub(n), size.saturating_sub(1)),
        _ => return None,
    };
    Some(if size == 0 || range.0 >= size || range.0 > range.1 { Err(()) } else { Ok(range) })
}

async fn serve(
    State(shared): State<Arc<Shared>>,
    Path((token, _file)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let entry = {
        let streams = shared.streams.lock().unwrap_or_else(|e| e.into_inner());
        streams.get(&token).cloned()
    };
    let Some(entry) = entry.filter(|e| e.stream.expires_at > now_unix_s()) else {
        return (StatusCode::GONE, "stream expired").into_response();
    };
    let size = entry.size;
    let (status, start, end) = match byte_range(&headers, size) {
        None => (StatusCode::OK, 0, size.saturating_sub(1)),
        Some(Ok((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Some(Err(())) => {
            return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{size}"))]).into_response();
        }
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "audio/flac")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        let chunks = stream::unfold((shared, entry, start, end + 1), |(shared, entry, pos, stop)| async move {
            if pos >= stop {
                return None;
            }
            match chunk(&shared, &entry, pos, stop).await {
                Ok(bytes) => {
                    let next = pos + bytes.len() as u64;
                    Some((Ok::<_, std::io::Error>(bytes), (shared, entry, next, stop)))
                }
                Err(e) => {
                    tracing::warn!("relay: track {}: {e:#}", entry.stream.track_id);
                    Some((Err(std::io::Error::other(e.to_string())), (shared, entry, stop, stop)))
                }
            }
        });
        Body::from_stream(chunks)
    };
    builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The bytes from `pos` to the end of whatever part holds it (the FLAC
/// header, or one segment), without going past `stop`.
async fn chunk(shared: &Shared, entry: &Entry, pos: u64, stop: u64) -> Result<Bytes> {
    let header = &entry.stream.init.flac_header;
    let header_len = header.len() as u64;
    if pos < header_len {
        let end = stop.min(header_len);
        return Ok(Bytes::copy_from_slice(&header[pos as usize..end as usize]));
    }
    // Last segment starting at or before `pos`.
    let index = entry.offsets.partition_point(|&o| o <= pos).checked_sub(1).ok_or_else(|| anyhow!("offset {pos} before the audio"))?;
    let audio = segment(shared, entry, index).await?;
    let seg_start = entry.offsets[index];
    let from = (pos - seg_start) as usize;
    let to = ((stop - seg_start) as usize).min(audio.len());
    ensure!(from < to, "offset {pos} past segment {}", index + 1);
    Ok(audio.slice(from..to))
}

/// Audio bytes of segment `index` (0-based; the URL counts from 1).
async fn segment(shared: &Shared, entry: &Entry, index: usize) -> Result<Bytes> {
    if let Some(hit) = entry.cache.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(i, _)| *i == index) {
        return Ok(hit.1.clone());
    }
    let url = entry.stream.template.replace("$SEGMENT$", &(index + 1).to_string());
    let resp = shared.http.get(&url).send().await.context("segment request")?;
    ensure!(resp.status().is_success(), "segment {}: HTTP {}", index + 1, resp.status().as_u16());
    let raw = resp.bytes().await.context("segment body")?;
    let key = entry.stream.key;
    let audio = tokio::task::spawn_blocking(move || cmaf::decrypt_segment(&raw, &key)).await??;
    let expected = entry.stream.init.segments[index].audio_bytes as usize;
    ensure!(audio.len() == expected, "segment {}: {} audio bytes, the init said {expected}", index + 1, audio.len());
    let audio = Bytes::from(audio);
    let mut cache = entry.cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.push_back((index, audio.clone()));
    while cache.len() > CACHED_SEGMENTS {
        cache.pop_front();
    }
    Ok(audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(h: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, h.parse().unwrap());
        byte_range(&headers, size)
    }

    #[test]
    fn ranges() {
        assert_eq!(byte_range(&HeaderMap::new(), 100), None);
        assert_eq!(range("bytes=0-9", 100), Some(Ok((0, 9))));
        assert_eq!(range("bytes=90-", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=90-500", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=100-", 100), Some(Err(())));
        assert_eq!(range("bytes=0-1,5-6", 100), None, "several ranges: the whole file");
        assert_eq!(range("items=0-1", 100), None);
    }
}
