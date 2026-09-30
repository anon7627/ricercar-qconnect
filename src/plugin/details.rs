//! `item.details {ref}` (capability `details`): what a page shows beyond the
//! item's children, as `{biography?, related?, facts?}`:
//! - artist (`artist/page`): biography, top tracks, similar artists, their
//!   playlists;
//! - album (`album/get`, `album/suggest`): label, genre, release date, discs,
//!   awards, copyright; similar albums;
//! - track (`track/get`): composer and credits (`performers`);
//! - label (`label/page`): description, founding year, origin, founders;
//!   top artists and tracks, playlists.
//!
//! Texts are plain: Qobuz sends some HTML, which is stripped here.

use serde_json::{json, Value};

use super::items::{self, tr, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::api::ApiClient;

/// Items per related shelf.
const SHELF_ITEMS: usize = 20;
const MAX_FACTS: usize = 30;
const MAX_FACT_VALUE: usize = 500;

/// Plain text from Qobuz HTML: tags dropped, `<br>` and paragraph ends as
/// line breaks, common entities decoded, blank lines collapsed.
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        let name = tag.trim_start_matches('/').split([' ', '/']).next().unwrap_or("");
        if matches!(name, "br" | "p" | "div" | "li") && (name == "br" || tag.starts_with('/')) {
            out.push('\n');
            if name == "p" {
                out.push('\n');
            }
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    let decoded = decode_entities(&out);
    let mut text = String::new();
    let mut blank = 0;
    for line in decoded.lines().map(str::trim) {
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !text.is_empty() {
            text.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        text.push_str(line);
    }
    text
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..semi];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "rsquo" | "lsquo" => Some('’'),
            "hellip" => Some('…'),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=semi]),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

fn text_of(v: Option<&Value>) -> Option<String> {
    let t = plain_text(v?.as_str()?);
    (!t.is_empty()).then_some(t)
}

fn shelf(title: String, items: Vec<Value>) -> Option<Value> {
    (!items.is_empty()).then(|| json!({"title": title, "items": items}))
}

/// Items of a list that may be bare (`[…]`) or wrapped (`{items: […]}`).
fn list(v: Option<&Value>) -> &[Value] {
    let v = match v {
        Some(Value::Object(o)) => o.get("items"),
        other => other,
    };
    v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn mapped(list: &[Value], map: impl Fn(&Value) -> Option<items::Item>) -> Vec<Value> {
    list.iter().filter_map(map).take(SHELF_ITEMS).map(|i| i.to_json()).collect()
}

fn fact(label: String, value: impl Into<String>) -> Option<Value> {
    let value: String = value.into().trim().chars().take(MAX_FACT_VALUE).collect();
    (!value.is_empty()).then(|| json!({"label": label, "value": value}))
}

/// `2026-09-25` as the host's language writes dates.
fn date(iso: &str) -> String {
    match (iso.get(..4), iso.get(5..7), iso.get(8..10), tr("en", "fr").as_str()) {
        (Some(y), Some(m), Some(d), "fr") => format!("{d}/{m}/{y}"),
        _ => iso.to_string(),
    }
}

/// `performers`: "Name, Role, Role - Name, Role - …" → one fact per person.
fn credits(performers: &str) -> Vec<Value> {
    performers
        .split(" - ")
        .filter_map(|entry| {
            let mut parts = entry.split(',').map(str::trim).filter(|p| !p.is_empty());
            let name = parts.next()?;
            let roles: Vec<&str> = parts.collect();
            let label = if roles.is_empty() { tr("Performer", "Interprète") } else { roles.join(", ") };
            fact(label.chars().take(80).collect(), name)
        })
        .collect()
}

pub async fn get(api: &ApiClient, reference: &str) -> RpcResult {
    let r = Ref::parse(reference).ok_or_else(|| RpcError::not_found(format!("unknown ref {reference:?}")))?;
    let mut related: Vec<Value> = Vec::new();
    let mut facts: Vec<Value> = Vec::new();
    let mut biography = None;
    match &r {
        Ref::Artist(id) => {
            let page = api.artist_page(id).await?;
            biography = text_of(page.pointer("/biography/content"));
            related.extend(shelf(tr("Top tracks", "Titres phares"), mapped(list(page.get("top_tracks")), |t| items::track(t, None))));
            related.extend(shelf(tr("Similar artists", "Artistes similaires"), mapped(list(page.get("similar_artists")), items::artist)));
            related.extend(shelf(tr("Playlists", "Playlists"), mapped(list(page.get("playlists")), items::playlist)));
        }
        Ref::Album(id) => {
            let (album, similar) = tokio::join!(api.album_get(id, 0, 1), api.album_suggest(id));
            let album = album?;
            biography = text_of(album.get("description"));
            let s = |path: &str| album.pointer(path).and_then(Value::as_str).map(str::to_string);
            facts.extend(s("/label/name").and_then(|v| fact(tr("Label", "Label"), v)));
            facts.extend(s("/genre/name").and_then(|v| fact(tr("Genre", "Genre"), v)));
            facts.extend(s("/release_date_original").and_then(|v| fact(tr("Released", "Sortie"), date(&v))));
            if let Some(n) = album.get("media_count").and_then(Value::as_u64).filter(|&n| n > 1) {
                facts.extend(fact(tr("Discs", "Disques"), n.to_string()));
            }
            let awards: Vec<String> = list(album.get("awards"))
                .iter()
                .filter_map(|a| a.get("name").and_then(Value::as_str).map(str::to_string))
                .collect();
            if !awards.is_empty() {
                facts.extend(fact(tr("Awards", "Distinctions"), awards.join(" · ")));
            }
            facts.extend(s("/copyright").and_then(|v| fact("©".into(), v)));
            if let Ok(similar) = similar {
                related.extend(shelf(tr("Similar albums", "Albums similaires"), mapped(list(similar.get("albums")), items::album)));
            }
        }
        Ref::Track(id) => {
            let track = api.track_get(id).await?;
            if let Some(c) = track.pointer("/composer/name").and_then(Value::as_str) {
                facts.extend(fact(tr("Composer", "Compositeur"), c));
            }
            if let Some(p) = track.get("performers").and_then(Value::as_str) {
                facts.extend(credits(p));
            }
        }
        Ref::Label(id) => {
            let page = api.label_page(id).await?;
            biography = text_of(page.get("description"));
            let n = |key: &str| page.get(key).and_then(Value::as_u64).map(|n| n.to_string());
            facts.extend(n("foundation_year").and_then(|v| fact(tr("Founded", "Fondation"), v)));
            facts.extend(page.get("geographic_origin").and_then(Value::as_str).and_then(|v| fact(tr("Origin", "Origine"), v)));
            let founders: Vec<&str> = list(page.get("founders")).iter().filter_map(Value::as_str).collect();
            if !founders.is_empty() {
                facts.extend(fact(tr("Founders", "Fondateurs"), founders.join(", ")));
            }
            related.extend(shelf(tr("Top artists", "Artistes phares"), mapped(list(page.get("top_artists")), items::artist)));
            related.extend(shelf(tr("Top tracks", "Titres phares"), mapped(list(page.get("top_tracks")), |t| items::track(t, None))));
            related.extend(shelf(tr("Playlists", "Playlists"), mapped(list(page.get("playlists")), items::playlist)));
        }
        _ => {}
    }
    facts.truncate(MAX_FACTS);
    let mut out = json!({});
    if let Some(text) = biography {
        out["biography"] = json!({"text": text, "source": "Qobuz"});
    }
    if !related.is_empty() {
        out["related"] = related.into();
    }
    if !facts.is_empty() {
        out["facts"] = facts.into();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_plain_text() {
        assert_eq!(
            plain_text("<p>Linkin Park s&rsquo;est <b>imposé</b>&nbsp;:</p><p>Line<br/>break &amp; co &#233;t&#xE9;</p>"),
            "Linkin Park s’est imposé :\n\nLine\nbreak & co été"
        );
        assert_eq!(plain_text("no markup, a & b"), "no markup, a & b");
        assert_eq!(plain_text("<script>x"), "x", "tags go, text stays text");
        assert_eq!(plain_text("a <b unclosed"), "a");
    }

    #[test]
    fn credits_become_facts() {
        let f = credits("Neal Avron, Mixer - BRAD DELSON, Guitar, Co- Producer - Solo");
        assert_eq!(f[0], json!({"label": "Mixer", "value": "Neal Avron"}));
        assert_eq!(f[1], json!({"label": "Guitar, Co- Producer", "value": "BRAD DELSON"}));
        assert_eq!(f[2]["value"], "Solo");
    }
}
