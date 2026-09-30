//! What qconnect borrows from the Qobuz web player: its app id, the private
//! key it sends with OAuth codes, and its request-signing secret.
//!
//! All three are read from the live bundle (`play.qobuz.com/login`, then the
//! `bundle.js` it references), so that qconnect follows when Qobuz changes
//! them:
//! - app id: `production:{api:{appId:"…"` in the environment table;
//! - OAuth key: `authenticate({privateKey:"…"`;
//! - secret: not shipped as a constant but rebuilt at run time from base64
//!   fragments (`rng.prototype.initialization`): a seed, plus the `info` and
//!   `extras` fields of one entry of a timezone table; the concatenation,
//!   minus its last 44 characters, is base64 for the secret.
//!
//! qconnect starts from the cached values (`web-config.json`), or else the
//! built-in ones (`msgtype::api`). It reads the bundle again when the cache
//! is more than a day old, and whenever Qobuz refuses a signature or the app
//! id. The app id and the secret go together: one is never replaced without
//! the other.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::msgtype::api;

/// Minimum delay between two bundle downloads.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Age after which the cached values are checked against the live bundle.
const MAX_AGE_S: u64 = 24 * 3600;

/// Values read from one bundle.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WebConfig {
    pub app_id: String,
    pub secret: String,
    /// Absent from the cache of older versions, or not found in the bundle.
    #[serde(default)]
    pub oauth_key: Option<String>,
}

impl WebConfig {
    fn built_in() -> Self {
        WebConfig {
            app_id: api::APP_ID.into(),
            secret: api::APP_SECRET.into(),
            oauth_key: Some(api::OAUTH_PRIVATE_KEY.into()),
        }
    }

    fn is_valid(&self) -> bool {
        is_app_id(&self.app_id) && is_secret(&self.secret) && self.oauth_key.as_deref().is_none_or(is_oauth_key)
    }
}

struct State {
    config: Option<WebConfig>,
    cache: Option<PathBuf>,
    /// When the values in use were read from a bundle (unix seconds).
    fetched_at: u64,
    last_refresh: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State { config: None, cache: None, fetched_at: 0, last_refresh: None });
/// One bundle download at a time.
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Serialize, Deserialize)]
struct Cached {
    #[serde(flatten)]
    config: WebConfig,
    bundle: String,
    #[serde(default)]
    fetched_at: u64,
}

/// Cache of qconnect ≤ 0.1.5: the secret only.
#[derive(Deserialize)]
struct CachedSecret {
    secret: String,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Use `dir` to persist the values read from the bundle, and load them if
/// present.
pub fn init(dir: PathBuf) {
    let path = dir.join("web-config.json");
    let cached = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Cached>(&raw).ok())
        .filter(|c| c.config.is_valid());
    // An older cache only holds a secret for the built-in app id; the next
    // refresh replaces it.
    let legacy = || {
        let raw = std::fs::read_to_string(dir.join("web-secret.json")).ok()?;
        let c: CachedSecret = serde_json::from_str(&raw).ok()?;
        is_secret(&c.secret).then(|| WebConfig { secret: c.secret, ..WebConfig::built_in() })
    };
    let mut st = state();
    st.config = None;
    st.fetched_at = 0;
    if let Some(c) = cached {
        tracing::debug!("web player config from cache (bundle {}, app id {})", c.bundle, c.config.app_id);
        st.config = Some(c.config);
        st.fetched_at = c.fetched_at;
    } else if let Some(config) = legacy() {
        st.config = Some(config);
    }
    st.cache = Some(path);
}

fn config() -> WebConfig {
    state().config.clone().unwrap_or_else(WebConfig::built_in)
}

/// Secret to sign requests with.
pub fn current() -> String {
    config().secret
}

/// App id for `X-App-Id` and the OAuth sign-in page.
pub fn app_id() -> String {
    config().app_id
}

/// Private key sent with an OAuth code to `oauth/callback`.
pub fn oauth_key() -> String {
    config().oauth_key.unwrap_or_else(|| api::OAUTH_PRIVATE_KEY.into())
}

/// Whether the values in use were not checked against the live bundle for
/// a day.
pub fn is_stale() -> bool {
    now_s().saturating_sub(state().fetched_at) > MAX_AGE_S
}

fn is_app_id(s: &str) -> bool {
    (6..=12).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
}

fn is_oauth_key(s: &str) -> bool {
    (6..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_secret(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether Qobuz refused the request signature or the app id: both call for
/// reading the web player's values again.
pub fn is_signature_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<crate::api::HttpError>()
            .is_some_and(|h| h.status == 400 && (h.body.contains("request_sig") || h.body.contains("app_id")))
    })
}

/// Read the values from the live web player again. Returns whether they
/// changed, i.e. whether retrying a rejected request is worth it.
pub async fn refresh(http: &reqwest::Client, web_origin: &str) -> bool {
    let _guard = REFRESH.lock().await;
    let before = {
        let mut st = state();
        if st.last_refresh.is_some_and(|t| t.elapsed() < MIN_REFRESH_INTERVAL) {
            // Someone refreshed meanwhile: retry only if that changed things.
            return st.config.as_ref().is_some_and(|c| *c != WebConfig::built_in());
        }
        st.last_refresh = Some(Instant::now());
        st.config.clone().unwrap_or_else(WebConfig::built_in)
    };
    let (bundle, found) = match fetch(http, web_origin).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("cannot read the web player's config: {e:#}");
            return false;
        }
    };
    // Keep the known OAuth key if the bundle no longer shows it the same way.
    let config = WebConfig { oauth_key: found.oauth_key.or(before.oauth_key.clone()), ..found };
    let changed = config != before;
    let cache = {
        let mut st = state();
        st.config = Some(config.clone());
        st.fetched_at = now_s();
        st.cache.clone()
    };
    if changed {
        tracing::info!("web player {bundle}: new app id or keys (app id {})", config.app_id);
    } else {
        tracing::debug!("web player {bundle}: same app id and keys");
    }
    if let Some(path) = cache {
        let raw = serde_json::to_string(&Cached { config, bundle, fetched_at: now_s() }).expect("serialises");
        if let Err(e) = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, raw)) {
            tracing::warn!("cannot cache the web player's config in {}: {e}", path.display());
        }
    }
    changed
}

/// `refresh` when the values in use are more than a day old.
pub async fn refresh_if_stale(http: &reqwest::Client, web_origin: &str) {
    if is_stale() {
        refresh(http, web_origin).await;
    }
}

async fn fetch(http: &reqwest::Client, web_origin: &str) -> Result<(String, WebConfig)> {
    let get = |url: String| async move {
        let resp = http.get(&url).send().await?.error_for_status()?;
        anyhow::Ok(resp.text().await?)
    };
    let page = get(format!("{web_origin}/login")).await.context("login page")?;
    let bundle = bundle_path(&page).ok_or_else(|| anyhow!("no bundle.js on the login page"))?;
    let js = get(format!("{web_origin}{bundle}")).await.with_context(|| format!("downloading {bundle}"))?;
    Ok((bundle.to_string(), derive_config(&js)?))
}

/// App id, OAuth key and secret from the bundle source. The app id and the
/// secret are required: they only work together.
pub fn derive_config(js: &str) -> Result<WebConfig> {
    let production = js.find("production:{api:{").context("production environment not found")?;
    let app_id = between(&js[production..], "appId:\"", "\"").filter(|s| is_app_id(s)).context("production app id")?;
    let oauth_key = between(js, "authenticate({privateKey:\"", "\"").filter(|s| is_oauth_key(s));
    if oauth_key.is_none() {
        tracing::warn!("web player bundle: OAuth private key not found");
    }
    Ok(WebConfig { app_id: app_id.to_string(), secret: derive(js)?, oauth_key: oauth_key.map(str::to_string) })
}

/// `/resources/<version>/bundle.js`, as referenced by the login page.
fn bundle_path(page: &str) -> Option<&str> {
    let end = page.find("/bundle.js")? + "/bundle.js".len();
    let start = page[..end].rfind('"')? + 1;
    let path = &page[start..end];
    path.starts_with('/').then_some(path)
}

/// Text between `open` and `close`, after `from`.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let len = s[start..].find(close)?;
    Some(&s[start..start + len])
}

/// Secret from the bundle source.
pub fn derive(js: &str) -> Result<String> {
    // The production branch is the last `initialSeed(...)` call of
    // `initialization`: `…:c.initialSeed("<seed>",window.utimezone.<zone>):void 0}`.
    let init = js.find(".initialization=function(){").context("initialization() not found")?;
    let body = &js[init..];
    let body = &body[..body.find('}').context("initialization() not closed")?];
    let last = body.rfind("initialSeed(\"").context("no initialSeed call")?;
    let call = &body[last..];
    let seed = between(call, "initialSeed(\"", "\"").context("seed")?;
    let zone = between(call, "window.utimezone.", ")").context("zone")?;

    // The table: `{abidjan:34,…,berlin:53,…,timezones:[{offset:…,name:…,info:…,extras:…},…]}`.
    let table_at = js.find("timezones:[{offset:").context("timezone table not found")?;
    let head_start = js[..table_at].rfind('{').context("timezone table start")?;
    let head = &js[head_start..table_at];
    let index: usize = between(&format!("{head},"), &format!("{zone}:"), ",")
        .and_then(|n| n.parse().ok())
        .with_context(|| format!("index of zone {zone}"))?;
    let entries = &js[table_at + "timezones:[".len()..];
    let entries = &entries[..entries.find("]}").context("timezone table end")?];
    let entry = entries
        .split("},{")
        .nth(index)
        .with_context(|| format!("timezone entry {index}"))?;
    let info = between(entry, "info:\"", "\"").with_context(|| format!("info of zone {zone}"))?;
    let extras = between(entry, "extras:\"", "\"").with_context(|| format!("extras of zone {zone}"))?;

    let all = format!("{seed}{info}{extras}");
    let b64 = all.get(..all.len().saturating_sub(44)).unwrap_or_default();
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).context("base64")?;
    let secret = String::from_utf8(raw).context("utf-8")?;
    if !is_secret(&secret) {
        bail!("derived value is not a 32-digit hex secret");
    }
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relevant excerpts of the play.qobuz.com bundle 8.2.0-b034.
    const BUNDLE: &str = r#"c={nightly:{api:{appId:"377257687",appSecret:"05a4851e74ee47fda346f50cfdfc4f09"},braze:u,extra:l},production:{api:{appId:"798273057",appSecret:"05a4851e74ee47fda346f50cfdfc4f09"},braze:o(o({},u),{}),extra:l}};
n.authenticate({privateKey:"6lz8C03UDIC7",code:t});
x;c.initialization=function(){var e="nightly",t="integration",r=window.__ENVIRONMENT__;return"recette"===r?c.initialSeed("ZjY5YTc3MzQ2ODZjYjk0Mjc2MjkzNz",window.utimezone.london):"beta"!==r?[e,t].includes(r)?c.initialSeed("ODA2MzMxYzNiMGI2NDFkYTkyM2I4OT",window.utimezone.abidjan):c.initialSeed("YWJiMjEzNjQ5NDVjMDU4MzMwOTY2N2",window.utimezone.berlin):void 0},c.string=function(e,t){};
16242:(e,t)=>{t.default={abidjan:1,london:2,berlin:3,timezones:[{offset:"GMT-12:00",name:"Etc/GMT-12"},{offset:"GMT",name:"Africa/Abidjan",info:"BhZWQwMWQwNGE=NTI2OGQwY2ZmZD",extras:"UwNGZkYzliMTRhY2YxYzI3MzExOWE="},{offset:"GMT",name:"Europe/London",info:"hhNGI3YWMzODE=MTlkMTI1OWM1Nj",extras:"VkNDNlNDg2OWU2MjU1YWUxYTdmZTU="},{offset:"GMT+02:00",name:"Europe/Berlin",info:"QxM2NhM2Q5M2E=ZWI4MDU5YWEyNT",extras:"RlNDZjNTkyNTAzYjdkZGMwOGI5MGQ="},{offset:"GMT+03:00",name:"Africa/Addis_Ababa"}]};e.exports=t.default}"#;

    #[test]
    fn secret_is_derived_from_the_bundle() {
        assert_eq!(derive(BUNDLE).unwrap(), "abb21364945c0583309667d13ca3d93a");
        assert!(derive("nothing here").is_err());
    }

    #[test]
    fn app_id_and_keys_are_read_from_the_bundle() {
        assert_eq!(derive_config(BUNDLE).unwrap(), WebConfig::built_in(), "production values, not nightly's");
        let no_key = BUNDLE.replace("privateKey", "otherKey");
        assert_eq!(derive_config(&no_key).unwrap().oauth_key, None, "the OAuth key is optional");
        let no_app = BUNDLE.replace("production:", "prod:");
        assert!(derive_config(&no_app).is_err(), "no app id: no config");
    }

    /// Against the live web player: `cargo test live_bundle -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_bundle_still_yields_a_secret() {
        let http = reqwest::Client::builder().user_agent(api::USER_AGENT).build().unwrap();
        let (bundle, config) = fetch(&http, api::PLAY_ORIGIN).await.unwrap();
        println!("{bundle}: {config:?}");
        assert!(config.is_valid() && config.oauth_key.is_some());
    }

    #[test]
    fn bundle_is_found_on_the_login_page() {
        let page = r#"<script src="/resources/8.2.0-b034/bundle.js"></script>"#;
        assert_eq!(bundle_path(page), Some("/resources/8.2.0-b034/bundle.js"));
        assert_eq!(bundle_path("<html></html>"), None);
    }

    #[test]
    fn signature_errors_are_recognised() {
        let err = |status, body: &str| {
            anyhow::Error::from(crate::api::HttpError { status, retry_after: None, body: body.into() })
        };
        assert!(is_signature_error(&err(400, "Invalid Request Signature parameter (request_sig)").context("getFileUrl")));
        assert!(is_signature_error(&err(400, "Invalid or missing app_id parameter. (Root=1-6abd)")));
        assert!(!is_signature_error(&err(400, "Invalid track_id")));
        assert!(!is_signature_error(&err(401, "request_sig")));
    }
}
