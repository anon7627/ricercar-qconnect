//! `track.resolve`: a stream URL in a format the output plays natively.

use serde::Deserialize;
use serde_json::{json, Value};

use super::rpc::{RpcError, UNAVAILABLE};
use crate::api::{now_unix_s, ApiClient, HttpError, StreamUrl};
use crate::config::Quality;

/// Lifetime assumed for a stream URL that carries no expiry of its own.
const DEFAULT_TTL_S: i64 = 10 * 60;
/// Margin taken off an announced expiry.
const EXPIRY_MARGIN_S: i64 = 60;

/// What the host's current output accepts natively (`initialize.output`,
/// `output.changed`).
#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Output {
    #[serde(default)]
    pub max_rate: Option<u32>,
    #[serde(default)]
    pub max_bits: Option<u32>,
    #[serde(default)]
    pub rates: Vec<u32>,
}

impl Output {
    /// Best quality the DAC plays without conversion. Unknown output: no cap.
    pub fn max_quality(&self) -> Quality {
        let rate = self.max_rate.or_else(|| self.rates.iter().copied().max());
        let Some(rate) = rate else {
            return Quality::HiRes192;
        };
        if self.max_bits.is_some_and(|b| b < 24) {
            return Quality::Lossless;
        }
        match rate {
            176_400.. => Quality::HiRes192,
            88_200.. => Quality::HiRes96,
            _ => Quality::Lossless,
        }
    }

    /// Whether a stream of this format plays natively (0 = not announced).
    pub fn accepts(&self, rate: u32, bits: u32) -> bool {
        let rate_ok = rate == 0
            || if self.rates.is_empty() { self.max_rate.is_none_or(|m| rate <= m) } else { self.rates.contains(&rate) };
        let bits_ok = bits == 0 || self.max_bits.is_none_or(|m| bits <= m);
        rate_ok && bits_ok
    }
}

fn unavailable(message: impl Into<String>) -> RpcError {
    RpcError::new(UNAVAILABLE, message)
}

/// Stream URL for the best quality `output` accepts, stepping down while the
/// API hands out a format the DAC would have to convert.
async fn stream_for(api: &ApiClient, track_id: u32, output: &Output) -> Result<StreamUrl, RpcError> {
    let mut api = api.clone();
    let mut quality = output.max_quality();
    loop {
        let s = api.get_stream_url(track_id, quality).await?;
        if s.sample {
            return Err(unavailable("only a preview is available with this subscription"));
        }
        if output.accepts(s.sample_rate, s.bit_depth) {
            return Ok(s);
        }
        tracing::debug!("track {track_id}: {} Hz / {} bits not accepted by the output", s.sample_rate, s.bit_depth);
        let got = Quality::from_format_id(s.format_id).unwrap_or(quality).min(quality);
        quality = got
            .fallback_chain()
            .nth(1)
            .ok_or_else(|| unavailable(format!("no format of track {track_id} fits the output")))?;
    }
}

fn codec(mime: &str) -> &str {
    match mime {
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/mpeg" => "mp3",
        other => other.rsplit('/').next().unwrap_or(other),
    }
}

/// Stream URLs carry their expiry in `etsp` (unix seconds).
pub fn expires_at(url: &str, now: i64) -> i64 {
    let etsp = url
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("etsp=")))
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&t| t > now);
    match etsp {
        Some(t) => (t - EXPIRY_MARGIN_S).max(now + 1),
        None => now + DEFAULT_TTL_S,
    }
}

/// Format of a resolved stream, remembered for Qobuz Connect reports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resolved {
    pub track_id: u32,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub format_id: i32,
}

/// What play reports need from a resolution.
pub struct ForReport {
    pub blob: Option<String>,
    pub duration_s: Option<u64>,
}

pub async fn resolve(api: &ApiClient, reference: &str, output: &Output) -> Result<(Value, Resolved, ForReport), RpcError> {
    let id = reference
        .strip_prefix("track/")
        .and_then(|id| id.parse::<u32>().ok())
        .ok_or_else(|| RpcError::not_found(format!("{reference} is not a track")))?;
    let id_s = id.to_string();
    let (stream, track) = tokio::join!(stream_for(api, id, output), api.track_get(&id_s));
    let stream = stream.map_err(|mut e| {
        // A track still in the user's lists but gone from the catalogue:
        // `track/get` knows it no more, and Qobuz refuses to stream it.
        let gone = track.as_ref().is_err_and(|t| t.chain().any(|c| c.downcast_ref::<HttpError>().is_some_and(|h| h.status == 404)));
        if gone && e.code == UNAVAILABLE {
            e.message = format!("track {id} is no longer in the Qobuz catalogue ({})", e.message);
        }
        e
    })?;
    let resolved = Resolved {
        track_id: id,
        sample_rate: stream.sample_rate,
        bit_depth: stream.bit_depth,
        format_id: stream.format_id,
    };
    let track = track.map_err(|e| tracing::debug!("track/get {id}: {e:#}")).ok();
    let report = ForReport {
        blob: stream.blob.clone(),
        duration_s: track.as_ref().and_then(|t| t.get("duration")).and_then(Value::as_u64),
    };

    let mut out = json!({
        "url": stream.url,
        "expires_at": expires_at(&stream.url, now_unix_s()),
        "format": {
            "sample_rate": stream.sample_rate,
            "bits": stream.bit_depth,
            "channels": 2,
            "codec": codec(&stream.mime_type),
        },
        "live": false,
    });
    if let Some(track) = &track {
        if let Some(secs) = track.get("duration").and_then(Value::as_u64) {
            out["duration_ms"] = (secs * 1000).into();
        }
        let gain = track.pointer("/audio_info/replaygain_track_gain").and_then(Value::as_f64);
        let peak = track.pointer("/audio_info/replaygain_track_peak").and_then(Value::as_f64);
        if let (Some(gain), Some(peak)) = (gain, peak) {
            out["replaygain"] = json!({"track_gain": gain, "track_peak": peak});
        }
    }
    Ok((out, resolved, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(max_rate: u32, max_bits: u32, rates: &[u32]) -> Output {
        Output { max_rate: Some(max_rate), max_bits: Some(max_bits), rates: rates.to_vec() }
    }

    #[test]
    fn quality_follows_the_dac() {
        assert_eq!(Output::default().max_quality(), Quality::HiRes192);
        assert_eq!(out(192_000, 24, &[]).max_quality(), Quality::HiRes192);
        assert_eq!(out(176_400, 32, &[]).max_quality(), Quality::HiRes192);
        assert_eq!(out(96_000, 24, &[]).max_quality(), Quality::HiRes96);
        assert_eq!(out(88_200, 24, &[]).max_quality(), Quality::HiRes96);
        assert_eq!(out(48_000, 24, &[]).max_quality(), Quality::Lossless);
        assert_eq!(out(192_000, 16, &[]).max_quality(), Quality::Lossless);
        let rates_only = Output { rates: vec![44_100, 96_000], ..Default::default() };
        assert_eq!(rates_only.max_quality(), Quality::HiRes96);
    }

    #[test]
    fn accepted_formats() {
        let dac = out(192_000, 24, &[44_100, 48_000, 96_000, 192_000]);
        assert!(dac.accepts(96_000, 24));
        assert!(!dac.accepts(88_200, 24), "rate missing from the list");
        assert!(!dac.accepts(96_000, 32));
        assert!(dac.accepts(0, 0));
        assert!(out(96_000, 24, &[]).accepts(88_200, 24));
        assert!(!out(96_000, 24, &[]).accepts(192_000, 24));
    }

    #[test]
    fn expiry_comes_from_the_url_when_present() {
        let now = 1_000_000;
        assert_eq!(expires_at("https://cdn/file?uid=1&etsp=1000600&hmac=x", now), 1_000_540);
        assert_eq!(expires_at("https://cdn/file?etsp=999", now), now + DEFAULT_TTL_S);
        assert_eq!(expires_at("https://cdn/file.flac", now), now + DEFAULT_TTL_S);
    }
}
