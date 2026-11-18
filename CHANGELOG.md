# Changelog

## Unreleased

### lastfm-scrobbler

- Switched scrobble delivery to direct-send-first behavior: tracks are sent to
  Last.fm immediately when eligible, with SQLite used as fallback only for
  retriable failures.
- Migrated Last.fm work to an async worker path with coalesced now-playing
  updates from sync input backends.
- Hardened queue durability/operations:
  - SQLite busy timeout
  - WAL mode for file-backed queue DB
  - bounded queue size and retention TTL
  - dead-letter queue for non-retriable, expired, overflow, and retry-exhausted
    items, plus dead-letter TTL purge
- Added regression tests for now-playing coalescing, queue persistence, and
  dead-letter/retention behavior.
- Added multi-source input arbitration with a new `LASTFM_INPUT_BACKEND=auto`
  mode:
  - runs dedicated workers for MPD (idle events) and Shairport backends
  - selects one active source at a time and switches on activity changes
  - avoids duplicate now-playing updates when sources report the same track

### Documentation

- Expanded [lastfm-scrobbler docs](/home/ulf/data/audiotools-rs/docs/lastfm-scrobbler.md) with
  runtime flow details and Mermaid sequence/state diagrams.
- Documented queue hardening settings and defaults.
- Added headless `systemd --user` deployment guidance for MPD, mpdris2,
  lastfm-scrobbler, shairport-sync, snapserver, and snapclient, including
  lingering and package-update persistence notes.
