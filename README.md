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

## Background Daemon

`rdm add` hands a download to the background daemon `rdmd` and starts the daemon if it is not running. Downloads keep going after the terminal closes.

```bash
cargo build                                   # builds both rdm and rdmd
rdm add https://example.com/big.iso --dir ~/Downloads
rdm list                                      # ID, status, progress, speed
rdm pause dl_1
rdm resume dl_1
rdm cancel dl_1
rdm daemon status                             # also: daemon start, daemon stop
```

`rdm daemon stop` pauses every active download and saves its state before the daemon exits. Use `--endpoint <socket path or pipe name>` to talk to a daemon other than the default one.

Downloads wait in a queue: by default 3 run at once, oldest first. The queue and limits are saved to `queue.json` (in `%LOCALAPPDATA%\rustdm`, `~/.local/state/rustdm`, or `$RUSTDM_DATA_DIR`), so after a restart the daemon picks up where it left off.

```bash
rdm limits                                    # show the current limits
rdm limits --max-active 2 --max-speed 2M      # 2 at once, 2 MiB/s combined; --max-speed off removes the cap
rdm cancel dl_4 --delete                      # also delete the partial files
rdm remove dl_4                               # drop a finished or cancelled download from the list
```

## Milestone Roadmap

- **Milestone 1 (Complete):** Core multi-connection segmented download engine, dynamic work-stealing, direct-to-disk positional writes, atomic crash recovery, and `rdm` CLI utility.
- **Milestone 2 (In Progress):**
  - **Pre-flight disk space check:** Verifies available drive space before allocating or writing.
  - **Background Daemon (`rdmd`) (Done):** Runs downloads in the background over local IPC (Windows Named Pipes / Unix sockets), surviving terminal closure, controlled with `rdm add/list/pause/resume/cancel/daemon`.
  - **Multi-Download Queue (Done):** At most N downloads at once, a global speed cap, and a queue that survives daemon restarts.
  - **Interactive Terminal UI (TUI):** Mouse-clickable buttons (`[Pause]`, `[Resume]`, `[Add]`), browsable downloads table, and live IDM-style segment map using Ratatui.
- **Milestone 3+ (Planned):**
  - Native Desktop GUI (Slint/Tauri) connecting to the daemon.
  - Browser extension integration & native messaging host.
  - Universal media sniffer & adaptive HLS/DASH chunk muxing with ffmpeg.
