//! `playlists.*` (capability `playlist_edit`): edits of the account's own
//! playlists, as the web player makes them (unsigned form POSTs):
//!
//! | Method | Endpoint | Fields |
//! |---|---|---|
//! | `playlists.create {name, description?, public?}` | `playlist/create` | `name`, `description`, `is_public`, `is_collaborative` |
//! | `playlists.rename {ref, name}` | `playlist/update` | `playlist_id`, `name` |
//! | `playlists.delete {ref}` | `playlist/delete` | `playlist_id` |
//! | `playlists.add {ref, items}` | `playlist/addTracks` | `playlist_id`, `track_ids` (comma-separated) |
//! | `playlists.remove {ref, entries}` | `playlist/deleteTracks` | `playlist_id`, `playlist_track_ids` |
//! | `playlists.move {ref, entry, to}` | `playlist/updateTracksPosition` | `playlist_id`, `playlist_track_ids`, `insert_before` (1-based) |
//!
//! Every edit of an existing playlist first checks that the account owns
//! it: a followed playlist is never touched.

use serde::Deserialize;
use serde_json::Value;

use super::items::{self, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::api::ApiClient;

const MAX_NAME: usize = 200;
const MAX_DESCRIPTION: usize = 2000;
/// Tracks per request.
const MAX_TRACKS: usize = 500;

#[derive(Deserialize)]
pub struct Params {
    #[serde(rename = "ref")]
    reference: Option<String>,
    name: Option<String>,
    description: Option<String>,
    public: Option<bool>,
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    entries: Vec<String>,
    entry: Option<String>,
    to: Option<u32>,
}

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError::invalid_params(msg)
}

fn name(p: &Params) -> Result<String, RpcError> {
    let name = p.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| invalid("a name is needed"))?;
    Ok(name.chars().take(MAX_NAME).collect())
}

fn entry_id(s: &str) -> Result<String, RpcError> {
    let ok = !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_digit());
    ok.then(|| s.to_string()).ok_or_else(|| invalid(format!("{s:?} is not a playlist entry")))
}

/// The playlist id of `ref`, once checked to be the account's own.
async fn owned(api: &ApiClient, p: &Params) -> Result<String, RpcError> {
    let reference = p.reference.as_deref().ok_or_else(|| invalid("ref is needed"))?;
    let Some(Ref::Playlist(id)) = Ref::parse(reference) else {
        return Err(invalid(format!("{reference} is not a playlist")));
    };
    let playlist = api.playlist_get(&id, 0, 1).await?;
    let owner = playlist.pointer("/owner/id").and_then(Value::as_u64);
    if owner.is_none() || owner != items::user_id() {
        return Err(invalid(format!("{reference} belongs to another account")));
    }
    Ok(id)
}

pub async fn edit(api: &ApiClient, method: &str, p: Params) -> RpcResult {
    match method {
        "playlists.create" => {
            let mut fields = vec![
                ("name", name(&p)?),
                ("is_public", p.public.unwrap_or(false).to_string()),
                ("is_collaborative", "false".to_string()),
            ];
            if let Some(d) = p.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                fields.push(("description", d.chars().take(MAX_DESCRIPTION).collect()));
            }
            let created = api.playlist_edit("create", fields).await?;
            items::playlist(&created)
                .map(|i| i.to_json())
                .ok_or_else(|| RpcError::new(super::rpc::INTERNAL, "playlist/create: unreadable answer"))
        }
        "playlists.rename" => {
            let new_name = name(&p)?;
            let id = owned(api, &p).await?;
            api.playlist_edit("update", vec![("playlist_id", id), ("name", new_name)]).await?;
            Ok(Value::Null)
        }
        "playlists.delete" => {
            let id = owned(api, &p).await?;
            api.playlist_edit("delete", vec![("playlist_id", id)]).await?;
            Ok(Value::Null)
        }
        "playlists.add" => {
            let tracks: Vec<String> = p
                .items
                .iter()
                .map(|r| match Ref::parse(r) {
                    Some(Ref::Track(id)) => Ok(id),
                    _ => Err(invalid(format!("{r} is not a track"))),
                })
                .collect::<Result<_, _>>()?;
            if tracks.is_empty() || tracks.len() > MAX_TRACKS {
                return Err(invalid(format!("1 to {MAX_TRACKS} tracks at a time")));
            }
            let id = owned(api, &p).await?;
            let fields = vec![("playlist_id", id), ("track_ids", tracks.join(",")), ("no_duplicate", "false".into())];
            api.playlist_edit("addTracks", fields).await?;
            Ok(Value::Null)
        }
        "playlists.remove" => {
            let entries: Vec<String> = p.entries.iter().map(|e| entry_id(e)).collect::<Result<_, _>>()?;
            if entries.is_empty() || entries.len() > MAX_TRACKS {
                return Err(invalid(format!("1 to {MAX_TRACKS} entries at a time")));
            }
            let id = owned(api, &p).await?;
            api.playlist_edit("deleteTracks", vec![("playlist_id", id), ("playlist_track_ids", entries.join(","))]).await?;
            Ok(Value::Null)
        }
        "playlists.move" => {
            let entry = entry_id(p.entry.as_deref().ok_or_else(|| invalid("entry is needed"))?)?;
            let to = p.to.ok_or_else(|| invalid("to is needed"))?;
            let id = owned(api, &p).await?;
            let fields = vec![("playlist_id", id), ("playlist_track_ids", entry), ("insert_before", (to + 1).to_string())];
            api.playlist_edit("updateTracksPosition", fields).await?;
            Ok(Value::Null)
        }
        _ => Err(RpcError::new(super::rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_numeric_ids() {
        assert_eq!(entry_id("10756537909").unwrap(), "10756537909");
        assert!(entry_id("").is_err() && entry_id("1,2").is_err() && entry_id("abc").is_err());
    }

    #[test]
    fn names_are_trimmed_and_required() {
        let p = |n: &str| Params {
            reference: None, name: Some(n.into()), description: None, public: None,
            items: vec![], entries: vec![], entry: None, to: None,
        };
        assert_eq!(name(&p("  Soir  ")).unwrap(), "Soir");
        assert!(name(&p("   ")).is_err());
    }
}
