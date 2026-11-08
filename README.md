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

Downloads cope with bad networks on their own: a connection that goes silent for 30 s is dropped and reopened, a connection that keeps dropping carries on as long as data still arrives, and a download that fails on network or server trouble goes to `retrying` and tries again by itself (after 3 s, 6 s, 12 s, ... up to 5 times in a row without progress). Missing download folders are created. A paused download from a server that cannot send part of a file starts over instead of failing.

Only you can talk to your daemon: it listens on a per-user pipe (`\\.\pipe\rustdm-<user>`) or socket (`$XDG_RUNTIME_DIR/rustdm.sock`, or `/tmp/rustdm-<uid>.sock`), and `rdm` refuses to connect to one owned by another user. A cancelled download can be resumed with `rdm resume <id>` (it starts over if `--delete` removed its files); in the dashboard, `c` asks before cancelling. `rdmd` writes its log to `rdmd.log` in its data folder.

`rdm daemon stop` pauses every active download and saves its state before the daemon exits. Use `--endpoint <socket path or pipe name>` to talk to a daemon other than the default one.

Downloads wait in a queue: by default 3 run at once, in list order. Lowering `--max-active` sends the downloads furthest down the list back to the queue; they resume where they stopped once a slot frees up. A resumed download uses the same number of connections it started with. The queue and limits are saved to `queue.json` (in `%LOCALAPPDATA%\rustdm`, `~/.local/state/rustdm`, or `$RUSTDM_DATA_DIR`), so after a restart the daemon picks up where it left off.

```bash
rdm limits                                    # show the current limits
rdm limits --max-active 2 --max-speed 2M      # 2 at once, 2 MiB/s combined; --max-speed off removes the cap
rdm add https://example.com/a.iso --max-speed 500K   # cap one download
rdm speed dl_2 1M                             # change one download's cap; 0 or off removes it
rdm move dl_5 top                             # reorder the queue: top, up, down, bottom
rdm cancel dl_4 --delete                      # also delete the partial files
rdm remove dl_4                               # drop a finished or cancelled download from the list
```

## Dashboard

Run `rdm` with no command (or `rdm tui`) for a live dashboard of the daemon's downloads. It starts the daemon if needed.

- **Top pane:** every download in queue order, with status tabs (All, Downloading, Queued, Completed, Stopped) and counts.
- **Bottom pane:** the selected download's details, a segment map of what is on disk, one bar per connection, and a speed graph.
- **Mouse:** click the toolbar buttons (`[ + Add ]`, `[ || Pause ]`, `[ > Resume ]`, `[ x Cancel ]`, `[ - Remove ]`), tabs, and rows; the wheel scrolls.
- **Keys:** `a` add (or paste a URL), `p` pause, `r` resume, `c` cancel, `x` remove, `Shift+↑/↓` or `K`/`J` reorder the queue, `←/→` or `1`-`5` tabs, `↑/↓` select, `q` quit.

When output is not a terminal, or with `rdm tui --plain`, it prints a plain row each time a download changes and exits once nothing is running or queued, so scripts can use it.

## Milestone Roadmap

- **Milestone 1 (Complete):** Core multi-connection segmented download engine, dynamic work-stealing, direct-to-disk positional writes, atomic crash recovery, and `rdm` CLI utility.
- **Milestone 2 (Complete):**
  - **Pre-flight disk space check:** Verifies available drive space before allocating or writing.
  - **Background Daemon (`rdmd`) (Done):** Runs downloads in the background over local IPC (Windows Named Pipes / Unix sockets), surviving terminal closure, controlled with `rdm add/list/pause/resume/cancel/daemon`.
  - **Multi-Download Queue (Done):** At most N downloads at once, global and per-download speed caps, queue reordering, and a queue that survives daemon restarts.
  - **Interactive Terminal UI (TUI) (Done):** Mouse-clickable buttons and tabs, browsable downloads table, live segment map, connection lanes, and speed graph using Ratatui, with a plain-text fallback for scripts.
- **Terminal-first interface (Designed, not started):** the TUI becomes rustdm's complete interface, with Downloads, Settings, Logs and Help screens, dialogs for adding and managing downloads, and defaults for new downloads kept by the daemon. CLI subcommands stay for scripts. Design: [`docs/superpowers/specs/2026-11-09-rustdm-terminal-first-design.md`](docs/superpowers/specs/2026-11-09-rustdm-terminal-first-design.md); plan: [`docs/superpowers/plans/2026-11-09-rustdm-terminal-first.md`](docs/superpowers/plans/2026-11-09-rustdm-terminal-first.md).
- **Later (Planned):**
  - Browser extension integration & native messaging host, feeding downloads to the daemon.
  - HLS/DASH stream downloads.
  - A desktop GUI on the same daemon API, if one is needed.

## License

Copyright (C) 2026 ChinthanaSathyajith

rustdm is free software: you can redistribute it and/or modify it under the terms of the
GNU General Public License as published by the Free Software Foundation, either version 3
of the License, or (at your option) any later version.

rustdm is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without
even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
[GNU General Public License](LICENSE) for more details.
