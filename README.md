# audiotools-rs

## Table of Contents

- [Install](#install)
- [maintain-album-art](#maintain-album-art)
- [scale-cover](#scale-cover)
- [lastfm-scrobbler](#lastfm-scrobbler)

## Install

From the workspace root, install each application by path:

```bash
cargo install --path maintain-album-art
cargo install --path scale-cover
cargo install --path lastfm-scrobbler
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
