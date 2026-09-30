//! Minimal Qobuz REST API client, authenticated with the account's user token.

use anyhow::{anyhow, Result};
use md5::{Digest, Md5};
use reqwest::header::{HeaderMap, HeaderValue};

use crate::config::Quality;
use crate::msgtype::api;
use crate::secret;

#[derive(Clone, Debug)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    /// Web player origin, where the signing secret is re-derived from.
    web_origin: String,
    user_auth_token: Option<String>,
    x_session_id: Option<String>,
    x_session_expires_ms: u64,
}

/// Non-2xx answer from the API, kept typed so callers can tell an expired
/// session (401) from a missing item (404) or rate limiting (429).
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    /// `Retry-After` in seconds, when sent.
    pub retry_after: Option<u64>,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.body)
    }
}

impl std::error::Error for HttpError {}

/// `track/getFileUrl` answered without a URL: Qobuz will not stream the
/// track. `restrictions` holds its reasons (`SampleRestrictedByRightHolders`,
/// `FormatRestrictedByFormatAvailability`…).
#[derive(Debug, Clone)]
pub struct StreamRefused {
    pub track_id: u32,
    pub restrictions: Vec<String>,
}

impl std::fmt::Display for StreamRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Qobuz does not stream track {}", self.track_id)?;
        if !self.restrictions.is_empty() {
            write!(f, " ({})", self.restrictions.join(", "))?;
        }
        Ok(())
    }
}

impl std::error::Error for StreamRefused {}

#[derive(Debug, Clone)]
pub struct StreamUrl {
    pub url: String,
    pub format_id: i32,
    pub mime_type: String,
    /// 0 when the API does not say.
    pub sample_rate: u32,
    pub bit_depth: u32,
    /// A 30 s preview instead of the full track (no subscription for it).
    pub sample: bool,
    /// Opaque token Qobuz wants back in the end-of-play report.
    pub blob: Option<String>,
}

enum PostBody {
    Form(Vec<(&'static str, String)>),
    Json(serde_json::Value),
}

#[derive(Debug, Clone)]
pub struct UserInfo {
    pub id: u64,
    pub email: String,
    pub display_name: String,
    pub subscription: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WsToken {
    pub jwt: String,
    pub exp: i64,
    pub endpoint: String,
}

impl WsToken {
    pub fn is_valid(&self) -> bool {
        !self.jwt.is_empty() && self.exp > 0 && !self.endpoint.is_empty()
    }
    pub fn is_expired(&self, buffer_s: i64) -> bool {
        now_unix_s() + buffer_s >= self.exp
    }
}

pub fn now_unix_s() -> i64 {
    (now_ms() / 1000) as i64
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ApiClient {
    pub fn new() -> Self {
        Self { web_origin: api::PLAY_ORIGIN.to_string(), ..Self::with_base(api::API_BASE) }
    }

    /// Client against another API root. Tests point it at a mock server,
    /// which then also plays the web player.
    pub fn with_base(base: &str) -> Self {
        Self {
            web_origin: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .user_agent(api::USER_AGENT)
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("http client"),
            base: base.trim_end_matches('/').to_string(),
            user_auth_token: None,
            x_session_id: None,
            x_session_expires_ms: 0,
        }
    }

    pub fn set_user_token(&mut self, jwt: &str) {
        self.user_auth_token = Some(jwt.to_string());
        self.x_session_id = None;
        self.x_session_expires_ms = 0;
    }

    /// Check the web player's app id and keys when the cached ones are more
    /// than a day old.
    pub async fn refresh_web_config_if_stale(&self) {
        secret::refresh_if_stale(&self.http, &self.web_origin).await;
    }

    pub fn has_user_token(&self) -> bool {
        self.user_auth_token.is_some()
    }

    fn base_headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert("X-App-Id", HeaderValue::from_str(&secret::app_id())?);
        if let Some(tok) = &self.user_auth_token {
            h.insert("X-User-Auth-Token", HeaderValue::from_str(tok)?);
        }
        if let Some(sid) = &self.x_session_id {
            h.insert("X-Session-Id", HeaderValue::from_str(sid)?);
        }
        Ok(h)
    }

    fn sign(obj: &str, action: &str, params: &[(&str, String)]) -> (String, String) {
        let request_ts = now_unix_s().to_string();
        let mut sorted: Vec<_> = params.iter().collect();
        sorted.sort_by_key(|(k, _)| *k);
        let mut sig = format!("{obj}{action}");
        for (k, v) in sorted {
            sig.push_str(k);
            sig.push_str(v);
        }
        sig.push_str(&request_ts);
        sig.push_str(&secret::current());
        let signature = format!("{:x}", Md5::digest(sig.as_bytes()));
        (request_ts, signature)
    }

    async fn check_json(resp: reqwest::Response) -> Result<serde_json::Value> {
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()?.parse().ok());
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpError { status: status.as_u16(), retry_after, body: body.chars().take(200).collect() }.into());
        }
        Ok(resp.json().await?)
    }

    /// Start (or reuse) a Qobuz streaming session. Caches the session id.
    pub async fn ensure_session(&mut self) -> Result<()> {
        if self.x_session_id.is_some() && self.x_session_expires_ms > now_ms() + 60_000 {
            return Ok(());
        }
        let (request_ts, signature) = Self::sign("session", "start", &[("profile", "qbz-1".into())]);
        let body = format!("profile=qbz-1&request_ts={request_ts}&request_sig={signature}");
        let resp = self
            .http
            .post(format!("{}/session/start", self.base))
            .headers(self.base_headers()?)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;
        let resp = Self::check_json(resp).await?;
        let sid = resp
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("session/start: no session_id in response"))?;
        self.x_session_id = Some(sid.to_string());
        let expires_s = resp.get("expires_at").and_then(|v| v.as_u64()).unwrap_or(0);
        self.x_session_expires_ms = expires_s * 1000;
        Ok(())
    }

    /// Fetch a streaming URL for a track at the given quality, falling back to
    /// lower qualities when the requested one is unavailable.
    pub async fn get_stream_url(&mut self, track_id: u32, quality: Quality) -> Result<StreamUrl> {
        match self.stream_url_once(track_id, quality).await {
            Err(e) if secret::is_signature_error(&e) && secret::refresh(&self.http, &self.web_origin).await => {
                self.x_session_id = None;
                self.stream_url_once(track_id, quality).await
            }
            r => r,
        }
    }

    async fn stream_url_once(&mut self, track_id: u32, quality: Quality) -> Result<StreamUrl> {
        if let Err(e) = self.ensure_session().await {
            tracing::warn!("api: session/start failed ({e}); trying without session");
        }
        let mut last_err = anyhow!("no quality available");
        for q in quality.fallback_chain() {
            let params = [
                ("format_id", q.format_id().to_string()),
                ("intent", "stream".to_string()),
                ("track_id", track_id.to_string()),
            ];
            let (request_ts, signature) = Self::sign("track", "getFileUrl", &params);
            let resp = self
                .http
                .get(format!("{}/track/getFileUrl", self.base))
                .headers(self.base_headers()?)
                .query(&params)
                .query(&[("request_ts", request_ts), ("request_sig", signature)])
                .send()
                .await;
            let resp = match resp {
                Ok(r) => Self::check_json(r).await,
                Err(e) => Err(e.into()),
            };
            match resp {
                Ok(resp) => {
                    if let Some(url) = resp.get("url").and_then(|v| v.as_str()) {
                        let format_id = resp
                            .get("format_id")
                            .and_then(|v| v.as_i64())
                            .map_or(q.format_id(), |v| v as i32);
                        let khz = resp.get("sampling_rate").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        return Ok(StreamUrl {
                            url: url.to_string(),
                            format_id,
                            mime_type: resp
                                .get("mime_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("audio/flac")
                                .to_string(),
                            sample_rate: (khz * 1000.0).round() as u32,
                            bit_depth: resp.get("bit_depth").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                            sample: resp.get("sample").and_then(|v| v.as_bool()).unwrap_or(false),
                            blob: resp.get("blob").and_then(|v| v.as_str()).map(str::to_string),
                        });
                    }
                    let restrictions: Vec<String> = resp
                        .get("restrictions")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r.get("code")?.as_str().map(str::to_string))
                        .collect();
                    // Only a format restriction leaves a lower quality a chance;
                    // any other one (rights, catalogue, region) applies to all.
                    let format_only = !restrictions.is_empty() && restrictions.iter().all(|c| c.starts_with("Format"));
                    let refused = StreamRefused { track_id, restrictions };
                    if !format_only {
                        tracing::info!("api: getFileUrl (format {}): {refused}", q.format_id());
                        return Err(refused.into());
                    }
                    last_err = refused.into();
                }
                // No other quality will be signed any better.
                Err(e) if secret::is_signature_error(&e) => return Err(e),
                Err(e) => last_err = e,
            }
            tracing::debug!("api: format {} unavailable for track {track_id}: {last_err}", q.format_id());
        }
        Err(last_err.context(format!("no stream url for track {track_id}")))
    }

    /// GET `<obj>/<action>` with the user token; `signed` adds the request
    /// signature some endpoints require.
    async fn get_json(&self, obj: &str, action: &str, params: &[(&str, String)], signed: bool) -> Result<serde_json::Value> {
        match self.get_json_once(obj, action, params, signed).await {
            Err(e) if signed && secret::is_signature_error(&e) && secret::refresh(&self.http, &self.web_origin).await => {
                self.get_json_once(obj, action, params, signed).await
            }
            r => r,
        }
    }

    async fn get_json_once(&self, obj: &str, action: &str, params: &[(&str, String)], signed: bool) -> Result<serde_json::Value> {
        let mut req = self
            .http
            .get(format!("{}/{obj}/{action}", self.base))
            .headers(self.base_headers()?)
            .query(params);
        if signed {
            let (request_ts, signature) = Self::sign(obj, action, params);
            req = req.query(&[("request_ts", request_ts), ("request_sig", signature)]);
        }
        Self::check_json(req.send().await?).await
    }

    /// Raw `track/get` response.
    pub async fn track_get(&self, track_id: &str) -> Result<serde_json::Value> {
        self.get_json("track", "get", &[("track_id", track_id.to_string())], false).await
    }

    /// `catalog/search`; `kind` is `tracks`, `albums`, `artists` or
    /// `playlists`, or `None` for every group.
    pub async fn search(&self, query: &str, kind: Option<&str>, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let mut params = vec![("query", query.to_string()), ("offset", offset.to_string()), ("limit", limit.to_string())];
        if let Some(kind) = kind {
            params.push(("type", kind.to_string()));
        }
        self.get_json("catalog", "search", &params, false).await
    }

    /// Album with a page of its tracks.
    pub async fn album_get(&self, album_id: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [("album_id", album_id.to_string()), ("offset", offset.to_string()), ("limit", limit.to_string())];
        self.get_json("album", "get", &params, false).await
    }

    /// Artist with a page of its albums.
    pub async fn artist_get(&self, artist_id: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [
            ("artist_id", artist_id.to_string()),
            ("extra", "albums".to_string()),
            ("offset", offset.to_string()),
            ("limit", limit.to_string()),
        ];
        self.get_json("artist", "get", &params, false).await
    }

    /// Playlist with a page of its tracks.
    pub async fn playlist_get(&self, playlist_id: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [
            ("playlist_id", playlist_id.to_string()),
            ("extra", "tracks".to_string()),
            ("offset", offset.to_string()),
            ("limit", limit.to_string()),
        ];
        self.get_json("playlist", "get", &params, false).await
    }

    /// `track/lyricsUrl` (signed): `{track_id, lyrics_url}`, 404 when the
    /// track has no lyrics.
    pub async fn lyrics_url(&self, track_id: &str) -> Result<serde_json::Value> {
        self.get_json("track", "lyricsUrl", &[("track_id", track_id.to_string())], true).await
    }

    /// A JSON document at an absolute https URL handed out by the API (lyrics
    /// on a CDN): no Qobuz headers, the URL carries its own authorisation.
    pub async fn get_document(&self, url: &str) -> Result<serde_json::Value> {
        if !url.starts_with("https://") && !url.starts_with(&self.base) {
            return Err(anyhow!("refusing a non-https document URL"));
        }
        Self::check_json(self.http.get(url).send().await?).await
    }

    /// `radio/<seed>`: about 30 tracks close to a track, album or artist
    /// (`seed` is `track`, `album` or `artist`). The API ignores `limit`.
    pub async fn radio(&self, seed: &str, id: &str) -> Result<serde_json::Value> {
        let key = format!("{seed}_id");
        self.get_json("radio", seed, &[(key.as_str(), id.to_string())], false).await
    }

    /// `album/suggest`: albums similar to one (about 30, not paged).
    pub async fn album_suggest(&self, album_id: &str) -> Result<serde_json::Value> {
        self.get_json("album", "suggest", &[("album_id", album_id.to_string())], false).await
    }

    /// Top-level genres.
    pub async fn genres(&self) -> Result<serde_json::Value> {
        self.get_json("genre", "list", &[], false).await
    }

    /// New releases of one genre.
    pub async fn genre_new_releases(&self, genre_id: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [
            ("type", "new-releases".to_string()),
            ("genre_id", genre_id.to_string()),
            ("offset", offset.to_string()),
            ("limit", limit.to_string()),
        ];
        self.get_json("album", "getFeatured", &params, false).await
    }

    /// Albums the account bought.
    pub async fn purchases(&self, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [("type", "albums".to_string()), ("offset", offset.to_string()), ("limit", limit.to_string())];
        self.get_json("purchase", "getUserPurchases", &params, false).await
    }

    /// `artist/page`: biography, top tracks, similar artists, releases.
    pub async fn artist_page(&self, artist_id: &str) -> Result<serde_json::Value> {
        self.get_json("artist", "page", &[("artist_id", artist_id.to_string())], false).await
    }

    /// `label/page`: description, founders, top artists, playlists.
    pub async fn label_page(&self, label_id: &str) -> Result<serde_json::Value> {
        self.get_json("label", "page", &[("label_id", label_id.to_string())], false).await
    }

    /// Label with a page of its albums.
    pub async fn label_get(&self, label_id: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [
            ("label_id", label_id.to_string()),
            ("extra", "albums".to_string()),
            ("offset", offset.to_string()),
            ("limit", limit.to_string()),
        ];
        self.get_json("label", "get", &params, false).await
    }

    /// Ids of every favourite album, track and artist of the account.
    pub async fn favorite_ids(&self) -> Result<serde_json::Value> {
        self.get_json("favorite", "getUserFavoriteIds", &[], false).await
    }

    /// Playlists the user owns or follows.
    pub async fn user_playlists(&self, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [("offset", offset.to_string()), ("limit", limit.to_string())];
        self.get_json("playlist", "getUserPlaylists", &params, false).await
    }

    /// Favourites of one `kind`: `albums`, `tracks` or `artists`.
    pub async fn user_favorites(&self, kind: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [("type", kind.to_string()), ("offset", offset.to_string()), ("limit", limit.to_string())];
        self.get_json("favorite", "getUserFavorites", &params, true).await
    }

    /// Editorial album lists (`new-releases`, `editor-picks`…).
    pub async fn featured_albums(&self, kind: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [("type", kind.to_string()), ("offset", offset.to_string()), ("limit", limit.to_string())];
        self.get_json("album", "getFeatured", &params, false).await
    }

    /// One shelf of the web player's Discover page: `discover/<endpoint>`
    /// (`qobuzissims`, `playlists`…), answering `{has_more, items}`.
    pub async fn discover(&self, endpoint: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [("offset", offset.to_string()), ("limit", limit.to_string())];
        self.get_json("discover", endpoint, &params, false).await
    }

    /// Qobuz playlists of one theme (`playlist/getTags` slug).
    pub async fn discover_playlists(&self, tag: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [("tags", tag.to_string()), ("offset", offset.to_string()), ("limit", limit.to_string())];
        self.get_json("discover", "playlists", &params, false).await
    }

    /// Playlist themes: `{tags: [{slug, name_json, …}]}`, names in every
    /// language.
    pub async fn playlist_tags(&self) -> Result<serde_json::Value> {
        self.get_json("playlist", "getTags", &[], false).await
    }

    /// Mixes made for the account (WeeklyQ…): a list of `{type, title, …}`.
    pub async fn mixes(&self) -> Result<serde_json::Value> {
        self.get_json("dynamic-tracks", "list", &[], false).await
    }

    /// One mix, with a page of its tracks.
    pub async fn mix(&self, kind: &str, offset: u32, limit: u32) -> Result<serde_json::Value> {
        let params = [
            ("type", kind.to_string()),
            ("extra", "tracks".to_string()),
            ("offset", offset.to_string()),
            ("limit", limit.to_string()),
        ];
        self.get_json("dynamic-tracks", "get", &params, false).await
    }

    /// Add (`on`) or remove a favourite; `field` is `track_ids`, `album_ids`
    /// or `artist_ids`.
    pub async fn set_favorite(&self, field: &str, id: &str, on: bool) -> Result<()> {
        let action = if on { "create" } else { "delete" };
        self.get_json("favorite", action, &[(field, id.to_string())], false).await?;
        Ok(())
    }

    /// POST without signature, as the web player sends play reports.
    async fn post_unsigned(&self, obj: &str, action: &str, body: PostBody) -> Result<serde_json::Value> {
        let req = self.http.post(format!("{}/{obj}/{action}", self.base)).headers(self.base_headers()?);
        let req = match body {
            PostBody::Form(fields) => req.form(&fields),
            PostBody::Json(v) => req.json(&v),
        };
        Self::check_json(req.send().await?).await
    }

    /// `track/reportStreamingStart`: plays that just started.
    pub async fn report_streaming_start(&self, events: &serde_json::Value) -> Result<()> {
        let fields = vec![("events", events.to_string())];
        self.post_unsigned("track", "reportStreamingStart", PostBody::Form(fields)).await?;
        Ok(())
    }

    /// `track/reportStreamingEndJson`: finished plays, with how long each
    /// was listened to. Succeeds only if Qobuz answers `status: success`.
    pub async fn report_streaming_end(&self, events: &serde_json::Value) -> Result<()> {
        let body = serde_json::json!({
            "events": events,
            "renderer_context": {"software_version": secret::software_version()},
        });
        let resp = self.post_unsigned("track", "reportStreamingEndJson", PostBody::Json(body)).await?;
        match resp.get("status").and_then(|s| s.as_str()) {
            Some("success") => Ok(()),
            other => Err(anyhow!("reportStreamingEndJson: status {other:?}")),
        }
    }

    /// Page where the user signs in; Qobuz then redirects the browser to
    /// `redirect_url?code_autorisation=…`.
    pub fn oauth_url(redirect_url: &str) -> String {
        format!(
            "{}/signin/oauth?ext_app_id={}&redirect_url={}",
            api::SITE_BASE,
            secret::app_id(),
            urlencoding::encode(redirect_url)
        )
    }

    /// Exchange an OAuth authorisation code for a user auth token.
    /// Returns (token, user id).
    pub async fn oauth_exchange(&self, code: &str) -> Result<(String, u64)> {
        let resp = self
            .http
            .get(format!("{}/oauth/callback", self.base))
            .header("X-App-Id", secret::app_id())
            .query(&[("code", code), ("private_key", secret::oauth_key().as_str())])
            .send()
            .await?;
        let resp = Self::check_json(resp).await?;
        let token = resp
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("oauth/callback: no token in response"))?;
        let user_id = resp.get("user_id").and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
        Ok((token.to_string(), user_id.unwrap_or(0)))
    }

    /// Account behind the current user token; fails if the token is no
    /// longer accepted.
    pub async fn user_info(&self) -> Result<UserInfo> {
        let resp = self
            .http
            .post(format!("{}/user/login", self.base))
            .headers(self.base_headers()?)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body("extra=partner")
            .send()
            .await?;
        let resp = Self::check_json(resp).await?;
        let user = resp.get("user").unwrap_or(&resp);
        let s = |k: &str| user.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let subscription = user
            .get("credential")
            .and_then(|c| c.get("label").or_else(|| c.get("description")))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(UserInfo {
            id: user.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
            email: s("email"),
            display_name: s("display_name"),
            subscription,
        })
    }

    /// Mint a fresh Qobuz Connect WebSocket token (qws/createToken or qws/refreshToken).
    /// These endpoints are authenticated by headers only (no request signature).
    pub async fn get_ws_token(&self, is_refresh: bool) -> Result<Option<WsToken>> {
        let action = if is_refresh { "refreshToken" } else { "createToken" };
        let resp = self
            .http
            .post(format!("{}/qws/{action}", self.base))
            .headers(self.base_headers()?)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(reqwest::header::REFERER, format!("{}/", api::PLAY_ORIGIN))
            .header(reqwest::header::ORIGIN, api::PLAY_ORIGIN)
            .body("jwt=jwt_qws")
            .send()
            .await?;
        let resp = Self::check_json(resp).await?;
        let Some(obj) = resp.get("jwt_qws") else {
            return Ok(None);
        };
        let jwt = obj.get("jwt").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let exp = obj.get("exp").and_then(|v| v.as_i64()).unwrap_or(0);
        let endpoint = obj.get("endpoint").and_then(|v| v.as_str()).unwrap_or_default();
        let endpoint = urlencoding::decode(endpoint)?.into_owned();
        let token = WsToken { jwt, exp, endpoint };
        Ok(token.is_valid().then_some(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_sorts_params_and_is_md5_hex() {
        let a = ApiClient::sign("track", "getFileUrl", &[("b", "2".into()), ("a", "1".into())]);
        let b = ApiClient::sign("track", "getFileUrl", &[("a", "1".into()), ("b", "2".into())]);
        // Same second in practice; compare only when timestamps match.
        if a.0 == b.0 {
            assert_eq!(a.1, b.1);
        }
        assert_eq!(a.1.len(), 32);
        assert!(a.1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_expiry_uses_buffer() {
        let t = WsToken { jwt: "j".into(), exp: now_unix_s() + 30, endpoint: "wss://x".into() };
        assert!(t.is_valid());
        assert!(!t.is_expired(0));
        assert!(t.is_expired(60));
    }
}
