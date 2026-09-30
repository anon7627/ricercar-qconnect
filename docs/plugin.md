# qconnect: how the plugin works

qconnect is a source plugin (protocol v1): a process started by the host
player, speaking JSON-RPC 2.0 over stdin/stdout, one JSON message per line.
Logs go to stderr. The plugin never touches the signal: it hands the host
stream URLs that the host plays itself.

```
host ──(JSON-RPC over stdin/stdout)──► qconnect plugin
                                         ├─ auth.*        → Qobuz OAuth
                                         ├─ browse.*      → favourites, playlists, mixes, Discover
                                         ├─ search        → catalog/search
                                         ├─ item.get      → track/album/artist/playlist get
                                         ├─ track.resolve → track/getFileUrl
                                         └─ player.*  ◄── Qobuz Connect (remote_control)
host ──(HTTP)──► Qobuz CDN   (the stream never goes through qconnect)
```

Advertised capabilities: `auth`, `browse`, `search`, `resolve`,
`favorites`, `reporting`, `remote_control`, `library`, `lyrics`, `radio`,
`details`, `playlist_edit`.

The plugin never writes the account's e-mail address to its log (stderr)
nor to the account it reports to the host: hosts keep both, in their logs
and diagnostic reports. The account shows the Qobuz display name and the
subscription.

## Start-up

`initialize` provides:
- `data_dir`: account token, device id;
- `cache_dir`: signing secret;
- `output`: what the output plays natively;
- `locale`: language of the playlist themes' names and of the settings'
  labels (French or English);
- `host.name`: Qobuz Connect device name;
- `settings`: the values stored for the plugin's settings (below).

## Settings

Declared in the `initialize` result (`settings`), labelled in the host's
language. Both apply without restarting the plugin (`settings.changed`).

| Key | Type | Default | Effect |
|---|---|---|---|
| `report_playback` | bool | `true` | Report plays to Qobuz (below) |
| `cmaf` | bool | `false` | Fetch streams as encrypted CMAF segments, decrypted locally (see Resolution) |

Values of unknown keys, or of the wrong type, are ignored; missing keys
take their default.

## Play reports (`reporting`)

The capability is always declared; the `report_playback` setting decides
whether anything is sent. The plugin reports plays the way the web player
does:
- `playback.started {ref}` → `track/reportStreamingStart`, form field
  `events` = `[{track_id, date, user_id, format_id}]`;
- `playback.ended {ref, listened_ms}` → an event `{blob, track_context_uuid,
  start_stream, online: true, local: false, duration}` (seconds, at most the
  track's length), sent in batches of 20 to `track/reportStreamingEndJson`
  with `renderer_context.software_version` = `wp-<bundle version>`.

`blob` comes from the `track/getFileUrl` answer at resolution; a play
without one, or with nothing listened, is not reported. Unsent end events
wait in `data_dir/play-reports.json` (500 at most) and go out after the next
play or at the next start. Turning the setting off, or signing out, drops
them. Neither request is signed.

`output.changed` updates the output in use; the object is accepted on its
own or wrapped in `{output: …}`.

## Sign-in (`auth.*`)

| Method | Implementation |
|---|---|
| `auth.status` | `Credentials::load`, then `ApiClient::user_info`: `signed_in`, `expired` if the token is refused, `signed_out` without a token |
| `auth.begin` | Returns the Qobuz OAuth URL with `expects_input: true`. Also listens on `/login/callback` on a free `127.0.0.1` port for 15 minutes, and sends `auth.changed` when the browser comes back to it. |
| `auth.complete` | Extracts the code from the pasted address (`code_autorisation=`), trades it for a token, saves `account.json` |
| `auth.sign_out` | Deletes `account.json` and leaves Qobuz Connect |

When the API refuses the token mid-session, the plugin sends
`auth.changed {state: "expired"}` once.

## Catalogue

| Need | Endpoint |
|---|---|
| Search | `catalog/search?query=&type=&offset=&limit=` |
| Album and its tracks | `album/get?album_id=&offset=&limit=` |
| Artist and their albums | `artist/get?artist_id=&extra=albums` |
| Playlist | `playlist/get?playlist_id=&extra=tracks` |
| My playlists | `playlist/getUserPlaylists` |
| Favourites | `favorite/getUserFavorites?type=` (signed) |
| Add or remove a favourite | `favorite/create`, `favorite/delete` |
| New releases, selection | `album/getFeatured?type=new-releases|editor-picks` |
| Genres, their new releases | `genre/list`, `album/getFeatured?type=new-releases&genre_id=` |
| Purchases | `purchase/getUserPurchases?type=albums` |
| Radio from a track, album or artist | `radio/track?track_id=`, `radio/album?album_id=`, `radio/artist?artist_id=` (about 30 tracks, `limit` ignored) |
| Similar albums | `album/suggest?album_id=` (about 30, not paged) |
| Label and its albums | `label/get?label_id=&extra=albums` |
| Discover shelves | `discover/qobuzissims`, `albumOfTheWeek`, `playlists`, `mostStreamed`, `pressAward`, `idealDiscography` (`{has_more, items}`, no total) |
| Playlist themes | `playlist/getTags`, then `discover/playlists?tags=<slug>` |
| The account's mixes | `dynamic-tracks/list`, then `dynamic-tracks/get?type=&extra=tracks` |

Protocol refs:
- catalogue entries: `track/<id>`, `album/<id>`, `artist/<id>`,
  `playlist/<id>`, `label/<id>` (a folder of the label's albums,
  `label/get?extra=albums`), `mix/<type>`;
- sections: `fav` (a folder holding the next three), `fav/albums`,
  `fav/tracks`, `fav/artists`, `my/playlists`, `mixes`, `discover` (a
  folder holding the editorial shelves), `featured/new-releases`,
  `featured/editor-picks`, `discover/qobuzissims`,
  `discover/album-of-the-week`, `discover/playlists`,
  `discover/most-streamed`, `discover/press-awards`,
  `discover/ideal-discography`, `themes` (a folder holding one
  `theme/<slug>` folder per playlist theme), `genres` (in Discover, one
  `genre/<id>` folder of new releases per genre), `purchases`;
- related content: `radio/track/<id>`, `radio/album/<id>`,
  `radio/artist/<id>` (playlists of close tracks), `similar/<album id>`.

`browse.root` returns:
- `sections`, for hosts that show the plugin in their sidebar: Favourites ·
  My playlists · For you · Discover;
- `home`, the discovery shelves for hosts that merge the library lists:
  For you · New releases · Qobuzissimes · Album of the week · Qobuz
  playlists · Most streamed · Press awards · Ideal discography · Qobuz
  selection. Favourites and playlists are left out, since they reach the
  host through `library.*`.

**Playlists by theme.** The Discover folder also holds `themes`, after
Qobuz playlists (not on Home: 13 shelves would be too many). Each theme is
a folder of playlists (Hi-Res, Moods, Top playlists…). Qobuz names themes in
every language (`name_json`): the plugin takes the language of the host's
`locale` (`fr-FR` → `fr`), then English, then the slug.

**Mixes.** `mixes` lists the mixes Qobuz makes for the account, each one a
`playlist` item (`mix/<type>`) whose `browse.list` gives the tracks. The
web API only offers WeeklyQ (`type=weekly`; any other type answers `400
accepted values are weekly`): DailyQ, FavQ and TopQ exist in the mobile
apps only. Since the list comes from `dynamic-tracks/list`, new mixes show
up once the API offers them.

Albums and playlists carry `track_count` (hosts add them up on artist
pages), from `tracks_count`, or `track_count` in `discover/*` answers and
`dynamic-tracks/get`. The mixes list gives none.

`discover/*` answers use another album shape than `album/get` (`artists`
with roles, `dates.original`, `audio_info`, `rights.streamable`);
`items::album` reads both.

**Item fields beyond the basics:**
- `album_ref`, `artist_ref` (tracks, albums), `label_ref` (albums, tracks):
  from `album.id`, the main artist (`performer`, `artist`, or the entry of
  `artists` with the `main-artist` role) and `album.label.id`;
- `favorite`: whether the track, album or artist is among the account's
  favourites. The ids come from `favorite/getUserFavoriteIds`, read in the
  background at start-up and sign-in, again after 10 minutes, and updated at
  once by `favorites.set`. Absent until they are read;
- `entry_id` (tracks of a playlist): `playlist_track_id`, the entry in the
  playlist;
- `editable` (playlists): the account owns it (`owner.id` is its user id);
- `actions`, labelled in the host's language: on a track, its radio; on an
  album, its radio, similar albums and its label; on an artist, their radio.
  `play` actions point to a radio ref, `browse` actions to a folder.

**Continuous playback (`radio.next {seed, exclude, limit}`).** The seed is a
track, album, artist or radio ref. The plugin asks for that seed's radio
and answers `{items}`: playable tracks only, without the seed track and
without those in `exclude`, at most `limit` (20 by default). `-32602` for
any other seed.

Newer answers (`radio/*`, `artist/page`, `discover/*`) are read too: track
and disc numbers under `physical_support`, names as `{display}`, artist
pictures as a `portrait` hash
(`static.qobuz.com/images/artists/covers/large/<hash>.<format>`).

## Library (`library.*`)

| Method | Source | Items |
|---|---|---|
| `library.albums` | `favorite/getUserFavorites?type=albums` | favourite albums (artist, year and cover filled in; `browse.list` gives their tracks) |
| `library.artists` | `favorite/getUserFavorites?type=artists` | favourite artists (`browse.list` gives their albums) |
| `library.tracks` | `favorite/getUserFavorites?type=tracks` | favourite tracks |
| `library.playlists` | `playlist/getUserPlaylists` | playlists the user owns or follows (`browse.list` gives their tracks) |

All take `{offset, limit}` and answer `{items, total, has_more}`, 200 items
per page at most.

Many artists have no picture on Qobuz (`image`, `picture` and
`images.portrait` are all empty). For those, the plugin uses the cover of
one of their albums (`artist/get?extra=albums&limit=1`), with at most 8
lookups at once.

Pages hold at most 200 items. If the API ignores the requested `offset`, the
page is cut out of what it returns.

`favorites.set` accepts tracks, albums and artists; a playlist is refused
(`-32602`).

## Playlist edits (`playlists.*`)

Edits of the account's own playlists, made as the web player makes them:
unsigned form POSTs.

| Method | Endpoint | Fields |
|---|---|---|
| `playlists.create {name, description?, public?}` | `playlist/create` | `name`, `description`, `is_public`, `is_collaborative=false`; answers the new playlist item |
| `playlists.rename {ref, name}` | `playlist/update` | `playlist_id`, `name` |
| `playlists.delete {ref}` | `playlist/delete` | `playlist_id` |
| `playlists.add {ref, items}` | `playlist/addTracks` | `playlist_id`, `track_ids` (comma-separated), `no_duplicate=false` |
| `playlists.remove {ref, entries}` | `playlist/deleteTracks` | `playlist_id`, `playlist_track_ids` |
| `playlists.move {ref, entry, to}` | `playlist/updateTracksPosition` | `playlist_id`, `playlist_track_ids`, `insert_before` = `to` + 1 |

- **Ownership check.** Before editing an existing playlist, the plugin reads
  it (`playlist/get`) and refuses (`-32602`) unless its `owner.id` is the
  account's: a followed playlist is never touched.
- `items` must be track refs, 1 to 500; `entries` are the `entry_id` of the
  playlist's tracks (`playlist_track_id`, digits only).
- Names are trimmed (200 characters at most) and required.

## Details (`item.details`)

`{biography?: {text, source}, related?: [{title, items}], facts?: [{label,
value}]}`, titles and labels in the host's language, empty shelves left
out, 20 items per shelf:

| Ref | Source | Content |
|---|---|---|
| `artist/<id>` | `artist/page` | biography; top tracks, similar artists, playlists |
| `album/<id>` | `album/get`, `album/suggest` | description; label, genre, release date, discs, awards, copyright; similar albums |
| `track/<id>` | `track/get` | composer; credits from `performers` (`Name, Role, Role - …`), one fact per person |
| `label/<id>` | `label/page` | description; founding year, origin, founders; top artists and tracks, playlists |

Other refs answer `{}`. Qobuz texts may hold HTML: tags are dropped, line
breaks kept and entities decoded, so the host gets plain text.

## Lyrics (`lyrics.get`)

`track/lyricsUrl?track_id=` (signed) hands out a temporary, signed URL of a
JSON document; the plugin fetches it without Qobuz headers and never logs
it. The document's `original.lines` (`{line, start, end}`, times in ms)
become:
- `synced: [{time_ms, text}]` when `original.type` is `lsync` and every line
  has a time;
- `plain` otherwise.

A track without lyrics (404), a document of another track or an empty one
answer `not_found` (-32002). Tracks without lyrics are remembered for 6
hours, so the API is not asked again meanwhile.

## Resolution (`track.resolve`)

**Quality matched to the output:**
- `max_rate ≥ 176400`: `hires-192`;
- `≥ 88200`: `hires-96`;
- otherwise, or `max_bits < 24`: `lossless`.

**Fallback:**
- If the stream is not played natively (rate missing from `rates`, or above
  `max_rate` / `max_bits`), the next lower quality is requested.
- If nothing fits: `unavailable` (`-32003`).
- A 30 s preview (`sample: true`, subscription too low) also gives
  `unavailable`.
- When `getFileUrl` answers without `url`, Qobuz refuses the track and says
  why in `restrictions` (`SampleRestrictedByRightHolders`…): `unavailable`
  at once, since no lower quality would be served. Only a format
  restriction (`FormatRestrictedByFormatAvailability`) lets the next
  quality be tried. Tracks removed from the catalogue but still in the
  user's lists end this way; `track/get` then answers 404, and the message
  says the track is no longer in the catalogue.

**Response:**
- `format`: `sample_rate`, `bits`, `channels`, `codec`;
- `duration_ms` and `replaygain`, from `track/get`;
- `expires_at`: the URL's `etsp` parameter minus 60 s, or 10 minutes without
  it.

## Errors

| API response | Protocol code |
|---|---|
| HTTP 401 | `auth_required` (-32001) |
| HTTP 403 | `unavailable` (-32003) |
| `getFileUrl` without `url` (Qobuz refuses the track) | `unavailable` (-32003), `data.restrictions` holds Qobuz's codes |
| HTTP 404, unknown `ref` | `not_found` (-32002) |
| HTTP 429 | `rate_limited` (-32004), `data.retry_after` from `Retry-After` (30 s otherwise) |
| HTTP 5xx, connection error or timeout | `network` (-32005) |

## Web player values (`src/secret.rs`)

qconnect presents itself as the Qobuz web player, with three public values
from its bundle (`play.qobuz.com/login`, then the `bundle.js` it
references). They are the same for every user and identify no account:

| Value | Used for | Where in the bundle |
|---|---|---|
| app id | `X-App-Id` header, OAuth sign-in page | `production:{api:{appId:"…"` |
| OAuth private key | `oauth/callback`, with the code | `authenticate({privateKey:"…"` |
| signing secret | `request_sig` | rebuilt at run time, see below |

Some requests (`track/getFileUrl`, `session/start`,
`favorite/getUserFavorites`) carry a `request_sig`: MD5 of
`object + method + sorted parameters (key+value) + request_ts + secret`.
The web player does not ship the secret in clear. It rebuilds it at run time
(`rng.prototype.initialization`):
1. it takes a seed, plus the `info` and `extras` fields of one entry of its
   timezone table (the `berlin` entry in production);
2. it joins the three, drops the last 44 characters and base64-decodes the
   rest.

**When qconnect reads them.** qconnect uses the values cached in
`web-config.json` (`cache_dir`), or else the built-in ones
(`src/msgtype.rs`). It reads the bundle again:
- at `initialize`, in the background, when the cache is more than a day old
  (or missing);
- when Qobuz answers `400` about the signature (`request_sig`) or the app id
  (`Invalid or missing app_id parameter`); the request is then retried once.

The bundle is downloaded at most once a minute. The app id and the secret
are only replaced together, since one goes with the other; if the OAuth key
is no longer found, the known one is kept. A new app id may require signing
in again, since Qobuz ties account tokens to the app.

Check against the live web player:

```sh
cargo test live_bundle -- --ignored --nocapture
```

If this test fails, the bundle's structure has changed and `derive_config()`
needs updating.

## Qobuz Connect (`plugin/remote.rs`)

**Presence in the app.** As soon as the account is signed in (at
`initialize`, or after a sign-in), the plugin joins the account's Qobuz
Connect session as the renderer "<host.name> (<machine name>)". Its device
id is stable (`device.json`). It leaves the session on `auth.sign_out`, when
the token expires, and on `shutdown`.

**Session.** `session.rs` and `ws.rs` run the session: WebSocket, the app's
queue, state reported to the app. Their output is `remote.rs`, which turns
commands into plugin → host requests:

| Session command | Request to the host |
|---|---|
| `Load` | `player.play {items: [track/<id>], start: 0}`, then `player.seek` and `player.pause` once the track has started |
| `Preload` (gapless) | `player.enqueue {at: "next"}` |
| `Pause`, `Resume`, `Seek`, `Stop` | `player.pause`, `player.resume`, `player.seek`, `player.stop` (only while a track started from the app is playing) |
| `SetVolume` | `player.set_volume`, `player.set_mute` |

**From the host.** `player.state` feeds the position, the duration and the
play/pause state. It also produces these events:
- `Started`, with the format `track.resolve` picked for that track;
- `Paused`, `Resumed`;
- `Advanced`, when the host moves on to the preloaded track;
- `Ended`, on a stop less than 5 s before the end, or when the host moves on
  to a track that was not expected;
- `Stopped`, on other stops and on `player.taken_over`.

**Protocol v1 limits:**
- **No way to remove a queued item.** A track that was preloaded, then
  replaced in the app, stays in the host's queue. When the host reaches it,
  the plugin notices and the session restarts the right track.
- **Early `player.taken_over`.** One received within 3 s of the plugin's own
  `player.play`, before the track has started, is ignored: some hosts report
  it while they replace the queue.

## Tests

`tests/plugin.rs` runs `qconnect plugin --api-base <mock>` (hidden option)
against a mock Qobuz API. The mock checks signatures with a secret that
differs from `APP_SECRET`, served by a fake web player, which exercises the
secret re-derivation.

Covered:
- handshake;
- `auth.*`: sign-in through the browser and by paste, expired token;
- settings and play reports (on, then turned off);
- catalogue, `library.*`, `home` shelves, mixes, Discover shelves, search
  groups, `track.resolve` matched to the output, errors;
- the account's e-mail never appears in the log.

`plugin/remote.rs` has its own tests for the command translation.

## Risks

- **Web player values**: qconnect follows changes of the app id, the OAuth
  key and the secret, but not a change in how the bundle holds them; then
  `secret::derive_config` needs updating.
- **Lifetime of stream URLs and of the account token**: not measured.
- **Terms of use**: qconnect uses the Qobuz web API outside its terms of
  use; Qobuz may block it or restrict the accounts that use it.
