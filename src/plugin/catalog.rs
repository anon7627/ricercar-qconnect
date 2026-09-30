//! `browse.root`, `browse.list`, `library.*`, `search`, `item.get`,
//! `favorites.set`.

use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use super::items::{self, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::api::ApiClient;

/// Artist lookups run at once when filling in missing pictures.
const ART_LOOKUPS: usize = 8;

/// Protocol maximum page size.
const MAX_PAGE: u32 = 200;
const DEFAULT_PAGE: u32 = 50;

fn clamp(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
}

fn parse_ref(s: &str) -> Result<Ref, RpcError> {
    Ref::parse(s).ok_or_else(|| RpcError::not_found(format!("unknown ref {s:?}")))
}

pub fn section_title(r: &Ref) -> &'static str {
    match r {
        Ref::Favorites => "Favourites",
        Ref::FavAlbums => "Albums",
        Ref::FavTracks => "Tracks",
        Ref::FavArtists => "Artists",
        Ref::MyPlaylists => "My playlists",
        Ref::Mixes => "For you",
        Ref::Discover => "Discover",
        Ref::Themes => "Playlists by theme",
        Ref::Featured(kind) if kind == "new-releases" => "New releases",
        Ref::Featured(_) => "Qobuz selection",
        Ref::DiscoverShelf(shelf) => match shelf.as_str() {
            "qobuzissims" => "Qobuzissimes",
            "album-of-the-week" => "Album of the week",
            "playlists" => "Qobuz playlists",
            "most-streamed" => "Most streamed",
            "press-awards" => "Press awards",
            _ => "Ideal discography",
        },
        _ => "?",
    }
}

fn folder(r: Ref) -> Value {
    items::folder(&r, section_title(&r)).to_json()
}

/// `discover/<shelf>` → web player endpoint.
fn discover_endpoint(shelf: &str) -> &'static str {
    match shelf {
        "qobuzissims" => "qobuzissims",
        "album-of-the-week" => "albumOfTheWeek",
        "playlists" => "playlists",
        "most-streamed" => "mostStreamed",
        "press-awards" => "pressAward",
        _ => "idealDiscography",
    }
}

/// Editorial shelves, in the order Home shows them.
fn editorial() -> Vec<Ref> {
    let featured = |kind: &str| Ref::Featured(kind.to_string());
    let mut refs = vec![featured("new-releases")];
    refs.extend(items::DISCOVER.iter().map(|shelf| Ref::DiscoverShelf(shelf.to_string())));
    refs.push(featured("editor-picks"));
    refs
}

/// `sections` for hosts that show the plugin in their sidebar; `home`, the
/// discovery shelves, for hosts that merge the `library` lists instead
/// (favourites and playlists already reach them that way).
pub fn root() -> RpcResult {
    let sections = [Ref::Favorites, Ref::MyPlaylists, Ref::Mixes, Ref::Discover].map(folder);
    let home: Vec<Value> = std::iter::once(Ref::Mixes).chain(editorial()).map(folder).collect();
    Ok(json!({"sections": sections, "home": home}))
}

#[derive(Deserialize)]
pub struct PageParams {
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// `library.albums`, `.artists`, `.tracks`, `.playlists`: the account's
/// favourites and playlists, one page at a time.
pub async fn library(api: &ApiClient, method: &str, p: PageParams) -> RpcResult {
    let (offset, limit) = (p.offset, clamp(p.limit));
    let page = match method {
        "library.albums" => {
            let v = api.user_favorites("albums", offset, limit).await?;
            items::page(v.get("albums"), offset, limit, items::album)
        }
        "library.artists" => {
            let v = api.user_favorites("artists", offset, limit).await?;
            with_artist_art(api, items::page(v.get("artists"), offset, limit, items::artist)).await
        }
        "library.tracks" => {
            let v = api.user_favorites("tracks", offset, limit).await?;
            items::page(v.get("tracks"), offset, limit, |t| items::track(t, None))
        }
        "library.playlists" => {
            let v = api.user_playlists(offset, limit).await?;
            items::page(v.get("playlists"), offset, limit, items::playlist)
        }
        _ => return Err(RpcError::new(super::rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
    };
    Ok(page)
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// `lang`: the host's language, for the names of playlist themes.
pub async fn list(api: &ApiClient, p: ListParams, lang: &str) -> RpcResult {
    let (offset, limit) = (p.offset, clamp(p.limit));
    let page = match parse_ref(&p.reference)? {
        Ref::Favorites => {
            let children = [Ref::FavAlbums, Ref::FavTracks, Ref::FavArtists];
            let items: Vec<Value> = children.into_iter().map(folder).collect();
            return Ok(json!({"items": items, "total": 3, "has_more": false}));
        }
        Ref::Discover => {
            // The Home shelves, playlists by theme after the Qobuz playlists.
            let mut refs = editorial();
            let at = refs.iter().position(|r| *r == Ref::DiscoverShelf("playlists".into())).map_or(refs.len(), |i| i + 1);
            refs.insert(at, Ref::Themes);
            let items: Vec<Value> = refs.into_iter().map(folder).collect();
            return Ok(json!({"total": items.len(), "items": items, "has_more": false}));
        }
        Ref::Mixes => {
            // A plain list, all at once.
            let v = api.mixes().await?;
            let container = json!({"items": v.as_array().cloned().unwrap_or_default()});
            items::page(Some(&container), offset, limit, items::mix)
        }
        Ref::Mix(kind) => {
            let mut v = api.mix(&kind, offset, limit).await?;
            // The tracks come without a total; the mix gives it.
            if let (Some(n), Some(tracks)) = (v.get("track_count").cloned(), v.get_mut("tracks").and_then(Value::as_object_mut)) {
                tracks.entry("total").or_insert(n);
            }
            items::page(v.get("tracks"), offset, limit, |t| items::track(t, None))
        }
        Ref::Album(id) => {
            let album = api.album_get(&id, offset, limit).await?;
            items::page(album.get("tracks"), offset, limit, |t| items::track(t, Some(&album)))
        }
        Ref::Artist(id) => {
            let artist = api.artist_get(&id, offset, limit).await?;
            items::page(artist.get("albums"), offset, limit, items::album)
        }
        Ref::Playlist(id) => {
            let playlist = api.playlist_get(&id, offset, limit).await?;
            items::page(playlist.get("tracks"), offset, limit, |t| items::track(t, None))
        }
        Ref::Label(id) => {
            let label = api.label_get(&id, offset, limit).await?;
            items::page(label.get("albums"), offset, limit, items::album)
        }
        Ref::FavAlbums => {
            let v = api.user_favorites("albums", offset, limit).await?;
            items::page(v.get("albums"), offset, limit, items::album)
        }
        Ref::FavTracks => {
            let v = api.user_favorites("tracks", offset, limit).await?;
            items::page(v.get("tracks"), offset, limit, |t| items::track(t, None))
        }
        Ref::FavArtists => {
            let v = api.user_favorites("artists", offset, limit).await?;
            with_artist_art(api, items::page(v.get("artists"), offset, limit, items::artist)).await
        }
        Ref::MyPlaylists => {
            let v = api.user_playlists(offset, limit).await?;
            items::page(v.get("playlists"), offset, limit, items::playlist)
        }
        Ref::Featured(kind) => {
            let v = api.featured_albums(&kind, offset, limit).await?;
            items::page(v.get("albums"), offset, limit, items::album)
        }
        Ref::DiscoverShelf(shelf) => {
            let v = api.discover(discover_endpoint(&shelf), offset, limit).await?;
            let map = if shelf == "playlists" { items::playlist } else { items::album };
            items::page(Some(&v), offset, limit, map)
        }
        Ref::Themes => {
            let v = api.playlist_tags().await?;
            let container = json!({"items": v.get("tags").cloned().unwrap_or_default()});
            items::page(Some(&container), offset, limit, |t| items::theme(t, lang))
        }
        Ref::Theme(tag) => {
            let v = api.discover_playlists(&tag, offset, limit).await?;
            items::page(Some(&v), offset, limit, items::playlist)
        }
        Ref::Track(_) => return Err(RpcError::invalid_params("a track is not browsable")),
    };
    Ok(page)
}

#[derive(Deserialize)]
pub struct SearchParams {
    query: String,
    kinds: Option<Vec<String>>,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// Protocol kind → Qobuz search group.
fn search_group(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "track" => "tracks",
        "album" => "albums",
        "artist" => "artists",
        "playlist" => "playlists",
        _ => return None,
    })
}

pub async fn search(api: &ApiClient, p: SearchParams) -> RpcResult {
    let query = p.query.trim();
    if query.is_empty() {
        return Err(RpcError::invalid_params("empty query"));
    }
    let kinds: Vec<&str> = match &p.kinds {
        Some(kinds) => kinds.iter().map(String::as_str).filter(|k| search_group(k).is_some()).collect(),
        None => vec!["track", "album", "artist", "playlist"],
    };
    if kinds.is_empty() {
        return Ok(json!({"groups": []}));
    }
    let limit = clamp(p.limit);
    // One kind: let the API search only that; otherwise one call for all.
    let only = (kinds.len() == 1).then(|| search_group(kinds[0])).flatten();
    let v = api.search(query, only, p.offset, limit).await?;
    let groups: Vec<Value> = kinds
        .iter()
        .map(|&kind| {
            let container = v.get(search_group(kind).expect("filtered above"));
            let mut group = match kind {
                "track" => items::page(container, p.offset, limit, |t| items::track(t, None)),
                "album" => items::page(container, p.offset, limit, items::album),
                "artist" => items::page(container, p.offset, limit, items::artist),
                _ => items::page(container, p.offset, limit, items::playlist),
            };
            group["kind"] = kind.into();
            group
        })
        .collect();
    Ok(json!({"groups": groups}))
}

pub async fn get(api: &ApiClient, reference: &str, lang: &str) -> RpcResult {
    let r = parse_ref(reference)?;
    let item = match &r {
        Ref::Track(id) => items::track(&api.track_get(id).await?, None),
        Ref::Album(id) => items::album(&api.album_get(id, 0, 1).await?),
        Ref::Artist(id) => items::artist(&api.artist_get(id, 0, 1).await?),
        Ref::Playlist(id) => items::playlist(&api.playlist_get(id, 0, 1).await?),
        Ref::Label(id) => items::label(&api.label_get(id, 0, 1).await?),
        Ref::Mix(kind) => items::mix(&api.mix(kind, 0, 1).await?),
        Ref::Theme(tag) => {
            let v = api.playlist_tags().await?;
            let tags = v.get("tags").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            tags.iter().filter(|t| t.get("slug").and_then(Value::as_str) == Some(tag)).find_map(|t| items::theme(t, lang))
        }
        _ => return Ok(folder(r)),
    };
    item.map(|i| i.to_json()).ok_or_else(|| RpcError::not_found(format!("{reference}: unreadable answer")))
}

pub async fn set_favorite(api: &ApiClient, reference: &str, on: bool) -> RpcResult {
    let (kind, field, id) = match parse_ref(reference)? {
        Ref::Track(id) => ("track", "track_ids", id),
        Ref::Album(id) => ("album", "album_ids", id),
        Ref::Artist(id) => ("artist", "artist_ids", id),
        _ => return Err(RpcError::invalid_params(format!("{reference} cannot be a favourite"))),
    };
    api.set_favorite(field, &id, on).await?;
    items::note_favorite(kind, &id, on);
    Ok(Value::Null)
}

/// Many artists have no picture on Qobuz. Give those the cover of one of
/// their albums, so that the host's artist pages are not blank.
async fn with_artist_art(api: &ApiClient, mut page: Value) -> Value {
    let Some(list) = page.get_mut("items").and_then(Value::as_array_mut) else { return page };
    let missing: Vec<(usize, String)> = list
        .iter()
        .enumerate()
        .filter(|(_, it)| it.get("art").is_none())
        .filter_map(|(i, it)| Some((i, it["ref"].as_str()?.strip_prefix("artist/")?.to_string())))
        .collect();
    let found: Vec<(usize, String)> = stream::iter(missing)
        .map(|(i, id)| async move {
            let v = api.artist_get(&id, 0, 1).await.ok()?;
            let cover = v.pointer("/albums/items/0").and_then(items::album)?.art?;
            Some((i, cover))
        })
        .buffer_unordered(ART_LOOKUPS)
        .filter_map(|found| async move { found })
        .collect()
        .await;
    for (i, art) in found {
        list[i]["art"] = art.into();
    }
    page
}
