//! Protocol `ref`s and conversion of Qobuz API objects into protocol items.
//!
//! Refs: `track/<id>`, `album/<id>`, `artist/<id>`, `playlist/<id>` for
//! catalogue entries; `mix/<type>` for the account's mixes (WeeklyQ…);
//! `fav`, `fav/albums`, `fav/tracks`, `fav/artists`, `my/playlists`,
//! `mixes`, `discover`, `featured/<type>`, `discover/<shelf>`, `themes` and
//! `theme/<tag>` (playlists by theme) for sections.

use serde::Serialize;
use serde_json::{json, Value};

/// Editorial album lists offered as sections.
pub const FEATURED: [&str; 2] = ["new-releases", "editor-picks"];

/// Editorial shelves of the web player's Discover page (`discover/*`).
pub const DISCOVER: [&str; 6] =
    ["qobuzissims", "album-of-the-week", "playlists", "most-streamed", "press-awards", "ideal-discography"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    Track(String),
    Album(String),
    Artist(String),
    Playlist(String),
    Mix(String),
    Favorites,
    FavAlbums,
    FavTracks,
    FavArtists,
    MyPlaylists,
    Mixes,
    Discover,
    Featured(String),
    DiscoverShelf(String),
    Themes,
    Theme(String),
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Ref {
    pub fn parse(s: &str) -> Option<Self> {
        let r = match s {
            "fav" => Ref::Favorites,
            "fav/albums" => Ref::FavAlbums,
            "fav/tracks" => Ref::FavTracks,
            "fav/artists" => Ref::FavArtists,
            "my/playlists" => Ref::MyPlaylists,
            "mixes" => Ref::Mixes,
            "discover" => Ref::Discover,
            "themes" => Ref::Themes,
            _ => {
                let (kind, id) = s.split_once('/')?;
                let id = id.to_string();
                match kind {
                    "featured" if FEATURED.contains(&id.as_str()) => Ref::Featured(id),
                    "discover" if DISCOVER.contains(&id.as_str()) => Ref::DiscoverShelf(id),
                    "mix" if valid_id(&id) => Ref::Mix(id),
                    "theme" if valid_id(&id) => Ref::Theme(id),
                    "track" if valid_id(&id) => Ref::Track(id),
                    "album" if valid_id(&id) => Ref::Album(id),
                    "artist" if valid_id(&id) => Ref::Artist(id),
                    "playlist" if valid_id(&id) => Ref::Playlist(id),
                    _ => return None,
                }
            }
        };
        Some(r)
    }
}

impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Track(id) => write!(f, "track/{id}"),
            Ref::Album(id) => write!(f, "album/{id}"),
            Ref::Artist(id) => write!(f, "artist/{id}"),
            Ref::Playlist(id) => write!(f, "playlist/{id}"),
            Ref::Mix(kind) => write!(f, "mix/{kind}"),
            Ref::Favorites => f.write_str("fav"),
            Ref::FavAlbums => f.write_str("fav/albums"),
            Ref::FavTracks => f.write_str("fav/tracks"),
            Ref::FavArtists => f.write_str("fav/artists"),
            Ref::MyPlaylists => f.write_str("my/playlists"),
            Ref::Mixes => f.write_str("mixes"),
            Ref::Discover => f.write_str("discover"),
            Ref::Featured(kind) => write!(f, "featured/{kind}"),
            Ref::DiscoverShelf(shelf) => write!(f, "discover/{shelf}"),
            Ref::Themes => f.write_str("themes"),
            Ref::Theme(tag) => write!(f, "theme/{tag}"),
        }
    }
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
pub struct Format {
    pub sample_rate: u32,
    pub bits: u32,
    pub codec: &'static str,
}

#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct Item {
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_no: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_no: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    pub playable: bool,
    pub browsable: bool,
}

impl Item {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("item serialises")
    }
}

fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Ids come as numbers (tracks, artists, playlists) or strings (albums).
fn id_of(v: &Value) -> Option<String> {
    match v.get("id")? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

fn title_with_version(v: &Value, key: &str) -> String {
    let title = str_at(v, &[key]).unwrap_or_else(|| "?".into());
    match str_at(v, &["version"]) {
        Some(version) => format!("{title} ({version})"),
        None => title,
    }
}

fn year_of(album: &Value) -> Option<u32> {
    if let Some(date) = str_at(album, &["release_date_original"]).or_else(|| str_at(album, &["dates", "original"])) {
        return date.get(..4)?.parse().ok();
    }
    let ts = album.get("released_at")?.as_i64()?;
    // Days → civil year is overkill here: 365.2425-day years are exact enough
    // away from New Year's Eve.
    Some((1970.0 + ts as f64 / 31_556_952.0).floor() as u32)
}

fn cover_of(album: &Value) -> Option<String> {
    str_at(album, &["image", "large"])
        .or_else(|| str_at(album, &["image", "small"]))
        .or_else(|| str_at(album, &["image", "thumbnail"]))
}

/// Best format on offer, from `maximum_sampling_rate` (kHz) and
/// `maximum_bit_depth`, at the top level or under `audio_info` (`discover/*`).
fn format_of(v: &Value) -> Option<Format> {
    let v = if v.get("maximum_sampling_rate").is_some() { v } else { v.get("audio_info")? };
    let khz = v.get("maximum_sampling_rate")?.as_f64()?;
    let bits = v.get("maximum_bit_depth")?.as_u64()?;
    Some(Format { sample_rate: (khz * 1000.0).round() as u32, bits: bits as u32, codec: "flac" })
}

fn join(parts: &[Option<&str>]) -> Option<String> {
    let parts: Vec<&str> = parts.iter().flatten().copied().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A track; `album` supplies what tracks listed inside an album omit.
pub fn track(v: &Value, album: Option<&Value>) -> Option<Item> {
    let id = id_of(v)?;
    let album = v.get("album").or(album);
    let album_title = album.and_then(|a| str_at(a, &["title"]));
    let album_artist = album.and_then(|a| str_at(a, &["artist", "name"]));
    let artist = str_at(v, &["performer", "name"])
        .or_else(|| str_at(v, &["artist", "name"]))
        .or_else(|| album_artist.clone());
    Some(Item {
        reference: Ref::Track(id).to_string(),
        kind: "track",
        title: title_with_version(v, "title"),
        subtitle: join(&[artist.as_deref(), album_title.as_deref()]),
        artist,
        album: album_title,
        album_artist,
        track_no: v.get("track_number").and_then(Value::as_u64),
        disc_no: v.get("media_number").and_then(Value::as_u64),
        year: album.and_then(year_of),
        genre: album.and_then(|a| str_at(a, &["genre", "name"])),
        duration_ms: v.get("duration").and_then(Value::as_u64).map(|s| s * 1000),
        art: album.and_then(cover_of),
        format: format_of(v).or_else(|| album.and_then(format_of)),
        playable: v.get("streamable").and_then(Value::as_bool).unwrap_or(true),
        browsable: false,
    })
}

/// Main artist: `artist`, or in `discover/*` answers the `artists` list,
/// where the main artist carries the `main-artist` role.
fn album_artist_of(v: &Value) -> Option<String> {
    str_at(v, &["artist", "name"]).or_else(|| {
        let artists = v.get("artists")?.as_array()?;
        let main = |a: &&Value| a.get("roles").and_then(Value::as_array).is_some_and(|r| r.iter().any(|r| r == "main-artist"));
        artists.iter().find(main).or(artists.first()).and_then(|a| str_at(a, &["name"]))
    })
}

fn streamable(v: &Value) -> bool {
    v.get("streamable").or_else(|| v.pointer("/rights/streamable")).and_then(Value::as_bool).unwrap_or(true)
}

pub fn album(v: &Value) -> Option<Item> {
    let id = id_of(v)?;
    let artist = album_artist_of(v);
    let year = year_of(v);
    let year_s = year.map(|y| y.to_string());
    Some(Item {
        reference: Ref::Album(id).to_string(),
        kind: "album",
        title: title_with_version(v, "title"),
        subtitle: join(&[artist.as_deref(), year_s.as_deref()]),
        album_artist: artist.clone(),
        artist,
        year,
        genre: str_at(v, &["genre", "name"]),
        duration_ms: v.get("duration").and_then(Value::as_u64).map(|s| s * 1000),
        art: cover_of(v),
        format: format_of(v),
        playable: streamable(v),
        browsable: true,
        ..Default::default()
    })
}

pub fn artist(v: &Value) -> Option<Item> {
    let id = id_of(v)?;
    let art = str_at(v, &["image", "large"])
        .or_else(|| str_at(v, &["image", "medium"]))
        .or_else(|| str_at(v, &["picture"]));
    Some(Item {
        reference: Ref::Artist(id).to_string(),
        kind: "artist",
        title: str_at(v, &["name"]).unwrap_or_else(|| "?".into()),
        art,
        browsable: true,
        ..Default::default()
    })
}

pub fn playlist(v: &Value) -> Option<Item> {
    let id = id_of(v)?;
    let first = |key: &str| v.get(key)?.as_array()?.iter().find_map(|u| u.as_str()).map(str::to_string);
    Some(Item {
        reference: Ref::Playlist(id).to_string(),
        kind: "playlist",
        title: str_at(v, &["name"]).unwrap_or_else(|| "?".into()),
        subtitle: str_at(v, &["owner", "name"]),
        duration_ms: v.get("duration").and_then(Value::as_u64).map(|s| s * 1000),
        art: first("images300")
            .or_else(|| first("image_rectangle"))
            .or_else(|| first("images"))
            // `discover/playlists`: `image: {covers: [...], rectangle}`.
            .or_else(|| v.pointer("/image/covers")?.as_array()?.iter().find_map(|u| u.as_str()).map(str::to_string))
            .or_else(|| str_at(v, &["image", "rectangle"])),
        browsable: true,
        ..Default::default()
    })
}

/// A mix made for the account (`dynamic-tracks/list` entry: WeeklyQ…),
/// shown as a playlist.
pub fn mix(v: &Value) -> Option<Item> {
    let kind = str_at(v, &["type"]).filter(|t| valid_id(t))?;
    Some(Item {
        reference: Ref::Mix(kind).to_string(),
        kind: "playlist",
        title: str_at(v, &["title"]).unwrap_or_else(|| "?".into()),
        subtitle: str_at(v, &["baseline"]),
        duration_ms: v.get("duration").and_then(Value::as_u64).map(|s| s * 1000),
        art: str_at(v, &["images", "large"]).or_else(|| str_at(v, &["images", "small"])),
        browsable: true,
        ..Default::default()
    })
}

/// A playlist theme (`playlist/getTags` entry) as a folder of playlists,
/// named in `lang` (English, then the slug, when Qobuz has no such name).
pub fn theme(v: &Value, lang: &str) -> Option<Item> {
    let slug = str_at(v, &["slug"]).filter(|s| valid_id(s))?;
    let names: Value = str_at(v, &["name_json"]).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    let title = str_at(&names, &[lang]).or_else(|| str_at(&names, &["en"])).unwrap_or_else(|| slug.clone());
    Some(folder(&Ref::Theme(slug), &title))
}

/// A section: a folder the host lists under the plugin's name.
pub fn folder(r: &Ref, title: &str) -> Item {
    Item { reference: r.to_string(), kind: "folder", title: title.to_string(), browsable: true, ..Default::default() }
}

/// `{items, total, has_more}` from a Qobuz list container
/// (`{items: [...], total, offset, limit}`). Copes with an endpoint that
/// ignored the requested offset and started earlier.
pub fn page(container: Option<&Value>, offset: u32, limit: u32, map: impl Fn(&Value) -> Option<Item>) -> Value {
    let raw = container.and_then(|c| c.get("items")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let served_from = container.and_then(|c| c.get("offset")).and_then(Value::as_u64).unwrap_or(offset as u64);
    let skip = (offset as u64).saturating_sub(served_from) as usize;
    let raw = raw.get(skip..).unwrap_or(&[]);
    let items: Vec<Value> = raw.iter().take(limit as usize).filter_map(|v| map(v).map(|i| i.to_json())).collect();
    let total = container.and_then(|c| c.get("total")).and_then(Value::as_u64);
    let seen = offset as u64 + raw.len().min(limit as usize) as u64;
    // `discover/*` answers carry `has_more` but no total.
    let said_more = container.and_then(|c| c.get("has_more")).and_then(Value::as_bool);
    let has_more = match (total, said_more) {
        (Some(total), _) => seen < total,
        (None, Some(more)) => more || raw.len() > limit as usize,
        (None, None) => raw.len() >= limit as usize,
    };
    let mut out = json!({"items": items, "has_more": has_more});
    if let Some(total) = total {
        out["total"] = total.into();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_round_trip_and_reject_junk() {
        for s in [
            "track/123", "album/0060253780968", "artist/9", "playlist/42", "mix/weekly", "fav", "fav/albums", "my/playlists",
            "mixes", "discover", "featured/new-releases", "discover/qobuzissims", "themes", "theme/hi-res",
        ] {
            assert_eq!(Ref::parse(s).unwrap().to_string(), s);
        }
        for s in ["", "track/", "track/1/2", "album/a b", "featured/whatever", "discover/whatever", "mix/", "radio/1", "fav/stuff"] {
            assert_eq!(Ref::parse(s), None, "{s}");
        }
    }

    #[test]
    fn album_track_inherits_album_metadata() {
        let album = json!({
            "id": "abc123", "title": "Goldberg Variations", "artist": {"name": "Glenn Gould"},
            "release_date_original": "1982-01-01", "genre": {"name": "Classique"},
            "image": {"large": "https://x/l.jpg"}, "maximum_sampling_rate": 44.1, "maximum_bit_depth": 16
        });
        let t = json!({"id": 77, "title": "Aria", "duration": 185, "track_number": 1, "media_number": 1});
        let item = track(&t, Some(&album)).unwrap();
        assert_eq!(item.reference, "track/77");
        assert_eq!(item.artist.as_deref(), Some("Glenn Gould"));
        assert_eq!(item.album.as_deref(), Some("Goldberg Variations"));
        assert_eq!(item.subtitle.as_deref(), Some("Glenn Gould · Goldberg Variations"));
        assert_eq!(item.year, Some(1982));
        assert_eq!(item.duration_ms, Some(185_000));
        assert_eq!(item.format, Some(Format { sample_rate: 44100, bits: 16, codec: "flac" }));
        assert!(item.playable && !item.browsable);
    }

    #[test]
    fn albums_and_pages() {
        let a = json!({"id": "x1", "title": "Kind of Blue", "version": "Remastered", "artist": {"name": "Miles Davis"},
                       "released_at": 1_000_000_000, "streamable": false});
        let item = album(&a).unwrap();
        assert_eq!(item.title, "Kind of Blue (Remastered)");
        assert_eq!(item.subtitle.as_deref(), Some("Miles Davis · 2001"));
        assert!(!item.playable && item.browsable);

        let container = json!({"items": [a, {"title": "no id"}], "total": 10});
        let p = page(Some(&container), 0, 2, album);
        assert_eq!(p["items"].as_array().unwrap().len(), 1);
        assert_eq!(p["total"], 10);
        assert_eq!(p["has_more"], true);
        assert_eq!(page(None, 0, 50, album), json!({"items": [], "has_more": false}));

        // Offset ignored by the API: the page is cut out of what came back.
        let all = json!({"items": [{"id": "a"}, {"id": "b"}, {"id": "c"}], "offset": 0, "total": 3});
        let p = page(Some(&all), 1, 1, album);
        assert_eq!(p["items"][0]["ref"], "album/b");
        assert_eq!(p["has_more"], true);
    }

    #[test]
    fn discover_shapes() {
        let a = json!({
            "id": "jj5", "title": "The Source", "image": {"large": "https://x/l.jpg"},
            "artists": [{"name": "Featured Guest", "roles": ["featured-artist"]}, {"name": "Justus Eichhorn", "roles": ["main-artist"]}],
            "dates": {"original": "2026-09-11"}, "audio_info": {"maximum_sampling_rate": 96, "maximum_bit_depth": 24},
            "rights": {"streamable": false}
        });
        let item = album(&a).unwrap();
        assert_eq!(item.artist.as_deref(), Some("Justus Eichhorn"));
        assert_eq!(item.year, Some(2026));
        assert_eq!(item.format, Some(Format { sample_rate: 96000, bits: 24, codec: "flac" }));
        assert!(!item.playable);

        let p = json!({"id": 70, "name": "Warp", "owner": {"name": "Qobuz"},
                       "image": {"rectangle": "https://x/r.jpg", "covers": ["https://x/c.jpg"]}});
        assert_eq!(playlist(&p).unwrap().art.as_deref(), Some("https://x/c.jpg"));

        let m = json!({"type": "weekly", "title": "WeeklyQ", "baseline": "Every Friday", "duration": 7453,
                       "images": {"large": "https://x/w.png"}});
        let item = mix(&m).unwrap();
        assert_eq!((item.reference.as_str(), item.kind, item.browsable), ("mix/weekly", "playlist", true));
        assert_eq!(item.subtitle.as_deref(), Some("Every Friday"));

        // `has_more` but no total.
        let container = json!({"has_more": true, "items": [{"id": "a"}, {"id": "b"}]});
        assert_eq!(page(Some(&container), 0, 10, album)["has_more"], true);
        let container = json!({"has_more": false, "items": [{"id": "a"}, {"id": "b"}]});
        assert_eq!(page(Some(&container), 0, 2, album)["has_more"], false);

        let tag = json!({"slug": "mood", "name_json": "{\"fr\":\"Humeurs\",\"en\":\"Mood\",\"ja\":\"\"}"});
        assert_eq!(theme(&tag, "fr").unwrap().title, "Humeurs");
        assert_eq!(theme(&tag, "ja").unwrap().title, "Mood", "empty name: English");
        assert_eq!(theme(&json!({"slug": "event"}), "fr").unwrap().title, "event");
        assert_eq!(theme(&tag, "fr").unwrap().reference, "theme/mood");
    }

    #[test]
    fn item_json_skips_missing_fields() {
        let v = folder(&Ref::MyPlaylists, "Playlists").to_json();
        assert_eq!(v, json!({"ref": "my/playlists", "kind": "folder", "title": "Playlists", "playable": false, "browsable": true}));
    }
}
