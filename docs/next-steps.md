# rustdm: where we are and what's next

Updated: 2026-10-31

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

## Next Actions, in Order

1. **Review and Merge PR #4:** Author review of `feat/milestone-1-engine` into `main`.
2. **Design Milestone 2:** Define scope and architecture for the next phase:
   - Option A: Native Desktop GUI (e.g. Slint / Tauri) with real-time speed graphs and active connection monitors.
   - Option B: WebExtension & Native Messaging host (intercepting browser downloads and network media sniffer).
   - Option C: Multi-file download queue, bandwidth throttling, and daemon service.
