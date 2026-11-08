# rustdm: where we are and what's next

Updated: 2026-11-06

## Done

- **Milestone 1 Complete:** Segmented download engine and `rdm` CLI utility fully implemented. PR #4 merged into `main`.
- **Milestone 2 - Task 1 Complete (Pre-Flight Disk Space Check):**
  - **Pull Request #5 merged:** `feat: pre-flight disk space check and Milestone 2 roadmap`.
  - Cross-platform pre-flight disk space checking (`src/disk.rs` via `GetDiskFreeSpaceExW` on Windows and `statvfs` on Unix) integrated into `src/engine.rs` before preallocation.
- **Milestone 2 - Task 2 (Background Daemon & Local IPC - Phase 1 Complete):**
  - **Pull Request #6 merged:** `feat: background daemon core and cross-platform IPC engine`.
  - **Substep 2.1 (Wire Protocol):** JSON-RPC 2.0 protocol models with comprehensive error taxonomy covering 20+ edge cases (`src/ipc/protocol.rs`).
  - **Substep 2.2 (Cross-Platform IPC Transport):** Windows Named Pipes (`\\.\pipe\rustdm`) with `ERROR_PIPE_BUSY` (231) retry logic and Unix Domain Sockets (`/tmp/rustdm.sock`) with crash recovery and auto-cleanup (`src/ipc/transport.rs`).
  - **Substep 2.3 (Daemon Core & Engine Bridge):** In-memory multi-task manager coordinating background downloads, non-blocking pause/resume, and real-time telemetry streaming over IPC (`src/daemon/core.rs`).
- **Milestone 2 - Task 2 Complete (Daemon Binary & CLI Client, branch `feat/daemon-cli`):**
  - **Substep 2.4 (`rdmd`):** `src/bin/rdmd.rs` with the service loop in `src/daemon/service.rs`. An exclusive OS file lock (`<endpoint>.lock`) keeps a second daemon off the endpoint. Ctrl+C, SIGTERM, Ctrl+Break, console close, or `daemon.shutdown` pause every active download and give it up to 10 s to save its `.part.rdm` state. SIGHUP is ignored so the daemon outlives its terminal.
  - **Substep 2.5 (IPC client & CLI):** `src/ipc/client.rs` (`DaemonClient`, `connect_or_spawn`) and `src/daemon/launch.rs` (detached spawn of `rdmd` from the same folder as `rdm`). New `rdm` commands: `add`, `list`, `pause`, `cancel`, `resume <dl_N>`, `daemon start|status|stop`, plus a global `--endpoint`. `rdm resume` still accepts a `.part.rdm` file for foreground resumes.
  - **Daemon core fixes:** a cancelled download no longer turns into "paused" when its engine stops; a shutdown arriving between accept-loop iterations is no longer missed; `list` shows total size, percent, and ETA (the engine now publishes `Progress::total`).
- **Milestone 2 - Task 3 Complete (Multi-Download Queue, branch `feat/download-queue`, stacked on `feat/daemon-cli`):**
  - **Queue:** `rdm add` queues downloads; at most `max_active` (default 3) run at once, oldest first. Pausing a queued download takes it out of the queue; `rdm resume` puts it back. The core is split into `src/daemon/core.rs` (queue rules and scheduler), `src/daemon/task.rs` (one download's record), and `src/daemon/server.rs` (RPC dispatch and accept loop).
  - **Global bandwidth cap:** `src/limiter.rs` is a token bucket shared by every connection of every download (one-second burst). Throttle waits check the cancel flag every 100 ms, and the lowest accepted cap is 16 KiB/s, so pause and cancel stay responsive.
  - **Persistence:** `src/daemon/queue_store.rs` saves the queue and limits atomically to `queue.json` in `--data-dir` / `$RUSTDM_DATA_DIR` / `%LOCALAPPDATA%\rustdm` / `~/.local/state/rustdm` on every change. After a restart, downloads that were running or queued are queued again and resume from their `.part.rdm` files; user-paused ones stay paused. A corrupt queue file is renamed to `queue.json.corrupt` instead of being overwritten.
  - **New RPCs and commands:** `daemon.get_limits` / `daemon.set_limits` (`rdm limits --max-active N --max-speed 2M`), `download.remove` (`rdm remove <id>`), and `rdm cancel --delete`, which now deletes the right `.part` / `.part.rdm` files once the engine has stopped. The engine publishes the chosen output path early, so `rdm list` shows the real file name while downloading.
  - **Follow-ups:** lowering `max_active` requeues the extras, `rdm move` reorders the queue, `rdm speed` / `rdm add --max-speed` cap one download, and resumes keep the original connection count.
- **Milestone 2 - Task 4 Complete (Terminal Dashboard, branch `feat/tui`, stacked on `feat/download-queue`):**
  - `rdm` with no command, or `rdm tui`, opens a ratatui dashboard (`src/tui/`): `app.rs` holds the state and turns keys and clicks into actions, `ui.rs` draws it and records click targets, `format.rs` holds the text helpers shared with `rdm list`, and `mod.rs` runs the event loop against the daemon.
  - Top pane: downloads in queue order with status tabs; bottom pane: details, segment map, one bar per connection, and a speed graph. Toolbar buttons, tabs, and rows are clickable; the wheel scrolls; pasting a URL adds it; `Shift+↑/↓` reorders the queue.
  - Telemetry ticks now carry one entry per connection lane (`Progress::lane_snapshots`), on a dedicated connection that reconnects if the daemon restarts. The dashboard also survives a daemon restart.
  - Not a terminal, or `--plain`: prints a row whenever a download changes and exits once nothing is running or queued.
- **Hardening, batch 1 (network resilience, branch `feat/network-resilience`):** HTTP connect and read timeouts (30 s) so a silent connection is reopened; pause interrupts any wait; a connection's retry count resets whenever it makes progress; the daemon retries failed downloads by itself (`retrying`, 3 s doubling to 2 min, 5 in a row without progress); missing output folders are created and file errors name the path; resumes that the server cannot serve start over; speed and ETA are averaged over 3 s.
- **Hardening, batch 2 (safety, branch `fix/safety-hardening`):** requested file names are values for `{name}` (sanitized) instead of template text, and templates expand in one pass, so neither `../` nor a `{token}` inside a name can redirect a download; IPC messages are capped at 8 MiB; per-user pipe/socket names, `rdm` refuses a daemon run by another user (socket owner on Unix, pipe server's account on Windows), and sockets are 0600; a data-folder lock keeps two daemons off one queue; `rdmd` logs to `<data dir>/rdmd.log` (rotated at 5 MiB); cancelled downloads can be resumed; the dashboard asks before cancelling.
- **Testing:** 171 automated tests passing on Linux (unit, integration, crash, daemon process, queue, and CLI end-to-end suites). Windows-only code is type-checked; Windows CI runs on the next push.

## Milestone 2 Architecture & Status

1. **[x] Task 1: Pre-Flight Disk Space Check (PR #5 Merged)**
   - Query drive free space before creating part files or allocating disk blocks.
   - Reject / pause before downloading if `available_bytes < total_bytes + safety_margin`.

2. **[x] Task 2: Background Daemon & Local IPC (`rdmd`)**
   - [x] Substep 2.1: Wire Protocol & Serialization (`src/ipc/protocol.rs` - JSON-RPC 2.0 with comprehensive error taxonomy).
   - [x] Substep 2.2: Cross-Platform IPC Transport Layer (`src/ipc/transport.rs` - Windows Named Pipes & Unix Domain Sockets).
   - [x] Substep 2.3: In-Memory Daemon Core & Engine Controller (`src/daemon/mod.rs` & `src/daemon/core.rs` - multi-task coordinator, pause/resume, live telemetry broadcast).
   - [x] Substep 2.4: Standalone Daemon Executable (`src/bin/rdmd.rs` - background service entrypoint, single-instance lock, graceful signal shutdown).
   - [x] Substep 2.5: IPC Client & CLI Integration (`rdm` auto-spawning, remote control commands: `add`, `list`, `pause`, `resume`, `cancel`, `daemon start/status/stop`).
3. **[x] Task 3: Multi-Download Queue Manager**
   - [x] Queue state machine (Active, Queued, Paused, Completed, Failed, plus Pausing and Cancelled).
   - [x] Global bandwidth / concurrent download scheduler.
   - [x] Persistence of the queue across daemon restarts.
   - [x] Lowering `max_active` stops the downloads furthest down the queue and queues them again; they resume from their state files when a slot frees up.
   - [x] Queue reordering (`download.move`, `rdm move <id> top|up|down|bottom`); the order is saved with the queue.
   - [x] Per-download speed caps (`download.set_speed`, `rdm speed`, `rdm add --max-speed`), applied on top of the global cap and saved with the queue.
   - [x] A resume uses the connection count the download started with (saved in `.part.rdm`; older state files fall back to 8), splitting the largest gaps so every connection has work.

4. **[x] Task 4: Interactive Terminal UI (TUI) Dashboard (`rdm` / `rdm tui`)**
   - Built with `ratatui` + `crossterm`.
   - **Full Mouse Support:** Clickable action buttons (`[ ⏸ Pause ]`, `[ ▶ Resume ]`, `[ ➕ Add ]`, `[ ⏹ Cancel ]`), clickable category tabs, clickable download rows, and mouse-wheel scrolling.
   - **Two-Pane Layout:**
     - Top Pane: Browsable multi-download table listing all files, status, progress, and speed.
     - Bottom Pane: Detailed download inspector for the selected file, including live IDM-style segment map, active connection lanes, and speedometers.
   - **Adaptive Hybrid:** Automatically detects non-TTY or `--plain` flag to fall back cleanly to script-friendly streaming output.

5. **Future Evolution (Milestone 3+)**
   - Desktop Native GUI (Slint/Tauri) connects to the exact same daemon IPC pipe as Approach 2.
   - WebExtension + Native Messaging for automatic browser download interception.
   - Universal network media sniffer & HLS/DASH ffmpeg muxing.
