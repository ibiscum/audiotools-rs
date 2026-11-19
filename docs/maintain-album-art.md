# maintain-album-art

`maintain-album-art` scans a directory tree for `.flac` and `.mp3`
music files and audits album-art completeness at both track and directory
level.

## Purpose

- Audit embedded cover art quality for large music libraries.
- Detect tracks missing embedded artwork.
- Enforce a strict embedded front-cover target of `1000x1000`.
- Validate directory-level image conventions (`front.jpg` and `folder.jpg`).

## What it checks

### Per-track checks

- FLAC: reads metadata with `metaflac`.
- MP3: reads metadata with `id3`.
- For each track, reports:
  - `[FRONT 1000x1000]` if a front cover image exactly `1000x1000` is embedded.
  - `[HAS ART, NO FRONT 1000x1000]` if art exists but no qualifying front cover.
  - `[MISSING]` if no embedded artwork is present.
  - `[ERROR]` when metadata cannot be read.

### Per-directory checks

For directories containing audio files, it inspects image files in that same
directory and tracks:

- `front.jpg` must be exactly `1000x1000`.
- `folder.jpg` must be at least `1000x1000`.

A directory is marked `COMPLETE OK` only when both file-level and
directory-level checks pass.

## Usage

From the workspace root:

```bash
cargo run -p maintain-album-art -- <directory-to-scan>
```

Example:

```bash
cargo run -p maintain-album-art -- ~/Music
```

Show help:

```bash
cargo run -p maintain-album-art -- --help
```

## Options

- `--skip-complete`: omit fully complete directories and suppress OK track
  lines/counts from summary output.

## Logging

Logging is structured and leveled (`INFO`, `WARN`, `ERROR`, `DEBUG`, `TRACE`).
Control verbosity via `RUST_LOG`:

```bash
RUST_LOG=debug cargo run -p maintain-album-art -- ~/Music
RUST_LOG=trace cargo run -p maintain-album-art -- ~/Music
```

## Exit behavior

- Exits with usage info when required arguments are missing.
- Exits on invalid argument combinations (for example, multiple directories).
- Prints a final summary and directory audit status when the scan completes.
