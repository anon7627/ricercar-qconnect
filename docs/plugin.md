# qconnect: how the plugin works

qconnect is a source plugin (protocol v1): a process started by the host
player, speaking JSON-RPC 2.0 over stdin/stdout, one JSON message per line.
Logs go to stderr. The plugin never touches the signal: it hands the host
stream URLs that the host plays itself.

```
host ──(JSON-RPC over stdin/stdout)──► qconnect plugin
                                         ├─ auth.*        → Qobuz OAuth
                                         ├─ browse.*      → favourites, playlists, new releases
                                         ├─ search        → catalog/search
                                         ├─ item.get      → track/album/artist/playlist get
                                         ├─ track.resolve → track/getFileUrl
                                         └─ player.*  ◄── Qobuz Connect (remote_control)
host ──(HTTP)──► Qobuz CDN   (the stream never goes through qconnect)
```

Advertised capabilities: `auth`, `browse`, `search`, `resolve`,
`favorites`, `remote_control`, `library`.

The plugin never writes the account's e-mail address to its log (stderr)
nor to the account it reports to the host: hosts keep both, in their logs
and diagnostic reports. The account shows the Qobuz display name and the
subscription.

## Start-up

`initialize` provides:
- `data_dir`: account token, device id;
- `cache_dir`: signing secret;
- `output`: what the output plays natively;
- `host.name`: Qobuz Connect device name.

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

Protocol refs:
- catalogue entries: `track/<id>`, `album/<id>`, `artist/<id>`,
  `playlist/<id>`;
- sections: `fav` (a folder holding the next three), `fav/albums`,
  `fav/tracks`, `fav/artists`, `my/playlists`, `featured/new-releases`,
  `featured/editor-picks`.

`browse.root` returns:
- `sections`, for hosts that show the plugin in their sidebar: Favourites ·
  My playlists · New releases · Qobuz selection;
- `home`, the discovery shelves for hosts that merge the library lists:
  New releases · Qobuz selection. Favourites and playlists are left out,
  since they reach the host through `library.*`.

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
| HTTP 404, unknown `ref` | `not_found` (-32002) |
| HTTP 429 | `rate_limited` (-32004), `data.retry_after` from `Retry-After` (30 s otherwise) |
| HTTP 5xx, connection error or timeout | `network` (-32005) |

## Request signing (`src/secret.rs`)

Some requests (`track/getFileUrl`, `session/start`,
`favorite/getUserFavorites`) carry a `request_sig`: MD5 of
`object + method + sorted parameters (key+value) + request_ts + secret`.

The web player does not ship the secret in clear. It rebuilds it at run time
(`rng.prototype.initialization`):
1. it takes a seed, plus the `info` and `extras` fields of one entry of its
   timezone table (the `berlin` entry in production);
2. it joins the three, drops the last 44 characters and base64-decodes the
   rest.

qconnect signs with the cached secret (`web-secret.json`), or else with
`APP_SECRET` (`src/msgtype.rs`). When Qobuz answers `400 Invalid Request
Signature`, qconnect:
1. downloads `play.qobuz.com/login`, then the `bundle.js` it references;
2. re-derives the secret and caches it;
3. retries the request once.

It downloads the bundle at most once a minute.

Check against the live web player:

```sh
cargo test live_bundle -- --ignored --nocapture
```

If this test fails, the bundle's structure has changed and `derive()` needs
updating.

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
- catalogue, `library.*`, `home` shelves, search groups, `track.resolve`
  matched to the output, errors;
- the account's e-mail never appears in the log.

`plugin/remote.rs` has its own tests for the command translation.

## Risks

- **Request signing**: if Qobuz changes how the bundle hides the secret,
  `secret::derive` will need updating.
- **Lifetime of stream URLs and of the account token**: not measured.
- **Terms of use**: qconnect uses the Qobuz web API outside its terms of
  use; Qobuz may block it or restrict the accounts that use it.
