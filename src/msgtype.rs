//! Qobuz Connect protocol constants. Kept complete as a protocol reference,
//! so not every value is used.
#![allow(dead_code)]

/// qcloud outer frame message types.
pub mod qcloud {
    pub const AUTHENTICATE: u8 = 1;
    pub const SUBSCRIBE: u8 = 2;
    pub const UNSUBSCRIBE: u8 = 3;
    pub const PAYLOAD: u8 = 6;
    pub const ERROR: u8 = 9;
    pub const DISCONNECT: u8 = 10;

    /// QCloudProto::QP_QCONNECT.
    pub const PROTO_QCONNECT: u32 = 1;

    /// Channel ids are a type byte, optionally followed by data. Every
    /// QConnect payload the web player sends goes to the backend channel.
    pub const CHANNEL_BACKEND: u8 = 2;
}

/// Inner QConnect message types.
pub mod msg {
    // Renderer -> Server
    pub const RNDR_SRVR_JOIN_SESSION: i32 = 21;
    pub const RNDR_SRVR_DEVICE_INFO_UPDATED: i32 = 22;
    pub const RNDR_SRVR_STATE_UPDATED: i32 = 23;
    pub const RNDR_SRVR_RENDERER_ACTION: i32 = 24;
    pub const RNDR_SRVR_VOLUME_CHANGED: i32 = 25;
    pub const RNDR_SRVR_FILE_AUDIO_QUALITY_CHANGED: i32 = 26;
    pub const RNDR_SRVR_DEVICE_AUDIO_QUALITY_CHANGED: i32 = 27;
    pub const RNDR_SRVR_MAX_AUDIO_QUALITY_CHANGED: i32 = 28;
    pub const RNDR_SRVR_VOLUME_MUTED: i32 = 29;

    // Controller -> Server
    pub const CTRL_SRVR_JOIN_SESSION: i32 = 61;
    pub const CTRL_SRVR_SET_ACTIVE_RENDERER: i32 = 63;

    // Server -> Renderer
    pub const SRVR_RNDR_SET_STATE: i32 = 41;
    pub const SRVR_RNDR_SET_VOLUME: i32 = 42;
    pub const SRVR_RNDR_SET_ACTIVE: i32 = 43;
    pub const SRVR_RNDR_SET_MAX_AUDIO_QUALITY: i32 = 44;
    pub const SRVR_RNDR_SET_LOOP_MODE: i32 = 45;
    pub const SRVR_RNDR_SET_SHUFFLE_MODE: i32 = 46;
    pub const SRVR_RNDR_MUTE_VOLUME: i32 = 47;

    // Server -> Controller (observed by renderer)
    pub const SRVR_CTRL_SESSION_STATE: i32 = 81;
    pub const SRVR_CTRL_RENDERER_STATE_UPDATED: i32 = 82;
    pub const SRVR_CTRL_ADD_RENDERER: i32 = 83;
    pub const SRVR_CTRL_UPDATE_RENDERER: i32 = 84;
    pub const SRVR_CTRL_REMOVE_RENDERER: i32 = 85;
    pub const SRVR_CTRL_ACTIVE_RENDERER_CHANGED: i32 = 86;
    pub const SRVR_CTRL_VOLUME_CHANGED: i32 = 87;
    pub const SRVR_CTRL_QUEUE_ERROR_MESSAGE: i32 = 88;
    pub const SRVR_CTRL_QUEUE_CLEARED: i32 = 89;
    pub const SRVR_CTRL_QUEUE_STATE: i32 = 90;
    pub const SRVR_CTRL_QUEUE_TRACKS_LOADED: i32 = 91;
    pub const SRVR_CTRL_QUEUE_TRACKS_INSERTED: i32 = 92;
    pub const SRVR_CTRL_QUEUE_TRACKS_ADDED: i32 = 93;
    pub const SRVR_CTRL_QUEUE_TRACKS_REMOVED: i32 = 94;
    pub const SRVR_CTRL_QUEUE_TRACKS_REORDERED: i32 = 95;
    pub const SRVR_CTRL_SHUFFLE_MODE_SET: i32 = 96;
    pub const SRVR_CTRL_LOOP_MODE_SET: i32 = 97;
    pub const SRVR_CTRL_VOLUME_MUTED: i32 = 98;
    pub const SRVR_CTRL_MAX_AUDIO_QUALITY_CHANGED: i32 = 99;
    pub const SRVR_CTRL_FILE_AUDIO_QUALITY_CHANGED: i32 = 100;
    pub const SRVR_CTRL_DEVICE_AUDIO_QUALITY_CHANGED: i32 = 101;
    pub const SRVR_CTRL_AUTOPLAY_MODE_SET: i32 = 102;
    pub const SRVR_CTRL_AUTOPLAY_TRACKS_LOADED: i32 = 103;
    pub const SRVR_CTRL_AUTOPLAY_TRACKS_REMOVED: i32 = 104;
    pub const SRVR_CTRL_QUEUE_TRACKS_ADDED_FROM_AUTOPLAY: i32 = 105;
}

/// Renderer action codes (RndrSrvrRendererAction.action).
pub mod action {
    pub const PREVIOUS: i32 = 1;
    pub const NEXT: i32 = 2;
}

/// Join session reasons.
pub mod join_reason {
    pub const UNKNOWN: i32 = 0;
    pub const CONTROLLER_REQUEST: i32 = 1;
    pub const RECONNECTION: i32 = 2;
}

/// Playing states (protocol values).
pub mod playing_state {
    pub const UNKNOWN: i32 = 0;
    pub const STOPPED: i32 = 1;
    pub const PLAYING: i32 = 2;
    pub const PAUSED: i32 = 3;
}

/// Buffer states (protocol values).
pub mod buffer_state {
    pub const UNKNOWN: i32 = 0;
    pub const BUFFERING: i32 = 1;
    pub const OK: i32 = 2;
    pub const ERROR: i32 = 3;
    pub const UNDERRUN: i32 = 4;
}

/// Loop modes (protocol values).
pub mod loop_mode {
    pub const UNKNOWN: i32 = 0;
    pub const OFF: i32 = 1;
    pub const REPEAT_ONE: i32 = 2;
    pub const REPEAT_ALL: i32 = 3;
}

/// Qobuz web app API constants (public, from the production config of the
/// play.qobuz.com bundle 8.2.0). `secret.rs` reads the app id, the OAuth key
/// and the secret from the live bundle; these are the fallback.
pub mod api {
    pub const APP_ID: &str = "798273057";
    /// Request-signing secret at the time of writing (bundle 8.2.0-b034).
    /// `secret.rs` re-derives it from the live bundle when Qobuz rejects it.
    pub const APP_SECRET: &str = "abb21364945c0583309667d13ca3d93a";
    /// Sent with the OAuth authorisation code to `oauth/callback`.
    pub const OAUTH_PRIVATE_KEY: &str = "6lz8C03UDIC7";
    /// Web player version the constants above come from.
    pub const WEB_PLAYER_VERSION: &str = "8.2.0-b034";
    pub const SITE_BASE: &str = "https://www.qobuz.com";
    pub const API_BASE: &str = "https://www.qobuz.com/api.json/0.2";
    pub const USER_AGENT: &str = "Mozilla/5.0";
    pub const PLAY_ORIGIN: &str = "https://play.qobuz.com";
}
