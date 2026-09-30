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
/// App id and OAuth key of the mock web player, unlike the built-in ones.
const APP_ID: &str = "111222333";
const OAUTH_KEY: &str = "mockKey42";
const BUNDLE: &str = "c={production:{api:{appId:\"111222333\",appSecret:\"x\"}}};n.authenticate({privateKey:\"mockKey42\",code:t});c.initialization=function(){return c.initialSeed(\"MDEyMzQ1Njc4OWFiY2Rl\",window.utimezone.berlin)},c.string=1;t.default={berlin:1,timezones:[{offset:\"GMT\",name:\"UTC\"},{offset:\"GMT+02:00\",name:\"Europe/Berlin\",info:\"ZjAxMjM0NTY3ODlhYmNkZWY=\",extras:\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}]};";

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
        "maximum_sampling_rate": 96, "maximum_bit_depth": 24, "genre": {"name": "Classique"}, "tracks_count": 2,
        "label": {"id": 315932, "name": "Rise Above"},
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
    body: String,
) -> Response {
    let mut recorded = q.clone();
    if !body.is_empty() {
        recorded.insert("_body".into(), body.clone());
    }
    if let Some(app) = headers.get("X-App-Id").and_then(|v| v.to_str().ok()) {
        recorded.insert("X-App-Id".into(), app.into());
    }
    calls.lock().unwrap().push((path.clone(), recorded));
    match path.as_str() {
        "login" => return r#"<script src="/resources/9.9.9/bundle.js"></script>"#.into_response(),
        "resources/9.9.9/bundle.js" => return BUNDLE.into_response(),
        "track/getFileUrl" | "favorite/getUserFavorites" | "track/lyricsUrl" if q.get("request_sig") != Some(&signature(&path, &q)) => {
            return (StatusCode::BAD_REQUEST, r#"{"message":"Invalid Request Signature parameter (request_sig)"}"#)
                .into_response();
        }
        _ => {}
    }
    if path == "lyrics/77.json" {
        return Json(json!({"track_id": "77", "original": {"type": "lsync", "lang": "de", "lines": [
            {"line": "Aria", "start": "0", "end": "4000"}, {"line": "da capo", "start": "4000", "end": "8000"}
        ]}}))
        .into_response();
    }
    if path == "oauth/callback" {
        return if q.get("code").map(String::as_str) == Some(CODE) && q.get("private_key").map(String::as_str) == Some(OAUTH_KEY) {
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
        "artist/get" => json!({"id": 6, "name": "No Picture", "image": null,
                               "albums": {"total": 1, "items": [album_json()]}}),
        // Track 666: gone from the catalogue, still in the user's lists.
        "track/get" if q.get("track_id").map(String::as_str) == Some("666") => {
            return (StatusCode::NOT_FOUND, r#"{"status":"error","code":404,"message":"No result matching given argument"}"#)
                .into_response();
        }
        "track/getFileUrl" if q.get("track_id").map(String::as_str) == Some("666") => json!({
            "track_id": 666, "duration": 301, "sampling_rate": 44.1, "bit_depth": 16,
            "restrictions": [{"code": "SampleRestrictedByRightHolders"}]
        }),
        // Track 667: no format on offer at all.
        "track/getFileUrl" if q.get("track_id").map(String::as_str) == Some("667") => json!({
            "track_id": 667, "restrictions": [{"code": "FormatRestrictedByFormatAvailability"}]
        }),
        "track/get" => track_json(),
        "track/getFileUrl" => {
            let (fmt, khz, bits) = match q.get("format_id").map(String::as_str) {
                Some("27") => (27, 192.0, 24),
                Some("7") => (7, 96.0, 24),
                _ => (6, 44.1, 16),
            };
            json!({"url": format!("https://cdn.test/file?fmt={fmt}&etsp=4000000000&hmac=x"), "format_id": fmt,
                   "mime_type": "audio/flac", "sampling_rate": khz, "bit_depth": bits, "blob": "blob-77"})
        }
        "favorite/getUserFavorites" => match q.get("type").map(String::as_str) {
            Some("tracks") => json!({"tracks": {"total": 1, "items": [track_json()]}}),
            Some("artists") => json!({"artists": {"total": 2, "items": [
                {"id": 5, "name": "Glenn Gould", "image": {"large": "https://img/gould.jpg"}},
                {"id": 6, "name": "No Picture", "image": null}
            ]}}),
            _ => json!({"albums": {"total": 1, "items": [album_json()]}}),
        },
        "favorite/create" | "favorite/delete" => json!({"status": "success"}),
        "playlist/getUserPlaylists" => json!({"playlists": {"total": 2, "items": [
            {"id": 9, "name": "Soir", "owner": {"id": 1, "name": "Alice"}, "images300": ["https://img/p.jpg"], "tracks_count": 12},
            {"id": 10, "name": "Top 50", "owner": {"id": 412930, "name": "Qobuz"}}
        ]}}),
        "favorite/getUserFavoriteIds" => json!({"albums": ["abc"], "tracks": [77], "artists": [], "labels": []}),
        "label/get" => json!({"id": 315932, "name": "Rise Above Limited", "albums_count": 1,
                              "albums": {"offset": 0, "total": 1, "items": [album_json()]}}),
        "album/getFeatured" => json!({"albums": {"total": 0, "items": []}}),
        "track/lyricsUrl" if q.get("track_id").map(String::as_str) == Some("77") => {
            let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or_default();
            json!({"track_id": 77, "lyrics_url": format!("http://{host}/lyrics/77.json?Signature=x")})
        }
        "track/lyricsUrl" => {
            return (StatusCode::NOT_FOUND, r#"{"status":"error","code":404,"message":"Lyrics are not available for this track."}"#)
                .into_response();
        }
        "radio/track" | "radio/album" | "radio/artist" => json!({
            "title": "Aria", "images": {"large": "https://img/radio.jpg"}, "track_count": 3,
            "tracks": {"limit": 3, "items": [
                track_json(),
                {"id": 90, "title": "Prelude", "duration": 120, "rights": {"streamable": true},
                 "artists": [{"id": 8, "name": "Bach Player", "roles": ["main-artist"]}],
                 "album": {"id": "def", "title": "Preludes", "image": {"large": "https://img/def.jpg"}}},
                {"id": 91, "title": "Not here", "rights": {"streamable": false}}
            ]}
        }),
        "album/suggest" => json!({"albums": {"limit": 30, "items": [album_json()]}}),
        "genre/list" => json!({"genres": {"total": 1, "items": [{"id": 112, "name": "Pop/Rock", "slug": "pop-rock"}]}}),
        "purchase/getUserPurchases" => json!({"albums": {"offset": 0, "total": 0, "items": []}}),
        "track/reportStreamingStart" => json!({"status": "success"}),
        "track/reportStreamingEndJson" => json!({"status": "success"}),
        "dynamic-tracks/list" => json!([
            {"type": "weekly", "title": "WeeklyQ", "baseline": "Every Friday", "duration": 400,
             "images": {"large": "https://img/weekly.png"}}
        ]),
        "dynamic-tracks/get" if q.get("type").map(String::as_str) == Some("weekly") => json!({
            "type": "weekly", "title": "WeeklyQ", "track_count": 30,
            "tracks": {"offset": 0, "limit": 2, "items": [track_json()]}
        }),
        "discover/qobuzissims" => json!({"has_more": true, "items": [{
            "id": "jj5", "title": "The Source", "image": {"large": "https://img/jj5.jpg"},
            "artists": [{"id": 3, "name": "Justus Eichhorn", "roles": ["main-artist"]}],
            "dates": {"original": "2026-09-11"}, "audio_info": {"maximum_sampling_rate": 96, "maximum_bit_depth": 24},
            "rights": {"streamable": true}
        }]}),
        "playlist/getTags" => json!({"tags": [
            {"slug": "mood", "name_json": "{\"fr\":\"Humeurs\",\"en\":\"Mood\"}"},
            {"slug": "hi-res", "name_json": "{\"en\":\"Hi-Res\"}"}
        ]}),
        "discover/playlists" => json!({"has_more": false, "items": [
            {"id": 70, "name": "Warp", "owner": {"name": "Qobuz"}, "image": {"covers": ["https://img/warp.jpg"]}}
        ]}),
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
    /// The plugin's stderr (its log), kept to check what it writes there.
    log: PathBuf,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    notifications: Vec<Value>,
}

impl Host {
    fn spawn(api_base: &str) -> Self {
        let log = std::env::temp_dir().join(format!("qconnect-plugin-log-{}", uuid::Uuid::new_v4()));
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_qconnect"))
            .args(["plugin", "--api-base", api_base])
            .env("RUST_LOG", "qconnect=debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Host { child, log, stdin, lines, next_id: 0, notifications: vec![] }
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
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
    // Values checked against the web player a moment ago, but Qobuz has
    // changed them since: the first signed request is refused.
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let stale = json!({"app_id": "798273057", "secret": "abb21364945c0583309667d13ca3d93a", "oauth_key": "6lz8C03UDIC7",
                       "bundle": "/resources/1.0.0/bundle.js", "fetched_at": now});
    std::fs::write(dir.join("cache/web-config.json"), stale.to_string()).unwrap();
    let mut host = Host::spawn(&base);

    let dac_96k = json!({"device": "hw:1,0", "bit_perfect": true, "max_rate": 96000, "max_bits": 24,
                         "rates": [44100, 48000, 88200, 96000]});
    let init = host.initialize(&dir, dac_96k).await;
    assert_eq!(init["protocol"], 1);
    assert_eq!(init["plugin"]["id"], "qobuz");
    assert_eq!(init["capabilities"]["search"], true);
    assert_eq!(init["capabilities"]["remote_control"], true);
    assert_eq!(init["capabilities"]["library"], true);

    let status = host.ok("auth.status", json!({})).await;
    assert_eq!(status, json!({"state": "signed_in", "account": {"display_name": "Alice", "detail": "Premium"}}));

    let root = host.ok("browse.root", json!({})).await;
    let titles: Vec<&str> = root["sections"].as_array().unwrap().iter().map(|s| s["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Favourites", "My playlists", "For you", "Discover", "Purchases"]);
    assert_eq!(root["sections"][0]["ref"], "fav");
    let home: Vec<&str> = root["home"].as_array().unwrap().iter().map(|s| s["ref"].as_str().unwrap()).collect();
    let editorial = [
        "featured/new-releases", "discover/qobuzissims", "discover/album-of-the-week", "discover/playlists",
        "discover/most-streamed", "discover/press-awards", "discover/ideal-discography", "featured/editor-picks",
    ];
    assert_eq!(home[0], "mixes", "the account's mixes first");
    assert_eq!(home[1..], editorial, "then the discovery shelves, no library lists");
    let discover = host.ok("browse.list", json!({"ref": "discover"})).await;
    let refs: Vec<&str> = discover["items"].as_array().unwrap().iter().map(|s| s["ref"].as_str().unwrap()).collect();
    let mut with_themes = editorial.to_vec();
    with_themes.insert(4, "themes");
    with_themes.push("genres");
    assert_eq!(refs, with_themes, "playlists by theme after the Qobuz playlists");

    // Mixes made for the account, shown as playlists; each one lists its tracks.
    let mixes = host.ok("browse.list", json!({"ref": "mixes"})).await;
    assert_eq!(mixes["items"][0], json!({"ref": "mix/weekly", "kind": "playlist", "title": "WeeklyQ",
        "subtitle": "Every Friday", "duration_ms": 400_000, "art": "https://img/weekly.png", "playable": false, "browsable": true}));
    let weekly = host.ok("browse.list", json!({"ref": "mix/weekly", "offset": 0, "limit": 2})).await;
    assert_eq!(weekly["items"][0]["ref"], "track/77");
    assert_eq!(weekly["total"], 30);
    assert_eq!(weekly["has_more"], true);
    assert_eq!(host.ok("item.get", json!({"ref": "mix/weekly"})).await["title"], "WeeklyQ");
    assert_eq!(host.err_code("browse.list", json!({"ref": "mix/daily"})).await, -32002);

    // Discover shelves: albums and playlists in the web player's shape.
    let picks = host.ok("browse.list", json!({"ref": "discover/qobuzissims", "offset": 0, "limit": 24})).await;
    assert_eq!(picks["items"][0]["ref"], "album/jj5");
    assert_eq!(picks["items"][0]["subtitle"], "Justus Eichhorn · 2026");
    assert_eq!(picks["items"][0]["format"]["sample_rate"], 96000);
    assert_eq!(picks["has_more"], true);
    let lists = host.ok("browse.list", json!({"ref": "discover/playlists"})).await;
    assert_eq!(lists["items"][0]["ref"], "playlist/70");
    assert_eq!(lists["items"][0]["art"], "https://img/warp.jpg");
    assert_eq!(lists["has_more"], false);

    // Playlists by theme, named in the host's language (`fr-FR`).
    let themes = host.ok("browse.list", json!({"ref": "themes"})).await;
    assert_eq!(themes["items"][0], json!({"ref": "theme/mood", "kind": "folder", "title": "Humeurs",
        "playable": false, "browsable": true}));
    assert_eq!(themes["items"][1]["title"], "Hi-Res", "no French name: English");
    let mood = host.ok("browse.list", json!({"ref": "theme/mood"})).await;
    assert_eq!(mood["items"][0]["ref"], "playlist/70");
    assert!(calls.lock().unwrap().iter().any(|(p, q)| p == "discover/playlists" && q.get("tags").map(String::as_str) == Some("mood")));
    assert_eq!(host.ok("item.get", json!({"ref": "theme/mood"})).await["title"], "Humeurs");

    // Library lists: the account's favourites and playlists.
    let albums = host.ok("library.albums", json!({"offset": 0, "limit": 200})).await;
    assert_eq!(albums["items"][0]["ref"], "album/abc");
    assert_eq!(albums["items"][0]["artist"], "Glenn Gould");
    assert_eq!(albums["items"][0]["year"], 1982);
    assert_eq!(albums["items"][0]["art"], "https://img/abc.jpg");
    assert_eq!(albums["items"][0]["track_count"], 2);
    assert_eq!(albums["has_more"], false);
    let artists = host.ok("library.artists", json!({"offset": 0, "limit": 200})).await;
    let mut first = artists["items"][0].clone();
    first.as_object_mut().unwrap().remove("favorite");
    assert_eq!(first, json!({"ref": "artist/5", "kind": "artist", "title": "Glenn Gould",
        "art": "https://img/gould.jpg", "playable": false, "browsable": true,
        "actions": [{"id": "radio", "label": "Radio à partir de cet artiste", "ref": "radio/artist/5", "kind": "play"}]}));
    assert_eq!(artists["items"][1]["art"], "https://img/abc.jpg", "no picture: an album cover stands in");
    let tracks = host.ok("library.tracks", json!({})).await;
    assert_eq!(tracks["items"][0]["ref"], "track/77");
    assert_eq!(tracks["total"], 1);
    let playlists = host.ok("library.playlists", json!({"offset": 0, "limit": 200})).await;
    assert_eq!(playlists["items"][0]["kind"], "playlist");
    assert_eq!((playlists["items"][0]["editable"].as_bool(), playlists["items"][1]["editable"].as_bool()), (Some(true), Some(false)),
        "only the account's own playlists are editable");
    assert_eq!(playlists["items"][0]["browsable"], true);
    assert_eq!(playlists["items"][0]["track_count"], 12);
    assert_eq!(host.err_code("library.genres", json!({})).await, -32601);

    let fav = host.ok("browse.list", json!({"ref": "fav"})).await;
    assert_eq!(fav["items"][0]["ref"], "fav/albums");
    let fav_albums = host.ok("browse.list", json!({"ref": "fav/albums", "offset": 0, "limit": 20})).await;
    // The first signed request (library.albums above) was refused, the
    // secret re-derived from the bundle once, and the request retried.
    let paths: Vec<String> = calls.lock().unwrap().iter().map(|(p, _)| p.clone()).collect();
    let first = paths.iter().position(|p| p == "favorite/getUserFavorites").unwrap();
    assert_eq!(paths[first..first + 4], ["favorite/getUserFavorites", "login", "resources/9.9.9/bundle.js", "favorite/getUserFavorites"]);
    assert_eq!(paths.iter().filter(|p| *p == "resources/9.9.9/bundle.js").count(), 1, "bundle fetched once");
    let cached: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("cache/web-config.json")).unwrap()).unwrap();
    assert_eq!((cached["app_id"].as_str(), cached["secret"].as_str(), cached["oauth_key"].as_str()), (Some(APP_ID), Some(SECRET), Some(OAUTH_KEY)));
    let last = calls.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.1.get("X-App-Id").map(String::as_str), Some(APP_ID), "the bundle's app id is used from then on");
    assert_eq!(fav_albums["items"][0]["ref"], "album/abc");
    assert_eq!(fav_albums["has_more"], false);
    let playlists = host.ok("browse.list", json!({"ref": "my/playlists"})).await;
    assert_eq!(playlists["items"][0]["title"], "Soir");
    assert_eq!(playlists["items"][0]["art"], "https://img/p.jpg");

    let all = host.ok("search", json!({"query": "gould"})).await;
    let kinds: Vec<&str> = all["groups"].as_array().unwrap().iter().map(|g| g["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["track", "album", "artist", "playlist"]);

    let found = host.ok("search", json!({"query": "gould", "kinds": ["album", "track"], "offset": 0, "limit": 10})).await;
    assert_eq!(found["groups"][0]["kind"], "album");
    assert_eq!(found["groups"][0]["items"][0]["subtitle"], "Glenn Gould · 1982");
    assert_eq!(found["groups"][0]["items"][0]["format"], json!({"sample_rate": 96000, "bits": 24, "codec": "flac"}));
    assert_eq!(found["groups"][1]["kind"], "track");
    assert_eq!(found["groups"][1]["items"][0]["ref"], "track/77");

    // Favourite ids are read in the background at start-up.
    let mut fav = Value::Null;
    for _ in 0..100 {
        fav = host.ok("item.get", json!({"ref": "track/77"})).await["favorite"].clone();
        if !fav.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(fav, true, "a favourite track says so");
    assert_eq!(host.ok("item.get", json!({"ref": "album/abc"})).await["favorite"], true);
    assert_eq!(host.ok("favorites.set", json!({"ref": "album/abc", "on": false})).await, Value::Null);
    assert_eq!(host.ok("item.get", json!({"ref": "album/abc"})).await["favorite"], false, "updated at once");
    host.ok("favorites.set", json!({"ref": "album/abc", "on": true})).await;

    let tracks = host.ok("browse.list", json!({"ref": "album/abc", "offset": 0, "limit": 50})).await;
    assert_eq!(tracks["total"], 2);
    assert_eq!((tracks["items"][0]["album_ref"].as_str(), tracks["items"][0]["artist_ref"].as_str()), (Some("album/abc"), Some("artist/5")));
    // Actions on items, labelled in the host's language (fr-FR).
    let actions = |item: &Value| -> Vec<(String, String, String)> {
        item["actions"].as_array().unwrap().iter()
            .map(|a| (a["id"].as_str().unwrap().into(), a["ref"].as_str().unwrap().into(), a["kind"].as_str().unwrap().into()))
            .collect()
    };
    assert_eq!(tracks["items"][0]["actions"][0]["label"], "Radio à partir de ce titre");
    assert_eq!(actions(&tracks["items"][0]), [("radio".into(), "radio/track/77".into(), "play".into())]);
    let album_item = host.ok("item.get", json!({"ref": "album/abc"})).await;
    assert_eq!(actions(&album_item), [
        ("radio".into(), "radio/album/abc".into(), "play".into()),
        ("similar".into(), "similar/abc".into(), "browse".into()),
        ("label".into(), "label/315932".into(), "browse".into()),
    ]);
    assert_eq!(album_item["actions"][2]["label"], "Label : Rise Above");

    // Radio: a playlist of close tracks; radio.next leaves out what was played.
    let radio = host.ok("browse.list", json!({"ref": "radio/track/77"})).await;
    assert_eq!(radio["total"], 3);
    assert_eq!(radio["items"][1]["artist"], "Bach Player");
    assert_eq!(host.ok("item.get", json!({"ref": "radio/track/77"})).await["title"], "Radio : Aria");
    assert_eq!(init["capabilities"]["radio"], true);
    let next = host.ok("radio.next", json!({"seed": "track/77", "exclude": [], "limit": 10})).await;
    let refs: Vec<&str> = next["items"].as_array().unwrap().iter().map(|t| t["ref"].as_str().unwrap()).collect();
    assert_eq!(refs, ["track/90"], "not the seed, not an unplayable track");
    let next = host.ok("radio.next", json!({"seed": "album/abc", "exclude": ["track/90"], "limit": 10})).await;
    let refs: Vec<&str> = next["items"].as_array().unwrap().iter().map(|t| t["ref"].as_str().unwrap()).collect();
    assert_eq!(refs, ["track/77"]);
    assert_eq!(host.err_code("radio.next", json!({"seed": "playlist/9"})).await, -32602);

    let similar = host.ok("browse.list", json!({"ref": "similar/abc"})).await;
    assert_eq!(similar["items"][0]["ref"], "album/abc");
    let genres = host.ok("browse.list", json!({"ref": "genres"})).await;
    assert_eq!(genres["items"][0], json!({"ref": "genre/112", "kind": "folder", "title": "Pop/Rock", "playable": false, "browsable": true}));
    assert_eq!(host.ok("browse.list", json!({"ref": "genre/112"})).await["items"], json!([]));
    assert_eq!(host.ok("item.get", json!({"ref": "genre/112"})).await["title"], "Pop/Rock");
    assert_eq!(host.ok("browse.list", json!({"ref": "purchases"})).await["total"], 0);

    let label = host.ok("browse.list", json!({"ref": "label/315932"})).await;
    assert_eq!(label["items"][0]["ref"], "album/abc");
    assert_eq!(host.ok("item.get", json!({"ref": "label/315932"})).await["title"], "Rise Above Limited");
    assert_eq!(tracks["items"][1]["title"], "Variation 1");
    assert_eq!(tracks["items"][1]["album"], "Goldberg Variations");
    assert_eq!(tracks["items"][1]["art"], "https://img/abc.jpg");

    let item = host.ok("item.get", json!({"ref": "track/77"})).await;
    assert_eq!(item["kind"], "track");

    assert_eq!(init["capabilities"]["lyrics"], true);
    let lyrics = host.ok("lyrics.get", json!({"ref": "track/77"})).await;
    assert_eq!(lyrics, json!({"synced": [{"time_ms": 0, "text": "Aria"}, {"time_ms": 4000, "text": "da capo"}]}));
    assert_eq!(host.err_code("lyrics.get", json!({"ref": "track/78"})).await, -32002);
    let asked = |calls: &Calls| calls.lock().unwrap().iter().filter(|(p, q)| p == "track/lyricsUrl" && q["track_id"] == "78").count();
    assert_eq!(host.err_code("lyrics.get", json!({"ref": "track/78"})).await, -32002);
    assert_eq!(asked(&calls), 1, "no lyrics: remembered");
    assert_eq!(host.err_code("lyrics.get", json!({"ref": "album/abc"})).await, -32002);
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

    // A track Qobuz no longer streams: `unavailable` with Qobuz's reason,
    // after one request (no lower quality would be served either).
    format_ids(&calls);
    let refused = host.call("track.resolve", json!({"ref": "track/666", "purpose": "play"})).await;
    assert_eq!(refused["error"]["code"], -32003, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("no longer in the Qobuz catalogue") && message.contains("SampleRestrictedByRightHolders"), "{message}");
    assert_eq!(refused["error"]["data"]["restrictions"], json!(["SampleRestrictedByRightHolders"]));
    assert_eq!(format_ids(&calls).len(), 1);
    // A format restriction: each lower quality is tried before giving up.
    assert_eq!(host.err_code("track.resolve", json!({"ref": "track/667", "purpose": "play"})).await, -32003);
    assert!(format_ids(&calls).len() > 1);

    // A DAC listing only 48 kHz: nothing fits.
    host.send(json!({"jsonrpc": "2.0", "method": "output.changed", "params": {"output": {"max_rate": 48000, "rates": [48000]}}})).await;
    assert_eq!(host.err_code("track.resolve", json!({"ref": "track/77", "purpose": "play"})).await, -32003);

    assert_eq!(host.ok("favorites.set", json!({"ref": "album/abc", "on": true})).await, Value::Null);
    assert!(calls.lock().unwrap().iter().any(|(p, q)| p == "favorite/create" && q.get("album_ids").map(String::as_str) == Some("abc")));

    assert_eq!(host.err_code("browse.list", json!({"ref": "radio/1"})).await, -32002);
    assert_eq!(host.err_code("track.resolve", json!({"ref": "album/abc"})).await, -32002);
    assert_eq!(host.err_code("search", json!({"nope": 1})).await, -32602);
    assert_eq!(host.err_code("player.fly", json!({})).await, -32601);

    assert!(!host.logs().contains("a@b.c"), "the account e-mail must never reach the log");
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sign_in_and_out() {
    let (base, calls) = start_mock().await;
    let dir = data_dir(None);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;
    // No cache: the web player's values are read at start-up.
    let cache = dir.join("cache/web-config.json");
    for _ in 0..100 {
        if cache.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(cache.exists(), "web player config read at start-up");

    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "signed_out"}));
    assert_eq!(host.err_code("search", json!({"query": "x"})).await, -32001);

    // Browser on this machine: the loopback listener catches the redirect.
    let begin = host.ok("auth.begin", json!({})).await;
    assert_eq!(begin["expects_input"], true);
    let url = begin["url"].as_str().unwrap();
    assert!(url.contains("/signin/oauth?") && url.contains(&format!("ext_app_id={APP_ID}")), "{url}");
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
    let callbacks: Vec<_> = calls.lock().unwrap().iter().filter(|(p, _)| p == "oauth/callback").map(|(_, q)| q.get("X-App-Id").cloned()).collect();
    assert_eq!(callbacks, [Some(APP_ID.to_string()), Some(APP_ID.to_string())], "codes traded with the bundle's app id and key");

    assert!(!host.logs().contains("a@b.c"), "the account e-mail must never reach the log");
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn settings_and_play_reports() {
    let (base, calls) = start_mock().await;
    let dir = data_dir(Some(TOKEN));
    let mut host = Host::spawn(&base);
    let init = host.initialize(&dir, json!({})).await;
    assert_eq!(init["capabilities"]["reporting"], true);
    let declared: Vec<(&str, &str, bool)> = init["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["key"].as_str().unwrap(), s["label"].as_str().unwrap(), s["default"].as_bool().unwrap()))
        .collect();
    assert_eq!(declared, [("report_playback", "Signaler les écoutes à Qobuz", true), ("cmaf", "Lecture chiffrée (CMAF)", false)]);

    let wait_for = |path: &'static str, calls: Calls| async move {
        for _ in 0..100 {
            if let Some(q) = calls.lock().unwrap().iter().rev().find(|(p, _)| p == path).map(|(_, q)| q.clone()) {
                return Some(q);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    };
    let notify = |method: &str, params: Value| json!({"jsonrpc": "2.0", "method": method, "params": params});

    // Reports on by default: start, then end with the blob of the stream.
    host.ok("track.resolve", json!({"ref": "track/77", "purpose": "play"})).await;
    host.send(notify("playback.started", json!({"ref": "track/77"}))).await;
    let start = wait_for("track/reportStreamingStart", calls.clone()).await.expect("start reported");
    let form: HashMap<String, String> = start["_body"]
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), urlencoding::decode(&v.replace('+', " ")).unwrap().into_owned()))
        .collect();
    let events: Value = serde_json::from_str(&form["events"]).unwrap();
    assert_eq!((events[0]["track_id"].as_u64(), events[0]["user_id"].as_u64()), (Some(77), Some(1)));
    assert!(events[0]["format_id"].is_number());
    host.send(notify("playback.progress", json!({"ref": "track/77", "pos_ms": 30_000}))).await;
    host.send(notify("playback.ended", json!({"ref": "track/77", "listened_ms": 120_500, "reason": "finished"}))).await;
    let end = wait_for("track/reportStreamingEndJson", calls.clone()).await.expect("end reported");
    let body: Value = serde_json::from_str(&end["_body"]).unwrap();
    assert_eq!(body["events"][0]["blob"], "blob-77");
    assert_eq!(body["events"][0]["duration"], 120);
    assert!(body["renderer_context"]["software_version"].as_str().unwrap().starts_with("wp-"));
    assert!(!dir.join("play-reports.json").exists(), "sent, so not kept");

    // Turned off: nothing more goes out.
    calls.lock().unwrap().clear();
    host.send(notify("settings.changed", json!({"settings": {"report_playback": false, "cmaf": false}}))).await;
    host.ok("track.resolve", json!({"ref": "track/77", "purpose": "play"})).await;
    host.send(notify("playback.started", json!({"ref": "track/77"}))).await;
    host.send(notify("playback.ended", json!({"ref": "track/77", "listened_ms": 60_000, "reason": "finished"}))).await;
    host.ok("auth.status", json!({})).await; // a round trip after the notifications
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reported = calls.lock().unwrap().iter().any(|(p, _)| p.starts_with("track/reportStreaming"));
    assert!(!reported, "reports turned off");

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn refused_token_reports_expiry() {
    let (base, _calls) = start_mock().await;
    let dir = data_dir(Some("stale-token"));
    let mut host = Host::spawn(&base);
    host.initialize(&dir, json!({})).await;

    let expired = host.ok("auth.status", json!({})).await;
    assert_eq!(expired, json!({"state": "expired", "account": {"display_name": "Alice"}}));
    assert_eq!(host.err_code("browse.list", json!({"ref": "fav/tracks"})).await, -32001);
    assert_eq!(host.notification("auth.changed").await["params"], json!({"state": "expired"}));

    assert!(!host.logs().contains("a@b.c"), "the account e-mail must never reach the log");
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
