# audiotools-rs

## Table of Contents

- [Install](#install)
- [maintain-album-art](#maintain-album-art)
- [scale-cover](#scale-cover)
- [lastfm-scrobbler](#lastfm-scrobbler)
- [stream-arbiter](#stream-arbiter)
- [volume-arbiter](#volume-arbiter)

## Install

From the workspace root, install each application by path:

```bash
cargo install --path maintain-album-art
cargo install --path scale-cover
cargo install --path lastfm-scrobbler
cargo install --path stream-arbiter
cargo install --path volume-arbiter
```

## maintain-album-art

`maintain-album-art` scans a directory tree for `.flac` and `.mp3` files and reports whether each file contains embedded album art in its metadata.

See [docs/maintain-album-art.md](docs/maintain-album-art.md) for full usage,
checks, options, and output details.

Quick run:

```bash
cargo run -p maintain-album-art -- ~/Music
```

## scale-cover

`scale-cover` uses `folder.jpg` as the source image and produces `front.jpg`
as a standardized `1000x1000` JPEG. It rejects invalid JPEG input and images
smaller than `1000x1000`.

See [docs/scale-cover.md](docs/scale-cover.md) for full behavior, failure
conditions, and logging details.

Quick run (from a directory containing `folder.jpg`):

```bash
cargo run -p scale-cover
```

## lastfm-scrobbler

See [docs/lastfm-scrobbler.md](docs/lastfm-scrobbler.md) for setup and service instructions.

## stream-arbiter

`stream-arbiter` watches Snapcast stream activity and switches selected groups
to the highest-priority active source.

Quick run:

```bash
cargo run -p stream-arbiter
```

## volume-arbiter

`volume-arbiter` bridges source volume to Snapcast client/group volume:
MPD volume is read directly from MPD, and AirPlay volume can be read from a
Shairport-updated file.

Quick run:

```bash
cargo run -p volume-arbiter
```
