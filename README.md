# rustdm (`rdm`)

A fast, resilient, segmented download engine and command-line utility for Windows 11 in Rust, inspired by Internet Download Manager (IDM).

## Features

- **Multi-Connection Segmented Downloads:** Splits downloads into parallel byte ranges (`[start, end)`) over HTTP(S) with `If-Range` validators.
- **Dynamic Work-Stealing:** Fast connections steal the upper half of the slowest remaining segment ($\ge 2\text{ MiB}$), keeping bandwidth fully saturated.
- **Direct-to-Disk Offset Writes:** Preallocates `<final>.part` and writes slices directly into position without temporary slice files or slow post-download stitching.
- **Crash Recovery & State Checkpointing:** Saves atomic JSON state (`<final>.part.rdm`) periodically and on pause. Killing the process at any moment and running `resume` completes the file byte-for-byte.
- **Single-Connection Fallback:** Gracefully falls back to single-connection streaming if the server ignores byte ranges (`200 OK`) or uses chunked transfer encoding.
- **Path Sanitization & Collision Safety:** Strips illegal Windows characters, protects reserved device names (`CON`, `PRN`, `NUL`), prevents directory traversal, and numbers collision duplicates (`file (1).iso`).
- **Resilient Error Policy:** Exponential backoff with jitter on transient drops, `Retry-After` compliance on 429/503 responses, and clean pause on 403/404/410 link expiry.

## Building

Requires Rust stable (2021 edition):

```bash
cargo build --release
```

The resulting binary will be in `target/release/rdm` (or `target/release/rdm.exe` on Windows).

## Testing

Run unit and integration tests (including the in-repo mock server and crash recovery suite):

```bash
cargo test --features testing
```

Run formatting and clippy checks:

```bash
cargo fmt --check
cargo clippy --all-targets --features testing -- -D warnings
```

## Quick Start & Demo

Start the throttled 64 MiB demo server in one terminal:

```bash
cargo run --features testing --example serve
```

In another terminal, run `rdm` to download the demo file:

```bash
cargo run -- download http://127.0.0.1:8080/demo.bin --connections 4
```

To test pause and resume, press `Ctrl+C` while downloading, then resume with:

```bash
cargo run -- resume path/to/demo.bin.part.rdm
```

## Milestone 1 Scope & Limitations

Milestone 1 implements the complete core download engine and CLI. The following features are planned for future milestones:
- Graphical User Interface (GUI)
- Browser extension integration & native messaging
- Multi-file download queues & bandwidth speed limiters
- Automatic re-resolution of expired links
- HTTP/3 and FTP support
