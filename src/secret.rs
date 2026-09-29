//! Request-signing secret of the Qobuz web player.
//!
//! The web player no longer ships its secret as a constant: it rebuilds it at
//! run time from base64 fragments spread over its bundle
//! (`rng.prototype.initialization`): a seed, plus the `info` and `extras`
//! fields of one entry of a timezone table; the concatenation, minus its
//! last 44 characters, is base64 for the secret.
//!
//! qconnect starts from the built-in `APP_SECRET` (or the cached one), and
//! re-derives it from the live bundle when Qobuz rejects a signature.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::msgtype::api;

/// Minimum delay between two bundle downloads.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

struct State {
    secret: Option<String>,
    cache: Option<PathBuf>,
    last_refresh: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State { secret: None, cache: None, last_refresh: None });
/// One bundle download at a time.
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Serialize, Deserialize)]
struct Cached {
    secret: String,
    bundle: String,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Use `dir` to persist the derived secret, and load it if present.
pub fn init(dir: PathBuf) {
    let path = dir.join("web-secret.json");
    let cached = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Cached>(&raw).ok())
        .filter(|c| is_secret(&c.secret));
    let mut st = state();
    if let Some(c) = cached {
        tracing::debug!("signing secret from cache (bundle {})", c.bundle);
        st.secret = Some(c.secret);
    }
    st.cache = Some(path);
}

/// Secret to sign requests with.
pub fn current() -> String {
    state().secret.clone().unwrap_or_else(|| api::APP_SECRET.to_string())
}

fn is_secret(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether Qobuz refused the request signature.
pub fn is_signature_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<crate::api::HttpError>()
            .is_some_and(|h| h.status == 400 && h.body.contains("request_sig"))
    })
}

/// Re-derive the secret from the live web player. Returns whether it
/// changed, i.e. whether retrying the rejected request is worth it.
pub async fn refresh(http: &reqwest::Client, web_origin: &str) -> bool {
    let _guard = REFRESH.lock().await;
    {
        let mut st = state();
        if st.last_refresh.is_some_and(|t| t.elapsed() < MIN_REFRESH_INTERVAL) {
            // Someone refreshed meanwhile: retry only if that changed things.
            return st.secret.is_some();
        }
        st.last_refresh = Some(Instant::now());
    }
    let (bundle, secret) = match fetch(http, web_origin).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("cannot derive the signing secret from the web player: {e:#}");
            return false;
        }
    };
    let cache = {
        let mut st = state();
        if st.secret.as_deref().unwrap_or(api::APP_SECRET) == secret {
            tracing::warn!("signature refused, but the web player ({bundle}) still uses the same secret");
            return false;
        }
        st.secret = Some(secret.clone());
        st.cache.clone()
    };
    tracing::info!("new signing secret from web player bundle {bundle}");
    if let Some(path) = cache {
        let raw = serde_json::to_string(&Cached { secret, bundle }).expect("serialises");
        if let Err(e) = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, raw)) {
            tracing::warn!("cannot cache the signing secret in {}: {e}", path.display());
        }
    }
    true
}

async fn fetch(http: &reqwest::Client, web_origin: &str) -> Result<(String, String)> {
    let get = |url: String| async move {
        let resp = http.get(&url).send().await?.error_for_status()?;
        anyhow::Ok(resp.text().await?)
    };
    let page = get(format!("{web_origin}/login")).await.context("login page")?;
    let bundle = bundle_path(&page).ok_or_else(|| anyhow!("no bundle.js on the login page"))?;
    let js = get(format!("{web_origin}{bundle}")).await.with_context(|| format!("downloading {bundle}"))?;
    Ok((bundle.to_string(), derive(&js)?))
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
    const BUNDLE: &str = r#"x;c.initialization=function(){var e="nightly",t="integration",r=window.__ENVIRONMENT__;return"recette"===r?c.initialSeed("ZjY5YTc3MzQ2ODZjYjk0Mjc2MjkzNz",window.utimezone.london):"beta"!==r?[e,t].includes(r)?c.initialSeed("ODA2MzMxYzNiMGI2NDFkYTkyM2I4OT",window.utimezone.abidjan):c.initialSeed("YWJiMjEzNjQ5NDVjMDU4MzMwOTY2N2",window.utimezone.berlin):void 0},c.string=function(e,t){};
16242:(e,t)=>{t.default={abidjan:1,london:2,berlin:3,timezones:[{offset:"GMT-12:00",name:"Etc/GMT-12"},{offset:"GMT",name:"Africa/Abidjan",info:"BhZWQwMWQwNGE=NTI2OGQwY2ZmZD",extras:"UwNGZkYzliMTRhY2YxYzI3MzExOWE="},{offset:"GMT",name:"Europe/London",info:"hhNGI3YWMzODE=MTlkMTI1OWM1Nj",extras:"VkNDNlNDg2OWU2MjU1YWUxYTdmZTU="},{offset:"GMT+02:00",name:"Europe/Berlin",info:"QxM2NhM2Q5M2E=ZWI4MDU5YWEyNT",extras:"RlNDZjNTkyNTAzYjdkZGMwOGI5MGQ="},{offset:"GMT+03:00",name:"Africa/Addis_Ababa"}]};e.exports=t.default}"#;

    #[test]
    fn secret_is_derived_from_the_bundle() {
        assert_eq!(derive(BUNDLE).unwrap(), "abb21364945c0583309667d13ca3d93a");
        assert!(derive("nothing here").is_err());
    }

    /// Against the live web player: `cargo test live_bundle -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_bundle_still_yields_a_secret() {
        let http = reqwest::Client::builder().user_agent(api::USER_AGENT).build().unwrap();
        let (bundle, secret) = fetch(&http, api::PLAY_ORIGIN).await.unwrap();
        println!("{bundle}: {secret}");
        assert!(is_secret(&secret));
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
        assert!(!is_signature_error(&err(400, "Invalid track_id")));
        assert!(!is_signature_error(&err(401, "request_sig")));
    }
}
