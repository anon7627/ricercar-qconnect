//! Protocol `ref`s and conversion of Qobuz API objects into protocol items.
//!
//! Refs: `track/<id>`, `album/<id>`, `artist/<id>`, `playlist/<id>`,
//! `label/<id>` for catalogue entries; `mix/<type>` for the account's mixes
//! (WeeklyQ…);
//! `fav`, `fav/albums`, `fav/tracks`, `fav/artists`, `my/playlists`,
//! `mixes`, `discover`, `featured/<type>`, `discover/<shelf>`, `themes` and
//! `theme/<tag>` (playlists by theme) for sections.

use std::collections::HashSet;
use std::sync::Mutex;

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
    Label(String),
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
                    "label" if valid_id(&id) => Ref::Label(id),
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
            Ref::Label(id) => write!(f, "label/{id}"),
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
    /// Albums, playlists: number of tracks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    pub playable: bool,
    pub browsable: bool,
    /// Tracks: their album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_ref: Option<String>,
    /// Tracks and albums: their main artist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_ref: Option<String>,
    /// Albums and tracks: their label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_ref: Option<String>,
    /// In the account's favourites; absent when not known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
    /// Tracks of a playlist: their entry in it (`playlist_track_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// Playlists: the account owns it, so it may edit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editable: Option<bool>,
    /// Related content offered in the item's menu.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

/// An entry of `Item.actions`: play or open a related ref.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Action {
    pub id: &'static str,
    pub label: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: &'static str,
}

/// Ids of the account's favourites (`favorite/getUserFavoriteIds`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FavoriteIds {
    pub albums: HashSet<String>,
    pub tracks: HashSet<String>,
    pub artists: HashSet<String>,
}

impl FavoriteIds {
    pub fn from_json(v: &Value) -> Self {
        let ids = |key: &str| -> HashSet<String> {
            v.get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|id| match id {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect()
        };
        FavoriteIds { albums: ids("albums"), tracks: ids("tracks"), artists: ids("artists") }
    }

    fn set(&mut self, kind: &str) -> Option<&mut HashSet<String>> {
        match kind {
            "album" => Some(&mut self.albums),
            "track" => Some(&mut self.tracks),
            "artist" => Some(&mut self.artists),
            _ => None,
        }
    }
}

/// The signed-in account, as far as item conversion needs it. One account
/// per plugin process, hence a global.
#[derive(Default)]
struct Account {
    user_id: Option<u64>,
    favorites: Option<FavoriteIds>,
}

static ACCOUNT: Mutex<Account> = Mutex::new(Account { user_id: None, favorites: None });

fn account() -> std::sync::MutexGuard<'static, Account> {
    ACCOUNT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Qobuz user id of the signed-in account (`None`: signed out).
pub fn set_user(user_id: Option<u64>) {
    let mut a = account();
    if a.user_id != user_id {
        a.favorites = None;
    }
    a.user_id = user_id;
}

pub fn set_favorites(favorites: Option<FavoriteIds>) {
    account().favorites = favorites;
}

/// After `favorites.set`: `kind` is `track`, `album` or `artist`.
pub fn note_favorite(kind: &str, id: &str, on: bool) {
    if let Some(set) = account().favorites.as_mut().and_then(|f| f.set(kind)) {
        if on {
            set.insert(id.to_string());
        } else {
            set.remove(id);
        }
    }
}

/// Whether `id` of `kind` is a favourite; `None` until the ids are loaded.
fn is_favorite(kind: &str, id: &str) -> Option<bool> {
    let mut a = account();
    let set = a.favorites.as_mut()?.set(kind)?;
    Some(set.contains(id))
}

fn owned_by_account(owner_id: Option<u64>) -> Option<bool> {
    let user = account().user_id?;
    Some(owner_id == Some(user))
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
    id_at(v, "id")
}

fn id_at(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

/// `name` as a string, or `{display}` in `artist/page` answers.
fn name_of(v: &Value) -> Option<String> {
    str_at(v, &["name"]).or_else(|| str_at(v, &["name", "display"]))
}

/// The main artist of a track or album: `performer` (tracks), `artist`, or
/// the `artists` list of newer answers, where the main artist carries the
/// `main-artist` role. Returns the object, with a name or an id.
fn main_artist(v: &Value) -> Option<&Value> {
    let named = |a: &&Value| name_of(a).is_some() || id_of(a).is_some();
    v.get("performer").filter(named).or_else(|| v.get("artist").filter(named)).or_else(|| {
        let artists = v.get("artists")?.as_array()?;
        let main = |a: &&Value| a.get("roles").and_then(Value::as_array).is_some_and(|r| r.iter().any(|r| r == "main-artist"));
        artists.iter().find(main).or(artists.first())
    })
}

fn artist_ref_of(v: &Value) -> Option<String> {
    main_artist(v).and_then(id_of).map(|id| Ref::Artist(id).to_string())
}

fn label_ref_of(album: &Value) -> Option<String> {
    album.get("label").and_then(id_of).map(|id| Ref::Label(id).to_string())
}

/// Artist picture: `image`, `picture`, or the `portrait` hash of newer
/// answers.
fn artist_art(v: &Value) -> Option<String> {
    str_at(v, &["image", "large"])
        .or_else(|| str_at(v, &["image", "medium"]))
        .or_else(|| str_at(v, &["picture"]))
        .or_else(|| {
            let hash = str_at(v, &["images", "portrait", "hash"]).filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()))?;
            let ext = str_at(v, &["images", "portrait", "format"]).filter(|f| f.chars().all(|c| c.is_ascii_alphanumeric()));
            Some(format!("https://static.qobuz.com/images/artists/covers/large/{hash}.{}", ext.as_deref().unwrap_or("jpg")))
        })
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

/// `tracks_count`, or `track_count` in `discover/*` and mixes.
fn track_count_of(v: &Value) -> Option<u32> {
    v.get("tracks_count").or_else(|| v.get("track_count")).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
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
    let album_artist = album.and_then(main_artist).and_then(name_of);
    let artist = main_artist(v).and_then(name_of).or_else(|| album_artist.clone());
    let number = |key: &str| v.get(key).or_else(|| v.get("physical_support")?.get(key)).and_then(Value::as_u64);
    Some(Item {
        reference: Ref::Track(id.clone()).to_string(),
        kind: "track",
        title: title_with_version(v, "title"),
        subtitle: join(&[artist.as_deref(), album_title.as_deref()]),
        artist,
        album: album_title,
        album_artist,
        track_no: number("track_number"),
        disc_no: number("media_number"),
        year: album.and_then(year_of),
        genre: album.and_then(|a| str_at(a, &["genre", "name"])),
        duration_ms: v.get("duration").and_then(Value::as_u64).map(|s| s * 1000),
        track_count: None,
        art: album.and_then(cover_of),
        format: format_of(v).or_else(|| album.and_then(format_of)),
        playable: streamable(v),
        browsable: false,
        album_ref: album.and_then(id_of).map(|a| Ref::Album(a).to_string()),
        artist_ref: artist_ref_of(v).or_else(|| album.and_then(artist_ref_of)),
        label_ref: album.and_then(label_ref_of),
        favorite: is_favorite("track", &id),
        entry_id: id_at(v, "playlist_track_id"),
        editable: None,
        actions: Vec::new(),
    })
}

fn streamable(v: &Value) -> bool {
    v.get("streamable").or_else(|| v.pointer("/rights/streamable")).and_then(Value::as_bool).unwrap_or(true)
}

pub fn album(v: &Value) -> Option<Item> {
    let id = id_of(v)?;
    let artist = main_artist(v).and_then(name_of);
    let year = year_of(v);
    let year_s = year.map(|y| y.to_string());
    Some(Item {
        kind: "album",
        title: title_with_version(v, "title"),
        subtitle: join(&[artist.as_deref(), year_s.as_deref()]),
        album_artist: artist.clone(),
        artist,
        year,
        genre: str_at(v, &["genre", "name"]),
        duration_ms: v.get("duration").and_then(Value::as_u64).map(|s| s * 1000),
        track_count: track_count_of(v),
        art: cover_of(v),
        format: format_of(v),
        playable: streamable(v),
        browsable: true,
        artist_ref: artist_ref_of(v),
        label_ref: label_ref_of(v),
        favorite: is_favorite("album", &id),
        reference: Ref::Album(id).to_string(),
        ..Default::default()
    })
}

pub fn artist(v: &Value) -> Option<Item> {
    let id = id_of(v)?;
    Some(Item {
        kind: "artist",
        title: name_of(v).unwrap_or_else(|| "?".into()),
        art: artist_art(v),
        browsable: true,
        favorite: is_favorite("artist", &id),
        reference: Ref::Artist(id).to_string(),
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
        track_count: track_count_of(v),
        art: first("images300")
            .or_else(|| first("image_rectangle"))
            .or_else(|| first("images"))
            // `discover/playlists`: `image: {covers: [...], rectangle}`.
            .or_else(|| v.pointer("/image/covers")?.as_array()?.iter().find_map(|u| u.as_str()).map(str::to_string))
            .or_else(|| str_at(v, &["image", "rectangle"])),
        browsable: true,
        editable: owned_by_account(v.pointer("/owner/id").and_then(Value::as_u64)),
        ..Default::default()
    })
}

/// A label, opened as the list of its albums.
pub fn label(v: &Value) -> Option<Item> {
    let id = id_of(v)?;
    Some(Item {
        kind: "folder",
        title: name_of(v).unwrap_or_else(|| "?".into()),
        subtitle: v.get("albums_count").and_then(Value::as_u64).map(|n| format!("{n} albums")),
        art: str_at(v, &["image", "large"]).or_else(|| str_at(v, &["image"])),
        browsable: true,
        reference: Ref::Label(id).to_string(),
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
        track_count: track_count_of(v),
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
            "track/123", "album/0060253780968", "artist/9", "playlist/42", "label/315932", "mix/weekly", "fav", "fav/albums", "my/playlists",
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
            "id": "jj5", "title": "The Source", "track_count": 26, "image": {"large": "https://x/l.jpg"},
            "artists": [{"name": "Featured Guest", "roles": ["featured-artist"]}, {"name": "Justus Eichhorn", "roles": ["main-artist"]}],
            "dates": {"original": "2026-09-11"}, "audio_info": {"maximum_sampling_rate": 96, "maximum_bit_depth": 24},
            "rights": {"streamable": false}
        });
        let item = album(&a).unwrap();
        assert_eq!(item.artist.as_deref(), Some("Justus Eichhorn"));
        assert_eq!(item.track_count, Some(26));
        assert_eq!(item.year, Some(2026));
        assert_eq!(item.format, Some(Format { sample_rate: 96000, bits: 24, codec: "flac" }));
        assert!(!item.playable);

        let p = json!({"id": 70, "name": "Warp", "owner": {"name": "Qobuz"},
                       "image": {"rectangle": "https://x/r.jpg", "covers": ["https://x/c.jpg"]}, "tracks_count": 14});
        assert_eq!(playlist(&p).unwrap().art.as_deref(), Some("https://x/c.jpg"));
        assert_eq!(playlist(&p).unwrap().track_count, Some(14));

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
    fn newer_shapes_and_links() {
        // A `radio/*` track: `artists` with roles, `physical_support`,
        // `audio_info`, `rights`, the album with its label.
        let t = json!({
            "id": 78636911, "title": "I'll Cut You Down", "duration": 301,
            "physical_support": {"media_number": 1, "track_number": 3},
            "audio_info": {"maximum_sampling_rate": 44.1, "maximum_bit_depth": 16},
            "rights": {"streamable": true},
            "artists": [{"id": 5, "name": "Guest", "roles": ["featured-artist"]},
                        {"id": 963677, "name": "Uncle Acid & the Deadbeats", "roles": ["main-artist"]}],
            "album": {"id": "gkgujbp3tsxbc", "title": "Blood Lust", "label": {"id": 315932, "name": "Rise Above"},
                      "image": {"large": "https://x/c.jpg"}}
        });
        let item = track(&t, None).unwrap();
        assert_eq!(item.artist.as_deref(), Some("Uncle Acid & the Deadbeats"));
        assert_eq!((item.track_no, item.disc_no), (Some(3), Some(1)));
        assert_eq!(item.album_ref.as_deref(), Some("album/gkgujbp3tsxbc"));
        assert_eq!(item.artist_ref.as_deref(), Some("artist/963677"));
        assert_eq!(item.label_ref.as_deref(), Some("label/315932"));
        assert!(item.playable);

        // Inside an album listing: the album passed along gives the links.
        let album_json = json!({"id": "abc", "title": "A", "artist": {"id": 7, "name": "B"}, "label": {"id": 9}});
        let item = track(&json!({"id": 1, "title": "T", "playlist_track_id": 10756537909u64}), Some(&album_json)).unwrap();
        assert_eq!((item.album_ref.as_deref(), item.artist_ref.as_deref()), (Some("album/abc"), Some("artist/7")));
        assert_eq!(item.entry_id.as_deref(), Some("10756537909"));
        let a = album(&album_json).unwrap();
        assert_eq!((a.artist_ref.as_deref(), a.label_ref.as_deref()), (Some("artist/7"), Some("label/9")));

        // `artist/page`: `name.display` and a portrait hash.
        let ar = artist(&json!({"id": 43470, "name": {"display": "30 Seconds To Mars"},
                                "images": {"portrait": {"hash": "eae6e529", "format": "jpg"}}})).unwrap();
        assert_eq!(ar.title, "30 Seconds To Mars");
        assert_eq!(ar.art.as_deref(), Some("https://static.qobuz.com/images/artists/covers/large/eae6e529.jpg"));
        let bad = artist(&json!({"id": 1, "name": "x", "images": {"portrait": {"hash": "../../etc"}}})).unwrap();
        assert_eq!(bad.art, None, "only a hex hash makes a URL");

        let l = label(&json!({"id": 315932, "name": "Rise Above Limited", "albums_count": 189})).unwrap();
        assert_eq!((l.reference.as_str(), l.kind, l.subtitle.as_deref()), ("label/315932", "folder", Some("189 albums")));
    }

    /// Account-dependent fields; one test, since the account is global.
    #[test]
    fn favourites_and_ownership_follow_the_account() {
        let mine = json!({"id": 1, "name": "Soir", "owner": {"id": 42}});
        let theirs = json!({"id": 2, "name": "Top", "owner": {"id": 7}});
        set_user(None);
        assert_eq!(playlist(&mine).unwrap().editable, None, "unknown when signed out");
        set_user(Some(42));
        assert_eq!(playlist(&mine).unwrap().editable, Some(true));
        assert_eq!(playlist(&theirs).unwrap().editable, Some(false));

        let t = json!({"id": 77, "title": "Aria"});
        assert_eq!(track(&t, None).unwrap().favorite, None, "ids not loaded yet");
        set_favorites(Some(FavoriteIds::from_json(&json!({"tracks": [77], "albums": ["abc"], "artists": []}))));
        assert_eq!(track(&t, None).unwrap().favorite, Some(true));
        assert_eq!(album(&json!({"id": "abc"})).unwrap().favorite, Some(true));
        assert_eq!(artist(&json!({"id": 5, "name": "x"})).unwrap().favorite, Some(false));
        note_favorite("track", "77", false);
        assert_eq!(track(&t, None).unwrap().favorite, Some(false));
        set_user(Some(43));
        assert_eq!(track(&t, None).unwrap().favorite, None, "another account: ids dropped");
        set_user(None);
    }

    #[test]
    fn item_json_skips_missing_fields() {
        let v = folder(&Ref::MyPlaylists, "Playlists").to_json();
        assert_eq!(v, json!({"ref": "my/playlists", "kind": "folder", "title": "Playlists", "playable": false, "browsable": true}));
    }
}
