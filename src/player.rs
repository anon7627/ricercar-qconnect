//! What the Qobuz Connect session drives: commands in, events out, and a
//! lock-free view of the playback position. The output behind it is the
//! plugin host (`plugin::remote`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::api::now_ms;

/// Commands the session sends to the player.
#[derive(Debug)]
pub enum PlayerCmd {
    /// Replace whatever is playing with `track_id`. `load_id` is echoed back in
    /// events so the session can ignore events from superseded loads.
    Load { load_id: u64, track_id: u32, position_ms: u64, paused: bool },
    Pause,
    Resume,
    Stop,
    Seek { position_ms: u64 },
    /// `percent: None` leaves the volume alone (only mute changes).
    SetVolume { percent: Option<u32>, muted: bool },
    /// Prepare the track that follows the current one, for gapless playback.
    /// `None` withdraws a previous preload. Outputs that cannot preload
    /// ignore it; the session then falls back to `Ended` + `Load`.
    Preload(Option<PreloadReq>),
}

#[derive(Debug, Clone, Copy)]
pub struct PreloadReq {
    pub load_id: u64,
    pub track_id: u32,
}

/// Events the player reports to the session.
#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Started { load_id: u64, sample_rate: u32, bit_depth: u32, channels: u32, format_id: i32 },
    Ended { load_id: u64 },
    /// The output moved on to the preloaded track by itself (gapless); it is
    /// now the current load. A `Started` for it follows.
    Advanced { load_id: u64 },
    /// Playback was paused, resumed or stopped on the output itself (e.g. from
    /// the host's own UI), not by a command from the session.
    Paused { load_id: u64 },
    Resumed { load_id: u64 },
    Stopped { load_id: u64 },
    Error { load_id: u64, message: String },
}

/// Lock-free view of playback state, read by the session for state reports.
#[derive(Debug, Default)]
pub struct Shared {
    playing: AtomicBool,
    buffering: AtomicBool,
    pos_ms: AtomicU64,
    pos_timestamp_ms: AtomicU64,
    duration_ms: AtomicU64,
}

impl Shared {
    /// Current position, extrapolated from the last measurement while playing.
    pub fn position_ms(&self) -> u64 {
        let value = self.pos_ms.load(Ordering::Relaxed);
        if !self.playing.load(Ordering::Relaxed) {
            return value;
        }
        let ts = self.pos_timestamp_ms.load(Ordering::Relaxed);
        value.saturating_add(now_ms().saturating_sub(ts))
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration_ms.load(Ordering::Relaxed)
    }

    pub fn is_buffering(&self) -> bool {
        self.buffering.load(Ordering::Relaxed)
    }

    pub(crate) fn set_position(&self, ms: u64) {
        self.pos_ms.store(ms, Ordering::Relaxed);
        self.pos_timestamp_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub(crate) fn set_playing(&self, playing: bool) {
        // Freeze or restart extrapolation from the current position.
        self.set_position(self.position_ms());
        self.playing.store(playing, Ordering::Relaxed);
    }

    pub(crate) fn set_buffering(&self, buffering: bool) {
        self.buffering.store(buffering, Ordering::Relaxed);
    }

    pub(crate) fn set_duration(&self, ms: u64) {
        self.duration_ms.store(ms, Ordering::Relaxed);
    }
}

pub struct Player {
    cmd_tx: UnboundedSender<PlayerCmd>,
    pub event_rx: UnboundedReceiver<PlayerEvent>,
    pub shared: Arc<Shared>,
}

/// The output side of a [`Player`]: what a backend task consumes and feeds.
pub(crate) struct Backend {
    pub cmd_rx: UnboundedReceiver<PlayerCmd>,
    pub event_tx: UnboundedSender<PlayerEvent>,
    pub shared: Arc<Shared>,
}

impl Player {
    pub(crate) fn channels() -> (Player, Backend) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded_channel();
        let shared = Arc::new(Shared::default());
        (Player { cmd_tx, event_rx, shared: shared.clone() }, Backend { cmd_rx, event_tx, shared })
    }

    /// A handle that can still stop the output once the session is gone.
    pub fn sender(&self) -> UnboundedSender<PlayerCmd> {
        self.cmd_tx.clone()
    }

    pub fn send(&self, cmd: PlayerCmd) {
        if self.cmd_tx.send(cmd).is_err() {
            tracing::error!("player: controller task is gone");
        }
    }
}
