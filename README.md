# qconnect

**Qobuz** source plugin for players that implement the source plugin
protocol v1 (separate process, JSON-RPC over stdin/stdout). The player starts
it and hands it everything Qobuz-related:

- **account sign-in**, through the official Qobuz page (your password never
  goes through qconnect);
- **catalogue**: search, favourites (with their state on every item),
  playlists, mixes made for you, Discover shelves, genres, labels,
  purchases; radio from a track, album or artist, similar albums; artist
  biographies, album and track credits; lyrics;
- **your playlists**: create, rename, delete, add and remove tracks (only
  playlists you own);
- **playback**: each track becomes a stream URL, in the best quality your DAC
  plays natively. The player reads the stream itself; audio never goes
  through qconnect. Optionally (setting "Encrypted streaming", off by
  default), tracks come as the current Qobuz web player fetches them,
  decrypted by qconnect on this computer and handed over as the original
  FLAC file, unchanged;
- **play reports** (setting, on by default): plays are reported to Qobuz as
  its apps do, so they count for the artists and your history;
- **Qobuz Connect**: the player shows up as a device in the Qobuz app (phone,
  desktop), wherever it is, and can be controlled from the app.

Implementation details: [docs/plugin.md](docs/plugin.md).

> Unofficial Qobuz client. It uses the Qobuz web API outside its terms of
> use. A Qobuz subscription is required. Qobuz may break it or restrict the
> accounts that use it. Use at your own risk.

## Installation

```sh
cargo install --path .         # → ~/.cargo/bin/qconnect
```

Build dependency: `protoc` (package `protobuf`), for prost-build.

Then declare the plugin in your player's configuration, with the absolute
path of the binary and the `plugin` argument:

```toml
[[plugins]]
id = "qobuz"
command = "/path/to/.cargo/bin/qconnect"
args = ["plugin"]
enabled = true
```

## Usage

1. In the player's plugin settings: Qobuz → **Sign in**. The Qobuz page
   opens in your browser.
   - Browser on the same machine: sign-in completes by itself.
   - Browser elsewhere (phone…): copy the full address of the page you land
     on (it contains `code_autorisation=`) and paste it into the player.
2. The Qobuz catalogue shows up in the player (browsing and search).
3. In the Qobuz app: Qobuz Connect icon → **<player name> (<machine
   name>)**. What you start from the app plays in the player. If you start
   something else directly in the player, the app sees it as a stop.

Quality follows the player's output: 192 kHz if it goes up to 176.4 kHz or
more, 96 kHz from 88.2 kHz, CD quality otherwise (and also for a DAC limited
to 16 bits). It also depends on your subscription.

## Data

In the directories the player gives the plugin (`data_dir`, `cache_dir`):

| File | Contents |
|---|---|
| `data_dir/account.json` | Qobuz session token (never the password), mode `600` |
| `data_dir/device.json` | Qobuz Connect device id |
| `data_dir/play-reports.json` | End-of-play reports not sent yet (only while some wait) |
| `cache_dir/web-config.json` | App id, OAuth key and signing secret read from the web player (public, the same for everyone) |

`account.json` gives access to your account, like a cookie. Signing out
deletes it.

## Troubleshooting

The plugin logs to stderr, which the player copies into its own log. More
detail: `RUST_LOG=qconnect=debug` in the player's environment.

- **`Invalid Request Signature` or `Invalid or missing app_id` in the
  log**: Qobuz changed the web player's secret or app id. qconnect reads
  them again from the web player by itself (and checks them daily anyway).
  If the message persists, run `cargo test live_bundle -- --ignored
  --nocapture`: if it fails, the bundle's layout has changed, and
  `src/secret.rs` needs updating.
- **The device does not show up in the app**: look for `registered with
  Qobuz Connect as renderer` in the log. The app must be signed in to the
  same account.
- **`ws: no valid token`**: the account token expired or was revoked. Sign
  in again from the player.

## Tests

```sh
cargo test                                        # unit tests + fake host against a mock API
cargo test live_bundle -- --ignored --nocapture   # secret extraction against the live web player
```

## Architecture

| Module | Role |
|---|---|
| `plugin/mod.rs` | JSON-RPC loop, `initialize`/`shutdown`, `auth.*`, Qobuz Connect start-up |
| `plugin/rpc.rs` | Line-based JSON-RPC 2.0, requests to the host, error codes |
| `plugin/catalog.rs`, `plugin/items.rs` | `browse.*`, `library.*`, `search`, `item.get`, `favorites.set`; API responses → items |
| `plugin/resolve.rs` | `track.resolve`: quality matched to the output, URL expiry |
| `plugin/remote.rs` | Qobuz Connect → host: commands → `player.*`, `player.state` → events |
| `session.rs`, `ws.rs`, `proto.rs` | Qobuz Connect renderer: WebSocket, state, the app's queue |
| `player.rs` | Interface between the session and its output (`remote.rs`) |
| `api.rs`, `secret.rs` | Qobuz REST API, request signing; app id, OAuth key and secret read from the web player |
| `account.rs`, `auth.rs` | OAuth code exchange, browser redirect listener, token storage |
