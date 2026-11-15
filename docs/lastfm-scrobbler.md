# Shairport Sync Last.fm Scrobbler

This workspace application reads Shairport Sync metadata from exactly one input
backend (`pipe`, `dbus`, or `mpris`), sends Last.fm “Now Playing” updates, and
attempts to scrobble recognized tracks.

The package manifest is at `lastfm-scrobbler/Cargo.toml`; its implementation is
in [`src/main.rs`](../lastfm-scrobbler/src/main.rs). Run the following commands
from the workspace root.

## Requirements

- Shairport Sync with at least one metadata interface enabled (`pipe`, native
  DBus, or MPRIS).
- A Last.fm account and API key/shared secret.
- The Last.fm account username and password used for password authentication.

Set `LASTFM_INPUT_BACKEND` to exactly one backend:
- `pipe` (default)
- `dbus`
- `mpris`

This selection is exclusive. The service consumes only the selected backend to
avoid duplicate scrobbles from mixed metadata sources.

For local/offline preparation without real Last.fm credentials, set
`LASTFM_OFFLINE_MODE=1`. In offline mode the app still reads and parses
metadata, but skips Last.fm authentication and API calls.

Failed scrobbles are queued in SQLite for retries. By default the queue
database is `/var/lib/shairport-scrobbler/scrobble-queue.sqlite3`. Override via
`LASTFM_QUEUE_DB` if needed.

### Input backend configuration (exclusive mode)

Set one backend in the service:

```ini
Environment="LASTFM_INPUT_BACKEND=pipe"
```

or

```ini
Environment="LASTFM_INPUT_BACKEND=dbus"
```

or

```ini
Environment="LASTFM_INPUT_BACKEND=mpris"
```

Only configure environment variables for the selected backend:
- `pipe`: `SHAIRPORT_PIPE`
- `dbus`: `SHAIRPORT_DBUS_BUS`, `SHAIRPORT_DBUS_SERVICE`,
  `SHAIRPORT_DBUS_PATH`
- `mpris`: `MPRIS_BUS`, `MPRIS_SERVICE`, `MPRIS_PATH`

If `dbus` or `mpris` is selected, leave `SHAIRPORT_PIPE` unset. If `pipe` is
selected, leave DBus/MPRIS override variables unset.

### Getting metadata from Shairport Sync

1. Edit `/etc/shairport-sync.conf` and enable metadata output to a named pipe.
   Example:

   ```conf
   metadata =
   {
       enabled = "yes";
       include_cover_art = "no";
       pipe_name = "/tmp/shairport-sync-metadata";
   };
   ```

2. Restart Shairport Sync:

   ```bash
   sudo systemctl restart shairport-sync
   ```

3. Confirm the metadata pipe exists:

   ```bash
   ls -l /tmp/shairport-sync-metadata
   ```

4. Verify metadata is flowing while AirPlay playback is active:

   ```bash
   sudo timeout 15s cat /tmp/shairport-sync-metadata
   ```

   You should see XML `<item>...</item>` records.

5. If you use a different pipe path, set it for this service:

   ```ini
   Environment="SHAIRPORT_PIPE=/your/pipe/path"
   ```

6. Ensure Shairport metadata includes track fields used by this app:
   - `core/minm` (title)
   - `core/asar` (artist)
   - `core/asal` (album)
   - `ssnc/mdst` and `ssnc/mden` (metadata block boundaries)

### Using native DBus input

1. Select DBus mode in the service:

   ```ini
   Environment="LASTFM_INPUT_BACKEND=dbus"
   ```

2. Keep defaults or override if your Shairport instance uses custom names:

   ```ini
   # Optional; defaults shown:
   # Environment="SHAIRPORT_DBUS_BUS=system"
   # Environment="SHAIRPORT_DBUS_SERVICE=org.gnome.ShairportSync"
   # Environment="SHAIRPORT_DBUS_PATH=/org/gnome/ShairportSync"
   ```

3. Ensure Shairport Sync is built/configured with DBus interface support.

### Using MPRIS input

1. Select MPRIS mode in the service:

   ```ini
   Environment="LASTFM_INPUT_BACKEND=mpris"
   ```

2. Keep defaults or override:

   ```ini
   # Optional; defaults shown:
   # Environment="MPRIS_BUS=system"
   # Environment="MPRIS_SERVICE=org.mpris.MediaPlayer2.ShairportSync"
   # Environment="MPRIS_PATH=/org/mpris/MediaPlayer2"
   ```

3. Ensure Shairport Sync is built/configured with MPRIS interface support.

### Last.fm API key and authentication

1. Create a Last.fm API application and copy its API key and shared secret:
   - Sign in to Last.fm.
   - Open the API account creation page at
     <https://www.last.fm/api/account/create>.
   - Submit the form and copy the generated values.

2. Configure all required Last.fm environment variables in the systemd service:

   ```ini
   Environment="LASTFM_API_KEY=your_api_key"
   Environment="LASTFM_API_SECRET=your_api_secret"
   Environment="LASTFM_USERNAME=your_lastfm_username"
   Environment="LASTFM_PASSWORD=your_lastfm_password"
   ```

3. Reload and restart the service after updating credentials:

   ```bash
   sudo systemctl daemon-reload
   sudo systemctl restart shairport-scrobbler
   ```

4. Verify authentication succeeded:

   ```bash
   journalctl -u shairport-scrobbler -n 100 --no-pager
   ```

   On successful startup, the app logs:
   `Successfully authenticated with Last.fm.`

Authentication details for this application:
- It uses Last.fm password authentication at startup (not browser OAuth).
- It authenticates once, then sends `now_playing` and `scrobble` requests with
  the authenticated session.
- Missing or invalid `LASTFM_API_KEY`, `LASTFM_API_SECRET`, `LASTFM_USERNAME`,
  or `LASTFM_PASSWORD` causes startup failure.
- Set `LASTFM_OFFLINE_MODE=1` to skip authentication and Last.fm requests during
  local metadata testing.
- When Last.fm requests fail, scrobbles are persisted to a local SQLite queue
  and retried automatically.

## Build

```bash
cargo build --release -p shairport-scrobbler
sudo install -m 755 target/release/shairport-scrobbler /usr/local/bin/
```

## Run as a systemd service (with Shairport Sync dependency)

Install and enable Shairport Sync first:

```bash
sudo systemctl enable --now shairport-sync
sudo systemctl status shairport-sync --no-pager
```

Then create the scrobbler service at
`/etc/systemd/system/shairport-scrobbler.service`:

```ini
[Unit]
Description=Shairport Sync Last.fm Scrobbler
# Start only after Shairport Sync is up.
After=shairport-sync.service
# Pull in Shairport Sync when this service starts.
Requires=shairport-sync.service

[Service]
Type=simple
Environment="LASTFM_INPUT_BACKEND=pipe"
Environment="LASTFM_API_KEY=your_api_key"
Environment="LASTFM_API_SECRET=your_api_secret"
Environment="LASTFM_USERNAME=your_username"
Environment="LASTFM_PASSWORD=your_password"
# Optional for local testing without Last.fm credentials/API calls.
# Environment="LASTFM_OFFLINE_MODE=1"
# Optional SQLite path for failed scrobble retry queue.
# Environment="LASTFM_QUEUE_DB=/var/lib/shairport-scrobbler/scrobble-queue.sqlite3"
# Optional; defaults to /tmp/shairport-sync-metadata.
# Environment="SHAIRPORT_PIPE=/path/to/shairport-sync-metadata"
# Optional DBus backend overrides.
# Environment="SHAIRPORT_DBUS_BUS=system"
# Environment="SHAIRPORT_DBUS_SERVICE=org.gnome.ShairportSync"
# Environment="SHAIRPORT_DBUS_PATH=/org/gnome/ShairportSync"
# Optional MPRIS backend overrides.
# Environment="MPRIS_BUS=system"
# Environment="MPRIS_SERVICE=org.mpris.MediaPlayer2.ShairportSync"
# Environment="MPRIS_PATH=/org/mpris/MediaPlayer2"
ExecStart=/usr/local/bin/shairport-scrobbler
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Dependency behavior:
- `After=shairport-sync.service`: startup ordering (scrobbler starts after
  Shairport Sync).
- `Requires=shairport-sync.service`: if Shairport Sync fails to start, this
  service is not started.
- The scrobbler also retries opening the metadata pipe every three seconds, so
  short races during boot are tolerated.

Protect the service file because it contains credentials, then reload and
enable it:

```bash
sudo chmod 600 /etc/systemd/system/shairport-scrobbler.service
sudo systemctl daemon-reload
sudo systemctl enable --now shairport-scrobbler
```

Verify both services and logs:

```bash
systemctl status shairport-sync shairport-scrobbler --no-pager
journalctl -u shairport-sync -u shairport-scrobbler -n 200 --no-pager
```

## Behavior

- Reads metadata from exactly one selected backend: `pipe`, `dbus`, or
  `mpris`.
- In `pipe` mode, decodes Shairport XML/Base64 metadata items.
- Uses the `core` DMAP fields `minm` (title), `asar` (artist), and `asal`
  (album). The `ssnc` events `mdst` and `mden` delimit metadata blocks.
- Sends a “Now Playing” update when a new track is recognized. The `pend` and
  `pfls` events mark playback stopped or paused; `prsm` and `pbeg` mark it
  resumed or started.
- Applies Last.fm scrobble thresholds:
  - tracks with duration `<= 30s` are not scrobbled
  - tracks `> 30s` are scrobbled after at least half the track duration or
    4 minutes, whichever comes first
  - if duration metadata is missing, falls back to a 30-second threshold
  - only active playback time counts toward the threshold (paused time does not)
- Failed scrobble submissions are stored in a local SQLite queue and retried
  periodically in batches (up to 50 tracks per request).
- Cached queued scrobbles are retried before new scrobble submissions.
- Retry handling is error-classified:
  - temporary service/network failures are retried
  - invalid session errors trigger re-authentication and retry
  - non-retriable request errors are dropped from the retry queue
- Retries opening the metadata pipe every three seconds if it is unavailable.

If Spotify is also linked to Last.fm and streamed to the Pi over AirPlay,
disable one of the scrobbling paths to avoid duplicate Spotify scrobbles.
