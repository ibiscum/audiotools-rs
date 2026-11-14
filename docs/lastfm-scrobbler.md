# Shairport Sync Last.fm Scrobbler

This workspace application reads Shairport Sync's XML/Base64 metadata pipe, sends
Last.fm “Now Playing” updates, and attempts to scrobble recognized tracks.

The package manifest is at `lastfm-scrobbler/Cargo.toml`; its implementation is
in [`src/main.rs`](../lastfm-scrobbler/src/main.rs). Run the following commands
from the workspace root.

## Requirements

- Shairport Sync configured to write metadata to a named pipe.
- A Last.fm account and API key/shared secret.
- The Last.fm account username and password used for password authentication.

The application reads `/tmp/shairport-sync-metadata` by default. Set
`SHAIRPORT_PIPE` if Shairport Sync uses a different pipe path.

## Build

```bash
cargo build --release -p shairport-scrobbler
sudo install -m 755 target/release/shairport-scrobbler /usr/local/bin/
```

## Configure systemd

Create `/etc/systemd/system/shairport-scrobbler.service` and replace the
placeholder values with your Last.fm credentials:

```ini
[Unit]
Description=Shairport Sync Last.fm Scrobbler
After=shairport-sync.service
Requires=shairport-sync.service

[Service]
Type=simple
Environment="LASTFM_API_KEY=your_api_key"
Environment="LASTFM_API_SECRET=your_api_secret"
Environment="LASTFM_USERNAME=your_username"
Environment="LASTFM_PASSWORD=your_password"
# Optional; defaults to /tmp/shairport-sync-metadata.
# Environment="SHAIRPORT_PIPE=/path/to/shairport-sync-metadata"
ExecStart=/usr/local/bin/shairport-scrobbler
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Protect the service file because it contains credentials, then enable the
service:

```bash
sudo chmod 600 /etc/systemd/system/shairport-scrobbler.service
sudo systemctl daemon-reload
sudo systemctl enable --now shairport-scrobbler
```

## Behavior

- Reads Shairport Sync metadata items from the named pipe and decodes Base64
  values.
- Uses the `core` DMAP fields `minm` (title), `asar` (artist), and `asal`
  (album). The `ssnc` events `mdst` and `mden` delimit metadata blocks.
- Sends a “Now Playing” update when a new track is recognized. The `pend` and
  `pfls` events mark playback stopped or paused; `prsm` and `pbeg` mark it
  resumed or started.
- Attempts one scrobble when at least 30 seconds have elapsed since the track
  was recognized and playback is marked active. Time spent paused still counts
  toward that elapsed-time threshold.
- Retries opening the metadata pipe every three seconds if it is unavailable.

If Spotify is also linked to Last.fm and streamed to the Pi over AirPlay,
disable one of the scrobbling paths to avoid duplicate Spotify scrobbles.
