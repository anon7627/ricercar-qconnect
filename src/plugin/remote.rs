//! Qobuz Connect → host player: the output behind the Connect session.
//!
//! Session commands become plugin → host `player.*` requests; the host's
//! `player.state` and `player.taken_over` notifications become player events.
//! The host plays `track/<id>` items through `track.resolve`, like any other
//! plugin track.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

use super::items;
use super::resolve::Resolved;
use super::rpc::Out;
use crate::api::ApiClient;
use crate::player::{Backend, PlayerCmd, PlayerEvent};

/// Formats of the last resolved streams, by track id.
pub type Formats = Arc<Mutex<HashMap<u32, Resolved>>>;

/// A `player.taken_over` this soon after our own `player.play` is the host
/// switching queues for us, not the user taking over.
const TAKEOVER_GRACE: Duration = Duration::from_secs(3);
/// A stop this close to the end of the track is the end of the queue.
const END_MARGIN_MS: u64 = 5_000;

#[derive(Debug)]
pub enum HostEvent {
    State(HostState),
    TakenOver,
}

/// `player.state` notification.
#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
pub struct HostState {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub item_ref: Option<String>,
    #[serde(default)]
    pub pos_ms: u64,
    #[serde(default)]
    pub dur_ms: u64,
}

/// The track handed to the host.
struct Loaded {
    load_id: u64,
    reference: String,
    /// The host has started playing it.
    started: bool,
    /// Applied once it has started.
    seek_ms: Option<u64>,
    pause: bool,
    status: String,
    requested_at: Instant,
}

struct Remote {
    backend: Backend,
    out: Out,
    api: ApiClient,
    formats: Formats,
    current: Option<Loaded>,
    /// (load id, ref) enqueued after the current track.
    preload: Option<(u64, String)>,
    last: HostState,
}

pub async fn run(backend: Backend, mut events: UnboundedReceiver<HostEvent>, out: Out, api: ApiClient, formats: Formats) {
    let mut r = Remote { backend, out, api, formats, current: None, preload: None, last: HostState::default() };
    loop {
        tokio::select! {
            cmd = r.backend.cmd_rx.recv() => match cmd {
                Some(cmd) => r.on_cmd(cmd).await,
                None => break,
            },
            Some(ev) = events.recv() => r.on_event(ev).await,
        }
    }
}

fn track_id(reference: &str) -> Option<u32> {
    reference.strip_prefix("track/")?.parse().ok()
}

impl Remote {
    fn emit(&self, ev: PlayerEvent) {
        tracing::debug!("remote: {ev:?}");
        let _ = self.backend.event_tx.send(ev);
    }

    async fn request(&self, method: &str, params: Value) -> bool {
        match self.out.request(method, params).await {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("{method} refused by the host: {} ({})", e.message, e.code);
                false
            }
        }
    }

    /// Protocol item for a track, with its metadata when the API answers.
    async fn item(&self, track_id: u32) -> Value {
        let id = track_id.to_string();
        match self.api.track_get(&id).await.ok().and_then(|v| items::track(&v, None)) {
            Some(item) => item.to_json(),
            None => json!({"ref": format!("track/{id}"), "kind": "track", "title": format!("Qobuz {id}"),
                           "playable": true, "browsable": false}),
        }
    }

    fn started_event(&self, load_id: u64, reference: &str) -> PlayerEvent {
        let f = track_id(reference).and_then(|id| self.formats.lock().unwrap_or_else(|e| e.into_inner()).get(&id).copied());
        PlayerEvent::Started {
            load_id,
            sample_rate: f.map_or(0, |f| f.sample_rate),
            bit_depth: f.map_or(0, |f| f.bit_depth),
            channels: 2,
            format_id: f.map_or(0, |f| f.format_id),
        }
    }

    fn mirror(&self, st: &HostState) {
        let shared = &self.backend.shared;
        if st.dur_ms > 0 {
            shared.set_duration(st.dur_ms);
        }
        shared.set_position(st.pos_ms);
        shared.set_playing(st.status == "playing");
    }

    async fn on_cmd(&mut self, cmd: PlayerCmd) {
        let shared = self.backend.shared.clone();
        match cmd {
            PlayerCmd::Load { load_id, track_id, position_ms, paused } => {
                let item = self.item(track_id).await;
                self.preload = None;
                self.current = Some(Loaded {
                    load_id,
                    reference: format!("track/{track_id}"),
                    started: false,
                    seek_ms: (position_ms > 0).then_some(position_ms),
                    pause: paused,
                    status: String::new(),
                    requested_at: Instant::now(),
                });
                shared.set_playing(false);
                shared.set_buffering(true);
                shared.set_position(position_ms);
                shared.set_duration(item.get("duration_ms").and_then(Value::as_u64).unwrap_or(0));
                if let Err(e) = self.out.request("player.play", json!({"items": [item], "start": 0})).await {
                    self.current = None;
                    shared.set_buffering(false);
                    self.emit(PlayerEvent::Error { load_id, message: format!("player.play: {}", e.message) });
                }
            }
            PlayerCmd::Pause => match self.current.as_mut() {
                Some(c) if !c.started => c.pause = true,
                Some(_) => {
                    shared.set_playing(false);
                    self.request("player.pause", json!({})).await;
                }
                None => {}
            },
            PlayerCmd::Resume => match self.current.as_mut() {
                Some(c) if !c.started => c.pause = false,
                Some(_) => {
                    self.request("player.resume", json!({})).await;
                }
                None => {}
            },
            PlayerCmd::Stop => {
                self.preload = None;
                shared.set_playing(false);
                shared.set_buffering(false);
                // Nothing of ours is playing: leave the user's playback alone.
                if self.current.take().is_some() {
                    self.request("player.stop", json!({})).await;
                }
            }
            PlayerCmd::Seek { position_ms } => match self.current.as_mut() {
                Some(c) if !c.started => c.seek_ms = Some(position_ms),
                Some(_) => {
                    shared.set_position(position_ms);
                    self.request("player.seek", json!({"ms": position_ms})).await;
                }
                None => {}
            },
            PlayerCmd::SetVolume { percent, muted } => {
                if let Some(percent) = percent {
                    self.request("player.set_volume", json!({"percent": percent})).await;
                }
                self.request("player.set_mute", json!({"on": muted})).await;
            }
            PlayerCmd::Preload(Some(req)) => {
                // The host cannot withdraw an enqueued item; a stale one is
                // caught when the host moves on to it (see `on_state`).
                let item = self.item(req.track_id).await;
                if self.request("player.enqueue", json!({"items": [item], "at": "next"})).await {
                    self.preload = Some((req.load_id, format!("track/{}", req.track_id)));
                }
            }
            PlayerCmd::Preload(None) => self.preload = None,
        }
    }

    async fn on_event(&mut self, ev: HostEvent) {
        match ev {
            HostEvent::TakenOver => {
                let Some(c) = &self.current else { return };
                if !c.started && c.requested_at.elapsed() < TAKEOVER_GRACE {
                    tracing::debug!("remote: taken_over right after our own play; ignored");
                    return;
                }
                let load_id = c.load_id;
                tracing::info!("the host is playing something else; leaving Qobuz Connect playback");
                self.current = None;
                self.preload = None;
                self.backend.shared.set_playing(false);
                self.emit(PlayerEvent::Stopped { load_id });
            }
            HostEvent::State(st) => self.on_state(st).await,
        }
    }

    async fn on_state(&mut self, st: HostState) {
        let prev = std::mem::replace(&mut self.last, st.clone());
        let Some(cur) = self.current.as_mut() else { return };
        let ours = st.item_ref.as_deref() == Some(cur.reference.as_str());

        if !cur.started {
            if !ours || st.status == "stopped" {
                return; // still loading
            }
            cur.started = true;
            cur.status = st.status.clone();
            let (load_id, reference, seek, pause) = (cur.load_id, cur.reference.clone(), cur.seek_ms.take(), cur.pause);
            self.backend.shared.set_buffering(false);
            self.mirror(&st);
            if let Some(ms) = seek {
                self.backend.shared.set_position(ms);
                self.request("player.seek", json!({"ms": ms})).await;
            }
            if pause && st.status == "playing" {
                self.backend.shared.set_playing(false);
                self.request("player.pause", json!({})).await;
            }
            self.emit(self.started_event(load_id, &reference));
            return;
        }

        if ours {
            let changed = st.status != cur.status;
            cur.status = st.status.clone();
            let load_id = cur.load_id;
            self.mirror(&st);
            if !changed {
                return;
            }
            match st.status.as_str() {
                "playing" => self.emit(PlayerEvent::Resumed { load_id }),
                "paused" => self.emit(PlayerEvent::Paused { load_id }),
                _ => {
                    self.current = None;
                    let ended = prev.dur_ms > 0 && prev.pos_ms + END_MARGIN_MS >= prev.dur_ms;
                    self.emit(if ended { PlayerEvent::Ended { load_id } } else { PlayerEvent::Stopped { load_id } });
                }
            }
            return;
        }

        // The host moved on to another item.
        let load_id = cur.load_id;
        match self.preload.take() {
            Some((next_id, next_ref)) if st.item_ref.as_deref() == Some(next_ref.as_str()) => {
                self.current = Some(Loaded {
                    load_id: next_id,
                    reference: next_ref.clone(),
                    started: true,
                    seek_ms: None,
                    pause: false,
                    status: st.status.clone(),
                    requested_at: Instant::now(),
                });
                self.mirror(&st);
                self.emit(PlayerEvent::Advanced { load_id: next_id });
                self.emit(self.started_event(next_id, &next_ref));
            }
            other => {
                // A stale enqueued item, or nothing: the session picks the
                // next track itself.
                self.preload = other;
                self.current = None;
                self.backend.shared.set_playing(false);
                self.emit(PlayerEvent::Ended { load_id });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::{Player, PreloadReq};

    struct Rig {
        player: Player,
        events: tokio::sync::mpsc::UnboundedSender<HostEvent>,
        sent: tokio::sync::mpsc::UnboundedReceiver<Value>,
        out: Out,
    }

    /// Remote task with a fake host that answers every request with `null`.
    fn rig() -> Rig {
        let (player, backend) = Player::channels();
        let (events, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out, sent) = Out::capture();
        // Unreachable API: items fall back to bare refs.
        let api = ApiClient::with_base("http://127.0.0.1:9");
        tokio::spawn(run(backend, events_rx, out.clone(), api, Formats::default()));
        Rig { player, events, sent, out }
    }

    impl Rig {
        /// Next request sent to the host, answered with `null`.
        async fn host_gets(&mut self) -> (String, Value) {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.sent.recv()).await.unwrap().unwrap();
            self.out.deliver(&msg["id"], Ok(Value::Null));
            (msg["method"].as_str().unwrap().to_string(), msg["params"].clone())
        }

        fn state(&self, status: &str, item_ref: Option<&str>, pos_ms: u64) {
            let st = HostState { status: status.into(), item_ref: item_ref.map(str::to_string), pos_ms, dur_ms: 180_000 };
            self.events.send(HostEvent::State(st)).unwrap();
        }

        async fn event(&mut self) -> PlayerEvent {
            tokio::time::timeout(Duration::from_secs(5), self.player.event_rx.recv()).await.unwrap().unwrap()
        }
    }

    fn load(load_id: u64, track_id: u32, position_ms: u64, paused: bool) -> PlayerCmd {
        PlayerCmd::Load { load_id, track_id, position_ms, paused }
    }

    #[tokio::test]
    async fn load_plays_then_seeks_and_pauses_once_started() {
        let mut r = rig();
        r.player.send(load(1, 77, 30_000, true));
        let (method, params) = r.host_gets().await;
        assert_eq!(method, "player.play");
        assert_eq!(params["items"][0]["ref"], "track/77");

        r.state("stopped", None, 0); // host still resolving: ignored
        r.events.send(HostEvent::TakenOver).unwrap(); // queue switch: ignored
        r.state("playing", Some("track/77"), 0);
        assert_eq!(r.host_gets().await, ("player.seek".into(), json!({"ms": 30_000})));
        assert_eq!(r.host_gets().await.0, "player.pause");
        assert!(matches!(r.event().await, PlayerEvent::Started { load_id: 1, .. }));

        r.state("paused", Some("track/77"), 30_000);
        assert!(matches!(r.event().await, PlayerEvent::Paused { load_id: 1 }));
        assert_eq!(r.player.shared.position_ms(), 30_000);
    }

    #[tokio::test]
    async fn gapless_advance_and_end_of_queue() {
        let mut r = rig();
        r.player.send(load(1, 77, 0, false));
        r.host_gets().await;
        r.state("playing", Some("track/77"), 0);
        assert!(matches!(r.event().await, PlayerEvent::Started { load_id: 1, .. }));

        r.player.send(PlayerCmd::Preload(Some(PreloadReq { load_id: 2, track_id: 78 })));
        let (method, params) = r.host_gets().await;
        assert_eq!((method.as_str(), &params["at"]), ("player.enqueue", &json!("next")));

        r.state("playing", Some("track/78"), 1_000);
        assert!(matches!(r.event().await, PlayerEvent::Advanced { load_id: 2 }));
        assert!(matches!(r.event().await, PlayerEvent::Started { load_id: 2, .. }));

        r.state("playing", Some("track/78"), 178_000);
        r.state("stopped", Some("track/78"), 0);
        assert!(matches!(r.event().await, PlayerEvent::Ended { load_id: 2 }));
    }

    #[tokio::test]
    async fn user_takeover_and_stale_items() {
        let mut r = rig();
        r.player.send(load(1, 77, 0, false));
        r.host_gets().await;
        r.state("playing", Some("track/77"), 0);
        r.event().await;
        // Moved to an item we did not preload: let the session pick.
        r.state("playing", Some("track/99"), 0);
        assert!(matches!(r.event().await, PlayerEvent::Ended { load_id: 1 }));

        r.player.send(load(3, 80, 0, false));
        r.host_gets().await;
        r.state("playing", Some("track/80"), 0);
        r.event().await;
        r.events.send(HostEvent::TakenOver).unwrap();
        assert!(matches!(r.event().await, PlayerEvent::Stopped { load_id: 3 }));

        // Nothing of ours playing: a stop must not reach the host.
        r.player.send(PlayerCmd::Stop);
        r.player.send(PlayerCmd::SetVolume { percent: Some(40), muted: false });
        assert_eq!(r.host_gets().await, ("player.set_volume".into(), json!({"percent": 40})));
    }
}
