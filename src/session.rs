//! Qobuz Connect renderer session: ties the cloud WebSocket (joined with the
//! signed-in account) and the player together.
//!
//! The server drives playback with `SrvrRndrSetState` (current/next queue
//! item + playing state). The renderer plays the current item, advances to
//! the next one on its own at end of track, and reports its state back with
//! `RndrSrvrStateUpdated`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::api::{now_ms, ApiClient};
use crate::config::{DeviceIdentity, Quality};
use crate::msgtype::{buffer_state, loop_mode, msg, playing_state};
use crate::player::{Player, PlayerCmd, PlayerEvent, PreloadReq};
use crate::proto::{
    Position, QConnectBatch, QConnectMessage, QueueTrackRef, QueueVersion, RendererState,
    RndrSrvrDeviceAudioQualityChanged, RndrSrvrFileAudioQualityChanged,
    RndrSrvrMaxAudioQualityChanged, RndrSrvrStateUpdated, RndrSrvrVolumeChanged,
    RndrSrvrVolumeMuted, SrvrRndrSetState,
};
use crate::ws::{run_ws, WsEvent, WsParams};

/// How often to report position while playing.
const HEARTBEAT: Duration = Duration::from_secs(5);
/// Position differences below this are treated as "no seek requested".
const SEEK_TOLERANCE_MS: u64 = 1_500;

struct WsConn {
    task: JoinHandle<()>,
    batch_tx: UnboundedSender<QConnectBatch>,
}

/// How the device presents itself in the Qobuz app.
#[derive(Clone, Debug)]
pub struct SessionParams {
    pub name: String,
    pub brand: String,
    pub model: String,
    /// Highest quality to offer (what the output plays natively).
    pub quality: Quality,
    /// Controllers may not change the volume.
    pub fixed_volume: bool,
    pub identity: DeviceIdentity,
}

pub struct Session {
    cfg: SessionParams,
    api: Arc<Mutex<ApiClient>>,
    player: Player,

    ws: Option<WsConn>,
    ws_connected: bool,
    /// Ready to report state: the server announced us as a renderer.
    registered: bool,
    /// Our renderer id in the account's session.
    renderer_id: Option<i32>,
    ws_event_tx: UnboundedSender<WsEvent>,
    batch_counter: i32,

    playing_state: i32,
    error: bool,
    queue_version: Option<QueueVersion>,
    current: Option<QueueTrackRef>,
    next: Option<QueueTrackRef>,
    /// Queue item currently handed to the player.
    loaded: Option<i32>,
    /// Id of the current load; player events for other ids are stale.
    load_id: u64,
    last_id: u64,
    /// (load id, queue item id) of the track preloaded for gapless playback.
    preloaded: Option<(u64, i32)>,
    loop_mode: i32,
    volume: u32,
    muted: bool,
    max_quality: Quality,
}

/// Run until the task is aborted. `api` must hold the user token.
pub async fn run(cfg: SessionParams, api: Arc<Mutex<ApiClient>>, player: Player) {
    let (ws_event_tx, mut ws_event_rx) = unbounded_channel();

    let mut session = Session {
        max_quality: cfg.quality,
        cfg,
        api,
        player,
        ws: None,
        ws_connected: false,
        registered: false,
        renderer_id: None,
        ws_event_tx,
        batch_counter: 0,
        playing_state: playing_state::STOPPED,
        error: false,
        queue_version: None,
        current: None,
        next: None,
        loaded: None,
        load_id: 0,
        last_id: 0,
        preloaded: None,
        loop_mode: loop_mode::OFF,
        volume: 100,
        muted: false,
    };
    tracing::info!("joining Qobuz Connect as {:?}", session.cfg.name);
    session.start_ws();

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Stop the output and the WebSocket when the task is aborted.
    let _guard = StopOnDrop(session.player.sender());
    loop {
        tokio::select! {
            Some(ev) = ws_event_rx.recv() => session.on_ws_event(ev),
            ev = session.player.event_rx.recv() => match ev {
                Some(ev) => session.on_player_event(ev),
                None => break,
            },
            _ = heartbeat.tick() => {
                if session.playing_state == playing_state::PLAYING {
                    session.send_state();
                }
            }
        }
    }
    tracing::info!("player gone; leaving Qobuz Connect");
}

struct StopOnDrop(tokio::sync::mpsc::UnboundedSender<PlayerCmd>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(PlayerCmd::Stop);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(ws) = self.ws.take() {
            ws.task.abort();
        }
    }
}

impl Session {
    /// Connect to Qobuz Connect.
    fn start_ws(&mut self) {
        let params = WsParams {
            device: self.cfg.identity.clone(),
            device_name: self.cfg.name.clone(),
            brand: self.cfg.brand.clone(),
            model: self.cfg.model.clone(),
            max_quality: self.max_quality,
            fixed_volume: self.cfg.fixed_volume,
        };
        let (batch_tx, batch_rx) = unbounded_channel();
        let task = tokio::spawn(run_ws(params, self.api.clone(), self.ws_event_tx.clone(), batch_rx));
        self.ws = Some(WsConn { task, batch_tx });
    }

    // ---------- cloud ----------

    fn on_ws_event(&mut self, ev: WsEvent) {
        match ev {
            // Reports wait until the server has registered us as a renderer.
            WsEvent::Connected => self.ws_connected = true,
            WsEvent::Disconnected => {
                self.ws_connected = false;
                self.registered = false;
            }
            WsEvent::Batch(batch) => {
                for m in batch.messages {
                    self.on_message(m);
                }
            }
        }
    }

    fn on_registered(&mut self) {
        self.registered = true;
        self.send_state();
        self.send_volume();
        self.send_max_quality();
    }

    fn on_message(&mut self, m: QConnectMessage) {
        tracing::debug!("<- message type {:?}", m.message_type);
        // The server announces every renderer of the session, us included
        // (matched by device uuid).
        if let Some(r) = &m.srvr_ctrl_add_renderer {
            let ours = r.device_info.as_ref().and_then(|d| d.device_uuid.as_deref())
                == Some(&self.cfg.identity.uuid_bytes()[..]);
            if ours && self.renderer_id.is_none() {
                self.renderer_id = r.renderer_id;
                tracing::info!(
                    "registered with Qobuz Connect as renderer {:?}; select {:?} in the Qobuz app",
                    r.renderer_id,
                    self.cfg.name
                );
                self.on_registered();
            }
        }
        if let Some(st) = &m.srvr_ctrl_session_state {
            if let Some(uuid) = st.session_uuid.as_deref().and_then(|u| uuid::Uuid::from_slice(u).ok()) {
                tracing::debug!("session {uuid}, active renderer {:?}", st.active_renderer_id);
            }
        }
        if let Some(c) = &m.srvr_ctrl_active_renderer_changed {
            if self.renderer_id.is_some() && c.active_renderer_id != self.renderer_id && self.loaded.is_some() {
                tracing::info!("another device was selected in the app; stopping");
                self.stop_playback();
                self.send_state();
            }
        }
        if let Some(s) = m.srvr_rndr_set_state {
            self.on_set_state(s);
        }
        if let Some(v) = m.srvr_rndr_set_volume {
            let target = match (v.volume, v.volume_delta) {
                (Some(abs), _) => abs as i64,
                (None, Some(delta)) => self.volume as i64 + delta as i64,
                (None, None) => self.volume as i64,
            };
            self.volume = target.clamp(0, 100) as u32;
            self.apply_volume();
            self.send_volume();
        }
        if let Some(m) = m.srvr_rndr_mute_volume {
            self.muted = m.value.unwrap_or(false);
            self.apply_volume();
            self.send(vec![QConnectMessage {
                message_type: Some(msg::RNDR_SRVR_VOLUME_MUTED),
                rndr_srvr_volume_muted: Some(RndrSrvrVolumeMuted { value: Some(self.muted) }),
                ..Default::default()
            }]);
        }
        if let Some(a) = m.srvr_rndr_set_active {
            if a.active == Some(true) {
                tracing::info!("selected in the Qobuz app");
            }
            if a.active == Some(false) {
                tracing::info!("renderer deactivated by the server");
                self.stop_playback();
                self.send_state();
            }
        }
        if let Some(q) = m.srvr_rndr_set_max_audio_quality {
            if let Some(requested) = q.max_audio_quality.and_then(Quality::from_protocol) {
                // Never exceed what the output plays natively.
                self.max_quality = requested.min(self.cfg.quality);
                tracing::info!("max audio quality set to {:?}", self.max_quality);
                self.send_max_quality();
            }
        }
        let loop_mode = m
            .srvr_rndr_set_loop_mode
            .map(|l| l.loop_mode)
            .or(m.srvr_ctrl_loop_mode_set.map(|l| l.loop_mode));
        if let Some(l) = loop_mode {
            self.loop_mode = l.unwrap_or(loop_mode::OFF);
            self.sync_preload();
        }
        if m.srvr_ctrl_queue_cleared.is_some() {
            self.next = None;
            self.sync_preload();
        }
    }

    fn on_set_state(&mut self, s: SrvrRndrSetState) {
        tracing::debug!("<- SetState {s:?}");
        if s.queue_version.is_some() {
            self.queue_version = s.queue_version;
        }
        let next_changed = s.next_queue_item.is_some() && s.next_queue_item != self.next;
        if s.next_queue_item.is_some() {
            self.next = s.next_queue_item;
        }
        let item_changed = match (&s.current_queue_item, &self.current) {
            (Some(new), Some(old)) => new.queue_item_id != old.queue_item_id || new.track_id != old.track_id,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if let Some(cur) = s.current_queue_item {
            self.current = Some(cur);
        }

        let want = s.playing_state.unwrap_or(self.playing_state);
        match want {
            playing_state::STOPPED => self.stop_playback(),
            playing_state::PLAYING | playing_state::PAUSED => {
                let paused = want == playing_state::PAUSED;
                let needs_load = item_changed || self.loaded.is_none() || self.error;
                if needs_load {
                    self.load_current(s.current_position.unwrap_or(0) as u64, paused);
                } else {
                    if let Some(pos) = s.current_position {
                        let pos = pos as u64;
                        if pos.abs_diff(self.player.shared.position_ms()) > SEEK_TOLERANCE_MS {
                            self.player.send(PlayerCmd::Seek { position_ms: pos });
                        }
                    }
                    if paused != (self.playing_state == playing_state::PAUSED) {
                        self.player.send(if paused { PlayerCmd::Pause } else { PlayerCmd::Resume });
                    }
                }
                if self.loaded.is_some() {
                    self.playing_state = want;
                }
            }
            other => tracing::debug!("SetState: ignoring playing state {other}"),
        }
        if next_changed {
            self.sync_preload();
        }
        self.send_state();
    }

    // ---------- player ----------

    fn on_player_event(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Started { load_id, sample_rate, bit_depth, channels, format_id } => {
                if load_id != self.load_id {
                    return;
                }
                let quality = Quality::from_format_id(format_id).unwrap_or(self.max_quality);
                self.send(vec![
                    QConnectMessage {
                        message_type: Some(msg::RNDR_SRVR_FILE_AUDIO_QUALITY_CHANGED),
                        rndr_srvr_file_audio_quality_changed: Some(RndrSrvrFileAudioQualityChanged {
                            sampling_rate: Some(sample_rate as i32),
                            bit_depth: Some(bit_depth as i32),
                            nb_channels: Some(channels as i32),
                            audio_quality: Some(quality.protocol()),
                        }),
                        ..Default::default()
                    },
                    // Bit-perfect: the device runs at exactly the file's format.
                    QConnectMessage {
                        message_type: Some(msg::RNDR_SRVR_DEVICE_AUDIO_QUALITY_CHANGED),
                        rndr_srvr_device_audio_quality_changed: Some(RndrSrvrDeviceAudioQualityChanged {
                            sampling_rate: Some(sample_rate as i32),
                            bit_depth: Some(bit_depth as i32),
                            nb_channels: Some(channels as i32),
                        }),
                        ..Default::default()
                    },
                ]);
                self.send_state();
            }
            PlayerEvent::Ended { load_id } => {
                if load_id != self.load_id {
                    return;
                }
                self.loaded = None;
                if self.loop_mode == loop_mode::REPEAT_ONE && self.current.is_some() {
                    self.load_current(0, false);
                } else if let Some(next) = self.next.take() {
                    self.current = Some(next);
                    self.load_current(0, false);
                } else {
                    self.playing_state = playing_state::STOPPED;
                }
                self.send_state();
            }
            PlayerEvent::Advanced { load_id } => {
                let Some((preload_id, item_id)) = self.preloaded.take() else { return };
                if preload_id != load_id {
                    return;
                }
                tracing::info!("gapless advance to queue item {item_id}");
                self.current = self.next.take();
                self.loaded = Some(item_id);
                self.load_id = load_id;
                self.error = false;
                self.playing_state = playing_state::PLAYING;
                self.send_state();
            }
            PlayerEvent::Paused { load_id } | PlayerEvent::Resumed { load_id } | PlayerEvent::Stopped { load_id }
                if load_id == self.load_id =>
            {
                self.playing_state = match ev {
                    PlayerEvent::Paused { .. } => playing_state::PAUSED,
                    PlayerEvent::Resumed { .. } => playing_state::PLAYING,
                    _ => {
                        self.loaded = None;
                        self.preloaded = None;
                        playing_state::STOPPED
                    }
                };
                tracing::info!("output changed state on its own: {ev:?}");
                self.send_state();
            }
            PlayerEvent::Paused { .. } | PlayerEvent::Resumed { .. } | PlayerEvent::Stopped { .. } => {}
            PlayerEvent::Error { load_id, message } => {
                if load_id != self.load_id {
                    return;
                }
                tracing::error!("playback error: {message}");
                self.loaded = None;
                self.error = true;
                self.playing_state = playing_state::STOPPED;
                self.send_state();
            }
        }
    }

    fn load_current(&mut self, position_ms: u64, paused: bool) {
        let Some(cur) = &self.current else {
            tracing::warn!("asked to play but no current queue item");
            return;
        };
        let (Some(track_id), Some(item_id)) = (cur.track_id, item_id(cur)) else {
            tracing::warn!("queue item without track id: {cur:?}");
            return;
        };
        self.load_id = self.new_id();
        self.preloaded = None; // a Load discards the output's preload
        self.loaded = Some(item_id);
        self.error = false;
        self.playing_state = if paused { playing_state::PAUSED } else { playing_state::PLAYING };
        tracing::info!("loading track {track_id} (queue item {item_id}) at {position_ms} ms");
        self.player.send(PlayerCmd::Load {
            load_id: self.load_id,
            track_id,
            position_ms,
            paused,
        });
        self.sync_preload();
    }

    fn new_id(&mut self) -> u64 {
        self.last_id += 1;
        self.last_id
    }

    /// Keep the output's gapless preload in line with the server's next item.
    fn sync_preload(&mut self) {
        let wanted = match (&self.next, self.loaded) {
            (Some(next), Some(_)) if self.loop_mode != loop_mode::REPEAT_ONE => {
                next.track_id.zip(item_id(next))
            }
            _ => None,
        };
        match wanted {
            Some((_, item_id)) if self.preloaded.is_some_and(|(_, i)| i == item_id) => {}
            Some((track_id, item_id)) => {
                let load_id = self.new_id();
                self.preloaded = Some((load_id, item_id));
                self.player.send(PlayerCmd::Preload(Some(PreloadReq {
                    load_id,
                    track_id,
                })));
            }
            None => {
                if self.preloaded.take().is_some() {
                    self.player.send(PlayerCmd::Preload(None));
                }
            }
        }
    }

    fn stop_playback(&mut self) {
        if self.loaded.take().is_some() {
            self.player.send(PlayerCmd::Stop);
        }
        self.load_id = self.new_id(); // ignore late events from the stopped load
        self.preloaded = None;
        self.playing_state = playing_state::STOPPED;
    }

    fn apply_volume(&self) {
        // With a fixed volume only mute reaches the output.
        let percent = (!self.cfg.fixed_volume).then_some(self.volume);
        self.player.send(PlayerCmd::SetVolume { percent, muted: self.muted });
    }

    // ---------- outbound ----------

    fn renderer_state(&self) -> RendererState {
        let shared = &self.player.shared;
        let loaded = self.loaded.is_some();
        let buffer = if self.error {
            buffer_state::ERROR
        } else if loaded && shared.is_buffering() {
            buffer_state::BUFFERING
        } else {
            buffer_state::OK
        };
        RendererState {
            playing_state: Some(self.playing_state),
            buffer_state: Some(buffer),
            current_position: Some(Position {
                timestamp: Some(now_ms()),
                value: Some(if loaded { shared.position_ms() as u32 } else { 0 }),
            }),
            duration: loaded.then(|| shared.duration_ms() as u32).filter(|d| *d > 0),
            queue_version: self.queue_version,
            current_queue_item_id: self.current.as_ref().and_then(item_id),
            next_queue_item_id: self.next.as_ref().and_then(item_id),
        }
    }

    fn send_state(&mut self) {
        let state = self.renderer_state();
        self.send(vec![QConnectMessage {
            message_type: Some(msg::RNDR_SRVR_STATE_UPDATED),
            rndr_srvr_state_updated: Some(RndrSrvrStateUpdated { state: Some(state) }),
            ..Default::default()
        }]);
    }

    fn send_volume(&mut self) {
        self.send(vec![QConnectMessage {
            message_type: Some(msg::RNDR_SRVR_VOLUME_CHANGED),
            rndr_srvr_volume_changed: Some(RndrSrvrVolumeChanged { volume: Some(self.volume) }),
            ..Default::default()
        }]);
    }

    fn send_max_quality(&mut self) {
        self.send(vec![QConnectMessage {
            message_type: Some(msg::RNDR_SRVR_MAX_AUDIO_QUALITY_CHANGED),
            rndr_srvr_max_audio_quality_changed: Some(RndrSrvrMaxAudioQualityChanged {
                audio_quality: Some(self.max_quality.protocol()),
                network_type: None,
            }),
            ..Default::default()
        }]);
    }

    fn send(&mut self, messages: Vec<QConnectMessage>) {
        let Some(ws) = &self.ws else { return };
        if !self.ws_connected || !self.registered {
            return;
        }
        self.batch_counter = self.batch_counter.wrapping_add(1);
        let batch = QConnectBatch {
            messages_time: Some(now_ms()),
            messages_id: Some(self.batch_counter),
            messages,
        };
        let _ = ws.batch_tx.send(batch);
    }
}

/// Queue item id of a track reference; the server sends -1 for "none".
fn item_id(r: &QueueTrackRef) -> Option<i32> {
    r.queue_item_id.filter(|&id| id >= 0)
}
