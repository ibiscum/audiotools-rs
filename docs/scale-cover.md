# scale_cover

`scale_cover` standardizes cover images in the current directory by using
`folder.jpg` as source and producing `front.jpg` as exact `1000x1000` JPEG.

## Purpose

- Enforce a consistent output cover image (`front.jpg`) at `1000x1000`.
- Keep a cleaned copy of the original image as `folder.jpg`.
- Reject invalid or undersized source images early.
- Write outputs atomically using temporary files to avoid partial writes.

## Input and output

- Fixed input: `folder.jpg`
- Fixed output: `front.jpg`

This binary does not take command-line arguments.

## Processing pipeline

1. Validate that `folder.jpg` is a decodable JPEG.
2. Re-encode `folder.jpg` in-place as clean JPEG (4:2:0 subsampling).
3. Verify source dimensions are at least `1000x1000`.
4. Atomically copy cleaned `folder.jpg` to `front.jpg`.
5. Load `front.jpg`, crop a square from top-left using `min(width, height)`.
6. Resize cropped square to exactly `1000x1000` with `Lanczos3`.
7. Re-encode and atomically replace `front.jpg` with the scaled JPEG.

## Run

From the workspace root (directory must contain `folder.jpg`):

```bash
cargo run -p scale_cover
```

Optional build + run:

```bash
cargo build -p scale_cover
./target/debug/scale_cover
```

## Logging

`scale_cover` uses standard leveled logging (`error`, `warn`, `info`, `debug`,
`trace`) and defaults to `info`.

Use `RUST_LOG` to change verbosity:

```bash
RUST_LOG=debug cargo run -p scale_cover
RUST_LOG=trace cargo run -p scale_cover
```

## Failure conditions

The tool returns an error when:

- `folder.jpg` is missing or unreadable.
- `folder.jpg` is not a valid/decodable JPEG.
- JPEG colorspace is not RGB.
- Source dimensions are smaller than `1000x1000`.
- Atomic copy/rename steps fail.

On failure, temporary files (for example `*.tmp`) are cleaned up when possible.
