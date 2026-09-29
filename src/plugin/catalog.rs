//! `browse.root`, `browse.list`, `search`, `item.get`, `favorites.set`.

use serde::Deserialize;
use serde_json::{json, Value};

use super::items::{self, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::api::ApiClient;

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
        Ref::Featured(kind) if kind == "new-releases" => "New releases",
        Ref::Featured(_) => "Qobuz selection",
        _ => "?",
    }
}

fn folder(r: Ref) -> Value {
    items::folder(&r, section_title(&r)).to_json()
}

pub fn root() -> RpcResult {
    let mut sections = vec![folder(Ref::Favorites), folder(Ref::MyPlaylists)];
    sections.extend(items::FEATURED.iter().map(|kind| folder(Ref::Featured(kind.to_string()))));
    Ok(json!({"sections": sections}))
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

pub async fn list(api: &ApiClient, p: ListParams) -> RpcResult {
    let (offset, limit) = (p.offset, clamp(p.limit));
    let page = match parse_ref(&p.reference)? {
        Ref::Favorites => {
            let children = [Ref::FavAlbums, Ref::FavTracks, Ref::FavArtists];
            let items: Vec<Value> = children.into_iter().map(folder).collect();
            return Ok(json!({"items": items, "total": 3, "has_more": false}));
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
            items::page(v.get("artists"), offset, limit, items::artist)
        }
        Ref::MyPlaylists => {
            let v = api.user_playlists(offset, limit).await?;
            items::page(v.get("playlists"), offset, limit, items::playlist)
        }
        Ref::Featured(kind) => {
            let v = api.featured_albums(&kind, offset, limit).await?;
            items::page(v.get("albums"), offset, limit, items::album)
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

pub async fn get(api: &ApiClient, reference: &str) -> RpcResult {
    let r = parse_ref(reference)?;
    let item = match &r {
        Ref::Track(id) => items::track(&api.track_get(id).await?, None),
        Ref::Album(id) => items::album(&api.album_get(id, 0, 1).await?),
        Ref::Artist(id) => items::artist(&api.artist_get(id, 0, 1).await?),
        Ref::Playlist(id) => items::playlist(&api.playlist_get(id, 0, 1).await?),
        _ => return Ok(folder(r)),
    };
    item.map(|i| i.to_json()).ok_or_else(|| RpcError::not_found(format!("{reference}: unreadable answer")))
}

pub async fn set_favorite(api: &ApiClient, reference: &str, on: bool) -> RpcResult {
    let (field, id) = match parse_ref(reference)? {
        Ref::Track(id) => ("track_ids", id),
        Ref::Album(id) => ("album_ids", id),
        Ref::Artist(id) => ("artist_ids", id),
        _ => return Err(RpcError::invalid_params(format!("{reference} cannot be a favourite"))),
    };
    api.set_favorite(field, &id, on).await?;
    Ok(Value::Null)
}
