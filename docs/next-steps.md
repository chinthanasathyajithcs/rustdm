# rustdm: where we are and what's next

Updated: 2026-11-02

## Done

- **Milestone 1 Complete:** Segmented download engine and `rdm` CLI utility fully implemented.
- **Pull Request #4 opened:** `feat: Milestone 1 segmented download engine` on branch `feat/milestone-1-engine` against `main`.
- **All 14 implementation tasks verified:**
  1. Scaffold & CI: Multi-platform matrix on Windows and Linux (`windows-latest` & `ubuntu-latest`).
  2. Byte ranges and segment math: `ByteRange`, `RangeSet`, `initial_segments`, `split_upper_half`.
  3. Filename sanitizing: Stripping invalid chars, directory traversal prevention, Windows reserved device names (`CON`, `PRN`, `NUL`).
  4. Path expansion & naming: RFC 5987 / Content-Disposition parser, template expansion, collision naming.
  5. Test server (Gate A: `tokio` approved): In-repo raw-TCP mock HTTP/1.1 server.
  6. Probe (Gate B: `reqwest` approved): 1-byte pre-probe (`Range: bytes=0-0`), strong ETag & Last-Modified detection, redirect tracking.
  7. Part file writer: Preallocated `<file>.part` with direct positional writes (`seek_write` on Windows, `write_at` on Unix) and atomic rename finish.
  8. Single-connection fallback: Streaming for servers ignoring byte ranges (`200 OK`) or using chunked streams.
  9. Parallel ranges with If-Range checks: Concurrent workers via `JoinSet`, `If-Range` validation, `FileChanged` drift detection.
  10. Failure policy: Exponential backoff with ±25% jitter, `Retry-After` header compliance, byte-level resume on drops.
  11. Work stealing: Dynamic lane redistribution stealing upper half of slowest remaining segment ($\ge 2\text{ MiB}$).
  12. State & resume: Atomic JSON state checkpointing (`<file>.part.rdm`), resuming from missing byte gaps.
  13. Pause, expired links, CLI & crash test: Clean `Ctrl+C` cancellation, 403 expired link preservation, full `rdm` CLI (`download`, `resume`), and 5-run black-box kill-and-resume crash test.
  14. Polish & demo: Live stderr progress reporter, throttled 64 MiB demo server (`examples/serve.rs`), CI 80% coverage gate, and `README.md`.
- **Testing:** 59 automated tests passing across unit, integration, and crash test suites.

## Milestone 2 Architecture & Scope (Agreed)

Milestone 2 establishes `rustdm` as a decoupled daemon service with an interactive terminal UI (TUI) and engine pre-flight safety checks:

1. **Pre-Flight Disk Space Check (First Task in M2):**
   - Query drive free space before creating part files or allocating disk blocks (`GetDiskFreeSpaceExW` on Windows, `statvfs` on Unix).
   - Reject / pause before downloading if `available_bytes < total_bytes + safety_margin`.
   - Prevent wasting user bandwidth and crashing on sparse-file allocation out-of-space errors.

2. **Background Daemon & Local IPC (`rdmd`):**
   - Standalone background service running the download engine independently of any active terminal window.
   - IPC transport: Windows Named Pipes (`\\.\pipe\rustdm`) and Unix Domain Sockets (`/tmp/rustdm.sock`).
   - JSON-RPC protocol for commands (`add`, `pause`, `resume`, `cancel`, `list`, `subscribe_telemetry`) and real-time event streaming (`telemetry.tick`).
   - Auto-spawning daemon: `rdm` CLI launches `rdmd` in the background if not already running.

3. **Multi-Download Queue Manager:**
   - Queue state machine (Active, Queued, Paused, Completed, Failed).
   - Global bandwidth / concurrent download scheduler.
   - Persistence of the queue across daemon restarts.

4. **Interactive Terminal UI (TUI) Dashboard (`rdm` / `rdm tui`):**
   - Built with `ratatui` + `crossterm`.
   - **Full Mouse Support:** Clickable action buttons (`[ ⏸ Pause ]`, `[ ▶ Resume ]`, `[ ➕ Add ]`, `[ ⏹ Cancel ]`), clickable category tabs, clickable download rows, and mouse-wheel scrolling.
   - **Two-Pane Layout:**
     - Top Pane: Browsable multi-download table listing all files, status, progress, and speed.
     - Bottom Pane: Detailed download inspector for the selected file, including live IDM-style segment map, active connection lanes, and speedometers.
   - **Adaptive Hybrid:** Automatically detects non-TTY or `--plain` flag to fall back cleanly to script-friendly streaming output.

5. **Future Evolution (Milestone 3+):**
   - Desktop Native GUI (Slint/Tauri) connects to the exact same daemon IPC pipe as Approach 2.
   - WebExtension + Native Messaging for automatic browser download interception.
   - Universal network media sniffer & HLS/DASH ffmpeg muxing.
