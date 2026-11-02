# rustdm: where we are and what's next

Updated: 2026-11-02

## Done

- **Milestone 1 Complete:** Segmented download engine and `rdm` CLI utility fully implemented. PR #4 merged into `main`.
- **Milestone 2 - Task 1 Complete (Pre-Flight Disk Space Check):**
  - **Pull Request #5 opened & passing:** `feat: pre-flight disk space check and Milestone 2 roadmap` on branch `feat/pre-flight-disk-check`.
  - Implemented `src/disk.rs` with `available_space` (cross-platform: native `GetDiskFreeSpaceExW` via `kernel32` on Windows, `statvfs` on Unix) and `ensure_disk_space`.
  - Automatically walks directory ancestors if the target folder or file does not exist yet.
  - Integrated into `src/engine.rs::download()` before `.part` creation to prevent wasted bandwidth or sparse-allocation out-of-space crashes.
  - Added `Mode::SimulateSize(u64)` to test server and added integration test verifying no files are created when space is inadequate.
- **Testing:** 64 automated tests passing across unit, integration, and crash test suites on both Windows and Linux CI.

## Milestone 2 Architecture & Status

1. **[x] Task 1: Pre-Flight Disk Space Check (PR #5)**
   - Query drive free space before creating part files or allocating disk blocks.
   - Reject / pause before downloading if `available_bytes < total_bytes + safety_margin`.

2. **[ ] Task 2: Background Daemon & Local IPC (`rdmd`) (Up Next)**
   - Standalone background service running the download engine independently of any active terminal window.
   - IPC transport: Windows Named Pipes (`\\.\pipe\rustdm`) and Unix Domain Sockets (`/tmp/rustdm.sock`).
   - JSON-RPC protocol for commands (`add`, `pause`, `resume`, `cancel`, `list`, `subscribe_telemetry`) and real-time event streaming (`telemetry.tick`).
   - Auto-spawning daemon: `rdm` CLI launches `rdmd` in the background if not already running.

3. **[ ] Task 3: Multi-Download Queue Manager**
   - Queue state machine (Active, Queued, Paused, Completed, Failed).
   - Global bandwidth / concurrent download scheduler.
   - Persistence of the queue across daemon restarts.

4. **[ ] Task 4: Interactive Terminal UI (TUI) Dashboard (`rdm` / `rdm tui`)**
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
