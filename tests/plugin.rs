//! Fake plugin host: runs `qconnect plugin` against a mock Qobuz API and
//! checks the JSON-RPC exchanges.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use md5::Digest;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

type Calls = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

const TOKEN: &str = "good-token";
const CODE: &str = "c0de";
/// Secret the mock API expects, different from the built-in one: the
/// plugin must re-derive it from the mock web player's bundle.
const SECRET: &str = "0123456789abcdef0123456789abcdef";
const BUNDLE: &str = "c.initialization=function(){return c.initialSeed(\"MDEyMzQ1Njc4OWFiY2Rl\",window.utimezone.berlin)},c.string=1;t.default={berlin:1,timezones:[{offset:\"GMT\",name:\"UTC\"},{offset:\"GMT+02:00\",name:\"Europe/Berlin\",info:\"ZjAxMjM0NTY3ODlhYmNkZWY=\",extras:\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}]};";

/// `request_sig` as the web player computes it.
fn signature(obj_action: &str, q: &HashMap<String, String>) -> String {
    let mut keys: Vec<_> = q.keys().filter(|k| !k.starts_with("request_")).collect();
    keys.sort();
    let mut raw = obj_action.replace('/', "");
    for k in keys {
        raw.push_str(k);
        raw.push_str(&q[k]);
    }
    raw.push_str(q.get("request_ts").map_or("", String::as_str));
    raw.push_str(SECRET);
    format!("{:x}", md5::Md5::digest(raw.as_bytes()))
}

fn album_json() -> Value {
    json!({
        "id": "abc", "title": "Goldberg Variations", "artist": {"id": 5, "name": "Glenn Gould"},
        "release_date_original": "1982-01-01", "image": {"large": "https://img/abc.jpg"},
        "maximum_sampling_rate": 96, "maximum_bit_depth": 24, "genre": {"name": "Classique"},
        "tracks": {"offset": 0, "total": 2, "items": [
            {"id": 77, "title": "Aria", "duration": 185, "track_number": 1, "media_number": 1},
            {"id": 78, "title": "Variation 1", "duration": 60, "track_number": 2, "media_number": 1}
        ]}
    })
}

fn track_json() -> Value {
    json!({
        "id": 77, "title": "Aria", "duration": 185, "track_number": 1, "performer": {"name": "Glenn Gould"},
        "album": {"id": "abc", "title": "Goldberg Variations", "artist": {"name": "Glenn Gould"}},
        "audio_info": {"replaygain_track_gain": -3.5, "replaygain_track_peak": 0.9}
    })
}

async fn mock(
    State(calls): State<Calls>,
    UrlPath(path): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    calls.lock().unwrap().push((path.clone(), q.clone()));
    match path.as_str() {
        "login" => return r#"<script src="/resources/9.9.9/bundle.js"></script>"#.into_response(),
        "resources/9.9.9/bundle.js" => return BUNDLE.into_response(),
        "track/getFileUrl" | "favorite/getUserFavorites" if q.get("request_sig") != Some(&signature(&path, &q)) => {
            return (StatusCode::BAD_REQUEST, r#"{"message":"Invalid Request Signature parameter (request_sig)"}"#)
                .into_response();
        }
        _ => {}
    }
    if path == "oauth/callback" {
        return if q.get("code").map(String::as_str) == Some(CODE) {
            Json(json!({"token": TOKEN, "user_id": 1})).into_response()
        } else {
            (StatusCode::BAD_REQUEST, "bad code").into_response()
        };
    }
    if headers.get("X-User-Auth-Token").and_then(|v| v.to_str().ok()) != Some(TOKEN) {
        return (StatusCode::UNAUTHORIZED, "{\"message\":\"User authentication is required.\"}").into_response();
    }
    let body = match path.as_str() {
        "user/login" => json!({"user": {"id": 1, "email": "a@b.c", "display_name": "Alice", "credential": {"label": "Premium"}}}),
        "session/start" => json!({"session_id": "sess", "expires_at": 4_000_000_000u64}),
        "catalog/search" => json!({
            "albums": {"total": 1, "items": [album_json()]},
            "tracks": {"total": 1, "items": [track_json()]},
            "artists": {"total": 0, "items": []}
        }),
        "album/get" => album_json(),
        "track/get" => track_json(),
        "track/getFileUrl" => {
            let (fmt, khz, bits) = match q.get("format_id").map(String::as_str) {
                Some("27") => (27, 192.0, 24),
                Some("7") => (7, 96.0, 24),
                _ => (6, 44.1, 16),
            };
            json!({"url": format!("https://cdn.test/file?fmt={fmt}&etsp=4000000000&hmac=x"), "format_id": fmt,
                   "mime_type": "audio/flac", "sampling_rate": khz, "bit_depth": bits})
        }
        "favorite/getUserFavorites" => json!({"albums": {"total": 1, "items": [album_json()]}}),
        "favorite/create" | "favorite/delete" => json!({"status": "success"}),
        "playlist/getUserPlaylists" => json!({"playlists": {"total": 1, "items": [
            {"id": 9, "name": "Soir", "owner": {"name": "Alice"}, "images300": ["https://img/p.jpg"]}
        ]}}),
        "album/getFeatured" => json!({"albums": {"total": 0, "items": []}}),
        _ => return (StatusCode::NOT_FOUND, "no such endpoint").into_response(),
    };
    Json(body).into_response()
}

async fn start_mock() -> (String, Calls) {
    let calls = Calls::default();
    let app = Router::new().route("/{*path}", any(mock)).with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, calls)
}

struct Host {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    notifications: Vec<Value>,
}

impl Host {
    fn spawn(api_base: &str) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_qconnect"))
            .args(["plugin", "--api-base", api_base])
            .env("RUST_LOG", "qconnect=debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Host { child, stdin, lines, next_id: 0, notifications: vec![] }
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("plugin answered in time")
            .unwrap()
            .expect("plugin still running");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }

    /// Whole response (`result` or `error`) to one request.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let msg = self.read().await;
            if msg.get("id") == Some(&json!(id)) {
                assert_eq!(msg["jsonrpc"], "2.0");
                return msg;
            }
            assert!(msg.get("method").is_some(), "unexpected message {msg}");
            self.notifications.push(msg);
        }
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let msg = self.call(method, params).await;
        assert!(msg.get("error").is_none(), "{method} failed: {msg}");
        msg["result"].clone()
    }

    async fn err_code(&mut self, method: &str, params: Value) -> i64 {
        let msg = self.call(method, params).await;
        msg["error"]["code"].as_i64().unwrap_or_else(|| panic!("{method} should fail: {msg}"))
    }

    async fn notification(&mut self, method: &str) -> Value {
        if let Some(i) = self.notifications.iter().position(|n| n["method"] == method) {
            return self.notifications.remove(i);
        }
        loop {
            let msg = self.read().await;
            if msg["method"] == method {
                return msg;
            }
            self.notifications.push(msg);
        }
    }

    async fn initialize(&mut self, data_dir: &Path, output: Value) -> Value {
        self.ok(
            "initialize",
            json!({"protocol": 1, "host": {"name": "fake", "version": "0"}, "data_dir": data_dir,
                   "cache_dir": data_dir.join("cache"), "locale": "fr-FR", "output": output}),
        )
        .await
    }

    async fn shutdown(mut self) {
        assert_eq!(self.ok("shutdown", Value::Null).await, Value::Null);
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await.unwrap().unwrap();
        assert!(status.success());
    }
}

fn data_dir(token: Option<&str>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qconnect-plugin-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(token) = token {
        let path = dir.join("account.json");
        let creds = json!({"user_id": 1, "email": "a@b.c", "display_name": "Alice", "token": token});
        std::fs::write(&path, creds.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    dir
}

fn format_ids(calls: &Calls) -> Vec<String> {
    let calls = std::mem::take(&mut *calls.lock().unwrap());
    calls.into_iter().filter(|(p, _)| p == "track/getFileUrl").map(|(_, q)| q["format_id"].clone()).collect()
}

#[tokio::test]
async fn catalogue_and_resolve() {
    let (base, calls) = start_mock().await;
    let dir = data_dir(Some(TOKEN));
    let mut host = Host::spawn(&base);

    let dac_96k = json!({"device": "hw:1,0", "bit_perfect": true, "max_rate": 96000, "max_bits": 24,
                         "rates": [44100, 48000, 88200, 96000]});
    let init = host.initialize(&dir, dac_96k).await;
    assert_eq!(init["protocol"], 1);
    assert_eq!(init["plugin"]["id"], "qobuz");
    assert_eq!(init["capabilities"]["search"], true);
    assert_eq!(init["capabilities"]["remote_control"], true);

    let status = host.ok("auth.status", json!({})).await;
    assert_eq!(status, json!({"state": "signed_in", "account": {"display_name": "Alice", "detail": "Premium"}}));

    let root = host.ok("browse.root", json!({})).await;
    let titles: Vec<&str> = root["sections"].as_array().unwrap().iter().map(|s| s["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Favourites", "My playlists", "New releases", "Qobuz selection"]);
    assert_eq!(root["sections"][0]["ref"], "fav");

    let fav = host.ok("browse.list", json!({"ref": "fav"})).await;
    assert_eq!(fav["items"][0]["ref"], "fav/albums");
    // First signed request: refused, secret re-derived from the bundle, retried.
    let fav_albums = host.ok("browse.list", json!({"ref": "fav/albums", "offset": 0, "limit": 20})).await;
    let paths: Vec<String> = calls.lock().unwrap().iter().map(|(p, _)| p.clone()).collect();
    let tail: Vec<&str> = paths.iter().rev().take(4).rev().map(String::as_str).collect();
    assert_eq!(tail, ["favorite/getUserFavorites", "login", "resources/9.9.9/bundle.js", "favorite/getUserFavorites"]);
    assert!(dir.join("cache/web-secret.json").exists(), "re-derived secret cached");
    assert_eq!(fav_albums["items"][0]["ref"], "album/abc");
    assert_eq!(fav_albums["has_more"], false);
    let playlists = host.ok("browse.list", json!({"ref": "my/playlists"})).await;
    assert_eq!(playlists["items"][0]["title"], "Soir");
    assert_eq!(playlists["items"][0]["art"], "https://img/p.jpg");

    let found = host.ok("search", json!({"query": "gould", "kinds": ["album", "track"], "offset": 0, "limit": 10})).await;
    assert_eq!(found["groups"][0]["kind"], "album");
    assert_eq!(found["groups"][0]["items"][0]["subtitle"], "Glenn Gould · 1982");
    assert_eq!(found["groups"][0]["items"][0]["format"], json!({"sample_rate": 96000, "bits": 24, "codec": "flac"}));
    assert_eq!(found["groups"][1]["kind"], "track");
    assert_eq!(found["groups"][1]["items"][0]["ref"], "track/77");

    let tracks = host.ok("browse.list", json!({"ref": "album/abc", "offset": 0, "limit": 50})).await;
    assert_eq!(tracks["total"], 2);
    assert_eq!(tracks["items"][1]["title"], "Variation 1");
    assert_eq!(tracks["items"][1]["album"], "Goldberg Variations");
    assert_eq!(tracks["items"][1]["art"], "https://img/abc.jpg");

    let item = host.ok("item.get", json!({"ref": "track/77"})).await;
    assert_eq!(item["kind"], "track");
    assert_eq!(item["duration_ms"], 185_000);

    format_ids(&calls);
    let r = host.ok("track.resolve", json!({"ref": "track/77", "purpose": "play"})).await;
    assert_eq!(format_ids(&calls), ["7"], "96 kHz DAC: hires-96 requested first");
    assert_eq!(r["url"], "https://cdn.test/file?fmt=7&etsp=4000000000&hmac=x");
    assert_eq!(r["expires_at"], 4_000_000_000i64 - 60);
    assert_eq!(r["format"], json!({"sample_rate": 96000, "bits": 24, "channels": 2, "codec": "flac"}));
    assert_eq!(r["duration_ms"], 185_000);
    assert_eq!(r["replaygain"], json!({"track_gain": -3.5, "track_peak": 0.9}));

    // Switch to a 48 kHz output: CD quality only.
    host.send(json!({"jsonrpc": "2.0", "method": "output.changed", "params": {"max_rate": 48000, "max_bits": 24}})).await;
    let r = host.ok("track.resolve", json!({"ref": "track/77", "purpose": "preload"})).await;
    assert_eq!(format_ids(&calls), ["6"]);
    assert_eq!(r["format"]["sample_rate"], 44100);

    // A DAC listing only 48 kHz: nothing fits.
    host.send(json!({"jsonrpc": "2.0", "method": "output.changed", "params": {"output": {"max_rate": 48000, "rates": [48000]}}})).await;
    assert_eq!(host.err_code("track.resolve", json!({"ref": "track/77", "purpose": "play"})).await, -32003);

    assert_eq!(host.ok("favorites.set", json!({"ref": "album/abc", "on": true})).await, Value::Null);
    assert!(calls.lock().unwrap().iter().any(|(p, q)| p == "favorite/create" && q.get("album_ids").map(String::as_str) == Some("abc")));

    assert_eq!(host.err_code("browse.list", json!({"ref": "radio/1"})).await, -32002);
    assert_eq!(host.err_code("track.resolve", json!({"ref": "album/abc"})).await, -32002);
    assert_eq!(host.err_code("search", json!({"nope": 1})).await, -32602);
    assert_eq!(host.err_code("player.fly", json!({})).await, -32601);

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sign_in_and_out() {
    let (base, _calls) = start_mock().await;
    let dir = data_dir(None);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;

    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "signed_out"}));
    assert_eq!(host.err_code("search", json!({"query": "x"})).await, -32001);

    // Browser on this machine: the loopback listener catches the redirect.
    let begin = host.ok("auth.begin", json!({})).await;
    assert_eq!(begin["expects_input"], true);
    let url = begin["url"].as_str().unwrap();
    assert!(url.contains("/signin/oauth?"), "{url}");
    let redirect = url.split("redirect_url=").nth(1).unwrap();
    let redirect = urlencoding::decode(redirect).unwrap().into_owned();
    assert!(redirect.starts_with("http://localhost:"), "{redirect}");
    let page = reqwest::get(format!("{redirect}?code_autorisation={CODE}")).await.unwrap();
    assert!(page.status().is_success());
    let changed = host.notification("auth.changed").await;
    assert_eq!(changed["params"]["state"], "signed_in");
    assert!(dir.join("account.json").exists());
    assert_eq!(host.ok("search", json!({"query": "x", "kinds": ["artist"]})).await["groups"][0]["kind"], "artist");

    assert_eq!(host.ok("auth.sign_out", json!({})).await["state"], "signed_out");
    assert!(!dir.join("account.json").exists());
    assert_eq!(host.err_code("item.get", json!({"ref": "track/77"})).await, -32001);

    // Browser elsewhere: the user pastes the address.
    host.ok("auth.begin", json!({})).await;
    assert_eq!(host.err_code("auth.complete", json!({"input": "https://www.qobuz.com/signin"})).await, -32602);
    let status = host.ok("auth.complete", json!({"input": format!("http://localhost/login/callback?code_autorisation={CODE}")})).await;
    assert_eq!(status["state"], "signed_in");
    assert_eq!(status["account"]["display_name"], "Alice");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn refused_token_reports_expiry() {
    let (base, _calls) = start_mock().await;
    let dir = data_dir(Some("stale-token"));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;

    assert_eq!(host.ok("auth.status", json!({})).await["state"], "expired");
    assert_eq!(host.err_code("browse.list", json!({"ref": "fav/tracks"})).await, -32001);
    assert_eq!(host.notification("auth.changed").await["params"], json!({"state": "expired"}));

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
