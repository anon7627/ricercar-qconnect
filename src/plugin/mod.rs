//! `qconnect plugin`: source plugin (protocol v1, see `docs/plugin.md`).
//! JSON-RPC on stdin/stdout.
//!
//! Once signed in, the plugin also joins Qobuz Connect as a renderer and
//! drives the host player through `player.*` requests (`remote_control`).

mod catalog;
mod details;
mod items;
mod lyrics;
mod playlists;
mod relay;
mod remote;
mod report;
mod resolve;
mod rpc;
mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::task::AbortHandle;

use crate::account;
use crate::api::{ApiClient, HttpError, UserInfo};
use crate::auth::Credentials;
use crate::config::DeviceIdentity;
use crate::player::Player;
use crate::session::{self, SessionParams};
use remote::{Formats, HostEvent, HostState};
use resolve::Output;
use rpc::{Incoming, Out, RpcError, RpcResult, AUTH_REQUIRED};

pub const PROTOCOL: u64 = 1;
/// How long `auth.begin`'s loopback listener waits for the browser.
const LOGIN_WAIT: Duration = Duration::from_secs(15 * 60);
/// Favourites may change in other apps: read their ids again after this.
const FAVORITES_MAX_AGE: Duration = Duration::from_secs(10 * 60);

struct State {
    data_dir: Option<PathBuf>,
    output: Output,
    /// Loopback listener and waiter of a pending `auth.begin`.
    login: Vec<AbortHandle>,
    /// `auth.changed {expired}` already sent for the current token.
    expiry_notified: bool,
    /// Host name from `initialize`, for the Qobuz Connect device name.
    host_name: String,
    /// Language of the host's `locale` (`fr`), for the names Qobuz gives in
    /// every language.
    lang: String,
    settings: settings::Settings,
    /// Qobuz user id of the signed-in account, for play reports.
    user_id: Option<u64>,
    /// When the favourite ids were last requested.
    favorites_at: Option<std::time::Instant>,
    connect: Option<Connect>,
}

/// Running Qobuz Connect renderer.
struct Connect {
    session: AbortHandle,
    events: tokio::sync::mpsc::UnboundedSender<HostEvent>,
}

struct Plugin {
    api_base: Option<String>,
    out: Out,
    state: Mutex<State>,
    /// Holds the user token and the cached streaming session.
    api: tokio::sync::Mutex<ApiClient>,
    formats: Formats,
    reports: report::Reporter,
    /// Serves CMAF streams as plain FLAC files (setting `cmaf`).
    relay: relay::Relay,
}

pub async fn run(api_base: Option<&str>) -> Result<()> {
    let (out, writer) = Out::stdout();
    let plugin = Arc::new(Plugin {
        api_base: api_base.map(str::to_string),
        out: out.clone(),
        state: Mutex::new(State {
            data_dir: None,
            output: Output::default(),
            login: Vec::new(),
            expiry_notified: false,
            host_name: "qconnect".into(),
            lang: "en".into(),
            settings: settings::Settings::default(),
            user_id: None,
            favorites_at: None,
            connect: None,
        }),
        api: tokio::sync::Mutex::new(ApiClient::new()),
        formats: Formats::default(),
        reports: report::Reporter::default(),
        relay: relay::Relay::default(),
    });
    *plugin.api.lock().await = plugin.new_api();
    tracing::info!("plugin mode, protocol v{PROTOCOL}");

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match rpc::parse(&line) {
            Incoming::Request { id, method, params } => match method.as_str() {
                // Handled in order: nothing else makes sense before it ends.
                "initialize" => out.respond(id, plugin.clone().initialize(params).await),
                "shutdown" => {
                    out.respond(id, Ok(Value::Null));
                    break;
                }
                _ => {
                    let plugin = plugin.clone();
                    tokio::spawn(async move {
                        let result = plugin.clone().handle(&method, params).await;
                        plugin.note_auth_failure(&method, &result);
                        plugin.out.respond(id, result);
                    });
                }
            },
            Incoming::Notification { method, params } => plugin.notification(&method, params),
            Incoming::Response { id, result } => out.deliver(&id, result),
            Incoming::Invalid { id, error } => out.respond(id, Err(error)),
        }
    }
    tracing::info!("plugin stopping");
    plugin.cancel_login();
    plugin.stop_connect();
    drop(out);
    drop(plugin);
    // In-flight tasks may still hold a sender; give the writer a moment.
    let _ = tokio::time::timeout(Duration::from_millis(500), writer).await;
    // The blocking stdin reader would keep the runtime alive.
    std::process::exit(0)
}

fn params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

#[derive(Deserialize)]
struct InitializeParams {
    data_dir: PathBuf,
    #[serde(default)]
    cache_dir: Option<PathBuf>,
    #[serde(default)]
    output: Option<Output>,
    #[serde(default)]
    protocol: Option<u64>,
    #[serde(default)]
    host: Option<HostInfo>,
    #[serde(default)]
    locale: Option<String>,
    /// Values the host stored for our settings.
    #[serde(default)]
    settings: Option<Value>,
}

#[derive(Deserialize)]
struct HostInfo {
    name: String,
}

#[derive(Deserialize)]
struct RefParams {
    #[serde(rename = "ref")]
    reference: String,
}

#[derive(Deserialize)]
struct FavoriteParams {
    #[serde(rename = "ref")]
    reference: String,
    on: bool,
}

#[derive(Deserialize)]
struct CompleteParams {
    input: String,
}

/// Account shown by the host. Never the e-mail address: hosts keep this in
/// their logs and diagnostic reports.
fn account_json(creds: &Credentials, info: Option<&UserInfo>) -> Value {
    let name = if creds.display_name.is_empty() { "Qobuz account" } else { &creds.display_name };
    let mut account = json!({"display_name": name});
    if let Some(sub) = info.and_then(|i| i.subscription.clone()) {
        account["detail"] = sub.into();
    }
    account
}

fn status_json(creds: &Credentials, info: Option<&UserInfo>) -> Value {
    json!({"state": "signed_in", "account": account_json(creds, info)})
}

impl Plugin {
    fn new_api(&self) -> ApiClient {
        match &self.api_base {
            Some(base) => ApiClient::with_base(base),
            None => ApiClient::new(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn data_dir(&self) -> Result<PathBuf, RpcError> {
        self.state().data_dir.clone().ok_or_else(|| RpcError::new(rpc::INVALID_REQUEST, "initialize first"))
    }

    /// Client with the user token, or `auth_required`.
    async fn authed_api(&self) -> Result<ApiClient, RpcError> {
        let api = self.api.lock().await;
        if api.has_user_token() {
            Ok(api.clone())
        } else {
            Err(RpcError::auth_required())
        }
    }

    async fn initialize(self: Arc<Self>, p: Value) -> RpcResult {
        let p: InitializeParams = params(p)?;
        if p.protocol.is_some_and(|v| v != PROTOCOL) {
            tracing::warn!("host speaks protocol {:?}, this plugin v{PROTOCOL}", p.protocol);
        }
        let creds = Credentials::load(&p.data_dir).map_err(RpcError::from)?;
        crate::secret::init(p.cache_dir.clone().unwrap_or_else(|| p.data_dir.clone()));
        // Off the handshake: requests meanwhile use the cached or built-in values.
        let web = self.new_api();
        tokio::spawn(async move { web.refresh_web_config_if_stale().await });
        {
            let mut st = self.state();
            st.output = p.output.unwrap_or_default();
            st.data_dir = Some(p.data_dir);
            st.expiry_notified = false;
            if let Some(host) = p.host.filter(|h| !h.name.trim().is_empty()) {
                st.host_name = host.name;
            }
            // `fr-FR`, `fr_FR.UTF-8` → `fr`.
            let lang = p.locale.as_deref().unwrap_or("").split(['-', '_', '.']).next().unwrap_or("").to_ascii_lowercase();
            st.lang = if lang.is_empty() { "en".into() } else { lang };
            items::set_lang(&st.lang);
            st.settings = settings::Settings::from_values(p.settings.as_ref());
            st.user_id = creds.as_ref().map(|c| c.user_id);
            st.favorites_at = None;
            items::set_user(st.user_id);
            tracing::info!("settings: {:?}", st.settings);
        }
        let mut api = self.new_api();
        if let Some(creds) = &creds {
            api.set_user_token(&creds.token);
        }
        *self.api.lock().await = api;
        tracing::info!("initialized, {}", if creds.is_some() { "signed in" } else { "signed out" });
        if creds.is_some() {
            self.start_connect().await;
            // Reports left over from last time.
            self.flush_reports();
            self.refresh_favorites();
        }
        let lang = self.state().lang.clone();
        Ok(json!({
            "protocol": PROTOCOL,
            "plugin": {"id": "qobuz", "name": "Qobuz", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "auth": true, "browse": true, "search": true, "resolve": true,
                // Always declared: whether reports go out is a setting.
                "favorites": true, "reporting": true, "remote_control": true, "library": true,
                "lyrics": true, "radio": true, "details": true, "playlist_edit": true
            },
            "settings": settings::declaration(&lang),
        }))
    }

    async fn handle(self: Arc<Self>, method: &str, p: Value) -> RpcResult {
        if matches!(method, "browse.list" | "search" | "item.get") || method.starts_with("library.") {
            self.refresh_favorites();
        }
        match method {
            "auth.status" => self.auth_status().await,
            "auth.begin" => self.auth_begin().await,
            "auth.complete" => {
                let p: CompleteParams = params(p)?;
                let code = account::extract_code(&p.input)
                    .ok_or_else(|| RpcError::invalid_params("no authorisation code in the input"))?;
                self.cancel_login();
                self.sign_in(&code).await
            }
            "auth.sign_out" => {
                self.cancel_login();
                self.stop_connect();
                let data_dir = self.data_dir()?;
                Credentials::delete(&data_dir).map_err(RpcError::from)?;
                self.state().user_id = None;
                items::set_user(None);
                self.reports.clear(Some(&data_dir));
                *self.api.lock().await = self.new_api();
                Ok(json!({"state": "signed_out"}))
            }
            "browse.root" => catalog::root(),
            "browse.list" => {
                let lang = self.state().lang.clone();
                catalog::list(&self.authed_api().await?, params(p)?, &lang).await
            }
            "search" => catalog::search(&self.authed_api().await?, params(p)?).await,
            "library.albums" | "library.artists" | "library.tracks" | "library.playlists" => {
                catalog::library(&self.authed_api().await?, method, params(p)?).await
            }
            "item.get" => {
                let p: RefParams = params(p)?;
                let lang = self.state().lang.clone();
                catalog::get(&self.authed_api().await?, &p.reference, &lang).await
            }
            "playlists.create" | "playlists.rename" | "playlists.delete" | "playlists.add" | "playlists.remove"
            | "playlists.move" => playlists::edit(&self.authed_api().await?, method, params(p)?).await,
            "item.details" => {
                let p: RefParams = params(p)?;
                details::get(&self.authed_api().await?, &p.reference).await
            }
            "radio.next" => catalog::radio_next(&self.authed_api().await?, params(p)?).await,
            "lyrics.get" => {
                let p: RefParams = params(p)?;
                lyrics::get(&self.authed_api().await?, &p.reference).await
            }
            "favorites.set" => {
                let p: FavoriteParams = params(p)?;
                catalog::set_favorite(&self.authed_api().await?, &p.reference, p.on).await
            }
            "track.resolve" => {
                let p: RefParams = params(p)?;
                let api = {
                    let mut api = self.api.lock().await;
                    if !api.has_user_token() {
                        return Err(RpcError::auth_required());
                    }
                    // Start the streaming session once, under the lock, so
                    // concurrent resolves share it.
                    if let Err(e) = api.ensure_session().await {
                        tracing::warn!("session/start failed: {e:#}");
                    }
                    api.clone()
                };
                let (output, cmaf) = {
                    let st = self.state();
                    (st.output.clone(), st.settings.cmaf)
                };
                let relay = cmaf.then_some(&self.relay);
                let (result, resolved, report) = resolve::resolve(&api, &p.reference, &output, relay).await?;
                self.formats.lock().unwrap_or_else(|e| e.into_inner()).insert(resolved.track_id, resolved);
                self.reports.resolved(resolved.track_id, resolved.format_id, report.blob, report.duration_s);
                Ok(result)
            }
            _ => Err(RpcError::new(rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
        }
    }

    fn notification(self: &Arc<Self>, method: &str, p: Value) {
        match method {
            "settings.changed" => {
                let s = settings::Settings::from_values(p.get("settings"));
                tracing::info!("settings changed: {s:?}");
                let stop_reports = !s.report_playback;
                self.state().settings = s;
                if stop_reports {
                    self.reports.clear(self.state().data_dir.as_deref());
                }
            }
            "playback.started" | "playback.ended" => self.playback(method, &p),
            "playback.progress" => {}
            "output.changed" => {
                // Accept the output object itself or wrapped in `output`.
                let p = p.get("output").cloned().unwrap_or(p);
                match serde_json::from_value::<Output>(p) {
                    Ok(output) => {
                        tracing::info!("output changed: max {:?} Hz / {:?} bits", output.max_rate, output.max_bits);
                        self.state().output = output;
                    }
                    Err(e) => tracing::warn!("output.changed: {e}"),
                }
            }
            "player.state" => match serde_json::from_value::<HostState>(p) {
                Ok(st) => self.to_connect(HostEvent::State(st)),
                Err(e) => tracing::warn!("player.state: {e}"),
            },
            "player.taken_over" => self.to_connect(HostEvent::TakenOver),
            _ => tracing::debug!("ignored notification {method}"),
        }
    }

    /// `playback.started` and `playback.ended` from the host: play reports,
    /// when the user left them on.
    fn playback(self: &Arc<Self>, method: &str, p: &Value) {
        let (enabled, user_id, data_dir) = {
            let st = self.state();
            (st.settings.report_playback, st.user_id, st.data_dir.clone())
        };
        let (true, Some(user_id)) = (enabled, user_id) else { return };
        let Some(reference) = p.get("ref").and_then(Value::as_str) else { return };
        if method == "playback.started" {
            let Some(events) = self.reports.started(reference, user_id) else { return };
            let me = self.clone();
            tokio::spawn(async move {
                let Ok(api) = me.authed_api().await else { return };
                if let Err(e) = api.report_streaming_start(&events).await {
                    tracing::info!("play report (start) not sent: {e:#}");
                }
            });
        } else {
            let listened_ms = p.get("listened_ms").and_then(Value::as_u64).unwrap_or(0);
            if self.reports.ended(reference, listened_ms, data_dir.as_deref()) {
                self.flush_reports();
            }
        }
    }

    /// Read the favourite ids again in the background, when they are older
    /// than `FAVORITES_MAX_AGE` (or were never read). Items say whether they
    /// are favourites once the ids are in.
    fn refresh_favorites(self: &Arc<Self>) {
        {
            let mut st = self.state();
            if st.user_id.is_none() || st.favorites_at.is_some_and(|t| t.elapsed() < FAVORITES_MAX_AGE) {
                return;
            }
            st.favorites_at = Some(std::time::Instant::now());
        }
        let me = self.clone();
        tokio::spawn(async move {
            let Ok(api) = me.authed_api().await else { return };
            match api.favorite_ids().await {
                Ok(v) => items::set_favorites(Some(items::FavoriteIds::from_json(&v))),
                Err(e) => {
                    tracing::info!("favourite ids not read: {e:#}");
                    me.state().favorites_at = None;
                }
            }
        });
    }

    /// Send the queued end-of-play reports in the background.
    fn flush_reports(self: &Arc<Self>) {
        if !self.state().settings.report_playback {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let Ok(api) = me.authed_api().await else { return };
            let data_dir = me.state().data_dir.clone();
            me.reports.flush(&api, data_dir.as_deref()).await;
        });
    }

    /// A token refused mid-session: tell the host once, without waiting
    /// for its next `auth.status`.
    fn note_auth_failure(&self, method: &str, result: &RpcResult) {
        if !matches!(result, Err(e) if e.code == AUTH_REQUIRED) || method.starts_with("auth.") {
            return;
        }
        let Ok(api) = self.api.try_lock() else { return };
        if !api.has_user_token() {
            return;
        }
        drop(api);
        let mut st = self.state();
        if !st.expiry_notified {
            st.expiry_notified = true;
            self.out.notify("auth.changed", json!({"state": "expired"}));
            drop(st);
            self.stop_connect();
        }
    }

    async fn auth_status(&self) -> RpcResult {
        let Some(creds) = Credentials::load(&self.data_dir()?).map_err(RpcError::from)? else {
            return Ok(json!({"state": "signed_out"}));
        };
        let mut api = self.new_api();
        api.set_user_token(&creds.token);
        match api.user_info().await {
            Ok(info) => Ok(status_json(&creds, Some(&info))),
            Err(e) if e.chain().any(|c| c.downcast_ref::<HttpError>().is_some_and(|h| matches!(h.status, 400 | 401 | 403))) => {
                tracing::info!("stored token refused: {e:#}");
                Ok(json!({"state": "expired", "account": account_json(&creds, None)}))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Sign-in URL. A loopback listener also catches the redirect when the
    /// browser runs on this machine; otherwise the user pastes the address.
    async fn auth_begin(self: Arc<Self>) -> RpcResult {
        self.data_dir()?;
        self.cancel_login();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await;
        let (redirect, handles) = match listener.and_then(|l| Ok((l.local_addr()?.port(), l))) {
            Ok((port, listener)) => {
                let (code_tx, mut code_rx) = tokio::sync::mpsc::unbounded_channel();
                let server = account::serve_callback(listener, code_tx).abort_handle();
                let me = self.clone();
                let server_handle = server.clone();
                let waiter = tokio::spawn(async move {
                    if let Ok(Some(code)) = tokio::time::timeout(LOGIN_WAIT, code_rx.recv()).await {
                        match me.sign_in(&code).await {
                            Ok(status) => me.out.notify("auth.changed", status),
                            Err(e) => tracing::warn!("sign-in through the loopback redirect failed: {}", e.message),
                        }
                    }
                    server_handle.abort();
                });
                (format!("http://localhost:{port}/login/callback"), vec![server, waiter.abort_handle()])
            }
            Err(e) => {
                tracing::warn!("no loopback listener ({e}); paste mode only");
                ("http://localhost/login/callback".to_string(), vec![])
            }
        };
        self.state().login = handles;
        let instructions = "Sign in on the Qobuz page. If the browser runs on this machine, sign-in completes by \
             itself. Otherwise copy the full address of the page you land on (it contains \"code_autorisation=\") and \
             paste it here.";
        Ok(json!({"url": ApiClient::oauth_url(&redirect), "instructions": instructions, "expects_input": true}))
    }

    async fn sign_in(self: &Arc<Self>, code: &str) -> RpcResult {
        let data_dir = self.data_dir()?;
        let (creds, info) = account::exchange_code(&self.new_api(), code).await?;
        creds.save(&data_dir).map_err(RpcError::from)?;
        let mut api = self.new_api();
        api.set_user_token(&creds.token);
        *self.api.lock().await = api;
        {
            let mut st = self.state();
            st.expiry_notified = false;
            st.user_id = Some(creds.user_id);
            st.favorites_at = None;
        }
        items::set_user(Some(creds.user_id));
        self.refresh_favorites();
        tracing::info!("signed in");
        self.stop_connect();
        self.start_connect().await;
        Ok(status_json(&creds, Some(&info)))
    }

    /// Join Qobuz Connect as a renderer that plays through the host.
    async fn start_connect(self: &Arc<Self>) {
        if self.state().connect.is_some() {
            return;
        }
        let Ok(api) = self.authed_api().await else { return };
        let Ok(data_dir) = self.data_dir() else { return };
        let identity = match DeviceIdentity::load_or_create(&data_dir) {
            Ok(identity) => identity,
            Err(e) => {
                tracing::warn!("no Qobuz Connect device identity ({e:#}); remote control disabled");
                return;
            }
        };
        let (host_name, quality) = {
            let st = self.state();
            (st.host_name.clone(), st.output.max_quality())
        };
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let hostname = hostname.trim();
        let params = SessionParams {
            name: if hostname.is_empty() { host_name.clone() } else { format!("{host_name} ({hostname})") },
            brand: host_name,
            model: "Linux".into(),
            quality,
            fixed_volume: false,
            identity,
        };
        let (player, backend) = Player::channels();
        let (events, events_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(remote::run(backend, events_rx, self.out.clone(), api.clone(), self.formats.clone()));
        let session = tokio::spawn(session::run(params, Arc::new(tokio::sync::Mutex::new(api)), player));
        let mut st = self.state();
        if st.connect.is_some() {
            session.abort(); // raced with another start
            return;
        }
        st.connect = Some(Connect { session: session.abort_handle(), events });
    }

    /// Leave Qobuz Connect. The output stops once the session is gone.
    fn stop_connect(&self) {
        if let Some(c) = self.state().connect.take() {
            tracing::info!("leaving Qobuz Connect");
            c.session.abort();
        }
    }

    fn to_connect(&self, ev: HostEvent) {
        if let Some(c) = &self.state().connect {
            let _ = c.events.send(ev);
        }
    }

    fn cancel_login(&self) {
        for h in std::mem::take(&mut self.state().login) {
            h.abort();
        }
    }
}
