# rustdm milestone 1: segmented download engine

Status: implemented and verified (2026-10-31), PR #4. Date: 2026-10-13. Updated 2026-10-14 with IDM wire findings (section 10).

## 1. Intent

rustdm is a lightweight, open-source download manager for Windows 11, in the
spirit of IDM but open and more reliable. Milestone 1 builds the **engine** as a
command-line tool: it downloads one file over HTTP(S) using several connections,
survives pause and crashes, and writes the result safely.

Success means all of these hold:

- A multi-connection download of a real file produces a byte-identical copy.
- Killing the process at any moment and running `resume` finishes the file correctly.
- When one connection finishes early, it takes over half of the slowest segment.
- Servers that misbehave (ignore Range, lie about length, drop connections) produce a correct file or a clear error, never a silently corrupt one.
- The author understands every core piece, and the repo reads as a portfolio project: tests, CI on Windows, a design doc, and a short demo.

Out of scope for milestone 1: GUI, browser extension, native messaging, BLAKE3,
archives, deduplication, HTTP/3, io_uring/IOCP tuning, queues or multi-file jobs,
FTP, proxies, bandwidth limits, scheduling, and re-resolving expired links.

## 2. Stack and dependency policy

- **Rust**, stable toolchain, Windows 11 as the target. A `lib` crate holds the engine and a thin `bin` holds the CLI, so a GUI can reuse the engine later.
- **tokio** for async tasks; **reqwest** (rustls) as the HTTP client. reqwest does the HTTP, TLS, redirects and keep-alive pooling, which we do not rewrite.
- **We write ourselves:** probing logic, segment math, scheduler and work-stealing, offset writer, state file, crash recovery, path templating and sanitizing.
- **Ask-first rule:** adding `reqwest` or any other crate that does core download work needs the author's explicit yes at the point we add it. Small utility crates (`serde`, `serde_json`, `clap`, `url`, `thiserror`) are listed in the plan and confirmed there.
- The test server is hand-written on raw `tokio` TCP, so tests control every byte, including lies.

## 3. Architecture

```
CLI (clap) ── start/resume/pause ──►  Engine
                                        │
   Probe ─► Plan ─► Scheduler ◄──────► Workers (N tokio tasks)
                      │                    │ stream chunks
                      │                    ▼
                      │                 Writer (per-worker file handle, offset writes)
                      ▼
                   StateStore (completed ranges, atomic JSON)
```

Modules (one purpose each, each testable alone):

| Module | Job |
|---|---|
| `probe` | One request to learn size, range support, validators, final URL. |
| `ranges` | Pure math on half-open byte ranges: set of completed ranges, complement, split. No I/O. |
| `scheduler` | Owns the segment table, hands out work, performs work-stealing. |
| `worker` | Fetches one range with a `Range` header, streams it, calls the writer, reports progress. |
| `writer` | Preallocates the file, writes a chunk at an offset, flushes. |
| `state` | Saves and loads the state file atomically. |
| `template` | Builds and sanitizes the destination path. |
| `error` | One error type that separates retryable, fatal, and "link expired". |

## 4. Core mechanisms

### 4.1 Probe

- Send `GET` with `Range: bytes=0-0` and `Accept-Encoding: identity`. This works on servers that reject HEAD, and `identity` stops compressed transfers from breaking offsets and lengths.
- `206` plus `Content-Range: bytes 0-0/TOTAL` means ranges work and TOTAL is the size.
- `200` means the server ignored Range. Fall back to **one connection** and stream the whole body.
- IDM does not probe this way. Its first request is a plain GET with no Range header, which it cuts off after about 240 KB once its other connections are running (section 10). The probe choice is an open decision (section 8).
- `416`, or a missing or non-numeric total, means we cannot segment. Fall back to one connection; with no length, progress is unknown.
- Record the final URL after redirects, a **strong** `ETag` if present, and `Last-Modified`.
- Every later range request sends `If-Range: <validator>`. If the file changed, the server answers `200` with the whole body. That is detected and treated as "file changed", never written at an offset.

### 4.2 Ranges and segments

- All ranges are half-open `[start, end)` in `u64`.
- Source of truth for progress is a **set of completed ranges**, merged when adjacent. The state file stores this set; there are no per-worker cursors on disk.
- Initial plan: split `[0, total)` into `min(connections, total / MIN_SEGMENT)` segments. Defaults: 8 connections, at most 16, `MIN_SEGMENT` 1 MiB.
- We start all segments at once. IDM ramps up instead: 1 connection, then 4, then 8 about 1.4 to 1.9 s later (section 10). Whether we ramp is an open decision (section 8).
- Resume rebuilds segments from the **missing** ranges (the complement of the completed set).

### 4.3 Writer

- The output is `<final name>.part`, preallocated to the full size with `File::set_len`. It is renamed to the final name only after every range is complete and the state file is removed.
- Each worker opens **its own handle** and writes with `seek_write` at the exact offset. No shared cursor, no locking around the file, no temp files to stitch.
- Writes run on `spawn_blocking` so file I/O never stalls the async runtime. Revisit only if a benchmark shows a problem.
- `set_len` may zero-fill on NTFS and be slow for large files. `SetFileValidData` is faster but needs elevated privilege and can expose stale disk data, so it is not used. We measure the real cost before deciding anything else.

### 4.4 Work-stealing

- Each active segment tracks `cursor` (next byte to write) and `end` (atomic).
- When a worker finishes, the scheduler picks the active segment with the **longest estimated time left**, where `eta = (end - cursor) / measured_speed`.
- If `end - cursor >= 2 * MIN_SEGMENT` (2 MiB), the scheduler lowers the victim's `end` to the midpoint and gives `[midpoint, old_end)` to the idle worker. Smaller leftovers are not split.
- The victim clamps every chunk to its current `end`. A chunk written just before the shrink may overlap the thief's range; the thief then rewrites identical bytes. This is harmless because the `If-Range` check guarantees the file is unchanged, and the completed-range set is idempotent.
- IDM also produces overlapping ranges (two requests ran to end of file in one run), so overlap is normal, not only an error case. Its first split point moved by a few KB between runs (about 233 KB and 237 KB on a 16 MB file), so split points are not fixed. See section 10.

### 4.5 State and crash safety

- State file: `<final name>.part.rdm`, JSON: version, URL, final URL, total size, validator, completed ranges, output path.
- **Ordering rule:** snapshot the completed set, then `sync_data` the output file, then write the snapshot to a temp file and rename it over the state file. A range is never recorded as done unless its bytes were flushed first.
- Save every 4 MiB of progress or 2 seconds, whichever comes first, and on pause.
- On `resume`: load state, probe again, compare size and validator. A mismatch aborts with a clear message instead of mixing two versions of a file.
- A corrupt or truncated state file is reported, never silently ignored.

### 4.6 Pause, resume, and failures

- `Ctrl+C` is a pause: cancel the workers, flush, save state, exit with a "paused" message. `resume <state-file>` continues.
- Per-segment retries with exponential backoff and jitter. A connection drop mid-segment resumes from the last written byte, not the segment start. `429` and `503` honor `Retry-After`.
- When a server refuses extra connections with `503`, IDM retries in tight bursts with no visible backoff, then every few seconds, and still completes. We keep our backoff and `Retry-After` rules; the wire run shows IDM does not do this, so it is not a model to copy (section 10).
- `403`, `404`, or `410` on a previously working link is reported as "link expired or removed" and the download pauses with state saved. Supplying a fresh URL is a later milestone.
- Short body, long body, or wrong `Content-Range` is a protocol error for that segment: discard, retry, and fail after a cap.

### 4.7 Path templating and sanitizing

- Default template: `{downloads}\{name}`. Tokens: `{downloads}`, `{host}`, `{name}`, `{stem}`, `{ext}`, `{date}`.
- Name priority: `Content-Disposition` `filename*` (RFC 5987/6266), then `filename`, then the last URL path segment (percent-decoded), then `download`.
- Each token value is sanitized **before** it is joined:
  - strip control characters; replace `< > : " / \ | ? *` with `_`;
  - drop `.` and `..` components; trim leading and trailing dots and spaces;
  - prefix reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, with or without an extension) with `_`;
  - cap each component at 255 characters and the full path under the Windows limit, trimming the stem and keeping the extension.
- After building the path, verify it is still inside the base directory; reject it otherwise.
- Never overwrite: on collision append ` (1)`, ` (2)`, and so on.

## 5. CLI (milestone 1)

```
rdm download <URL> [--out <template>] [--connections N]
rdm resume <path-to-.part.rdm> [--url <URL>]
```

Progress goes to the terminal (bytes, speed, active segments). Logs go through `tracing`; no `println!` debugging left in library code.

## 6. Testing strategy

Each piece is built test-first and passes before the next one starts.

- **Unit, pure:** range-set merge and complement, splitter boundaries, sanitizer cases (table-driven), template expansion.
- **Test server** (raw tokio TCP) with switchable behaviors: normal; ignores Range (`200`); `206` with wrong `Content-Range`; short body; long body; no `Content-Length` (chunked); drops the connection mid-body; per-connection throttle (to prove work-stealing helps); changes `ETag` between sessions; `403` after N requests (expired link); refuses connections beyond N with `503` (added from the IDM wire runs, section 10).
- **Integration:** download the same random file in every mode and compare SHA-256 with the source.
- **Crash test:** start a download in a child process, kill it at random points, resume, compare checksums. Repeat many times.
- Coverage target 80% on the library crate. CI on GitHub Actions `windows-latest` plus Linux.

## 7. Build order

Each step starts with a short explanation of the concept, then tests, then code.

1. `ranges`: set and splitter (pure math).
2. `template`: sanitizer and path building.
3. Test server and its misbehaving modes.
4. `probe`.
5. `writer`: preallocation and offset writes.
6. A single-connection download end to end.
7. Scheduler with N workers.
8. Work-stealing.
9. `state` and `resume`.
10. Pause and the crash test.
11. CLI polish, README, demo.

## 8. Decisions made on the author's behalf (redirect any of them)

- CLI first; the GUI comes after the engine is proven.
- Output goes to `.part` and is renamed on success.
- "Slowest" means longest estimated time left, not most bytes left.
- `set_len` for preallocation; no `SetFileValidData`.
- One file handle per worker with `seek_write`; progress stored as a completed-range set.
- `Accept-Encoding: identity` and `If-Range` on every range request.
- Test server written from raw TCP, not a framework.
- Expired-link refresh and cookie/auth persistence are deferred.
- **Decided by the author (2026-10-14):** (a) keep the `bytes=0-0` probe, which IDM does not use (section 4.1); (b) start all segments at once. The 1, 4, 8 ramp that IDM uses (section 4.2) stays a later option, to be added if measurement shows it helps.
- **Build location:** WSL for steps 1 to 3, then the Windows MSVC toolchain before step 5 (`next-steps.md`).

## 9. Open questions

- Exact default connection count; tune after measuring on the author's network (single connections are slow here, parallel ones scale well).
- Whether antivirus slows `.part` writes on the author's machine; measure, then decide.
- Still open from the IDM study: resume after killing IDM, wrong bytes in the overlap, a Range error status, a stalled connection, a mid-body drop by the server, and a changed file between sessions (section 10). The test-case list is in `docs/idm-behavior-checklist.md`. No code or findings from disassembly belong in this repo.

## 10. Reference: IDM behavior on the wire (2026-10-14)

Five runs of IDM 6.43 against the local test server (16 MB file, 256 KB/s per connection): three with default settings, one with the server ignoring Range, one with the server capping connections at 4. Behavior only; the full list and the test-case wording are in `docs/idm-behavior-checklist.md`. Five runs is a small sample.

- **First request:** a plain GET with no Range header, cut off after about 240 KB once the other connections were running.
- **Connections:** 1, then 4 within about 15 ms, then 8 about 1.4 to 1.9 s later. The 1 to 2 step was not seen, and one extra connection opened about 0.9 s after the first group with no clear cause.
- **Splits:** the first split point was about 233 KB in two runs and 237 KB in a third. Ranges overlapped at the tail, with two requests running to end of file.
- **User-Agent:** the built-in default on every request. No browser string was involved.
- **Headers:** Host, User-Agent, Accept, `Accept-Encoding: identity`, Accept-Language, Accept-Charset, Referer, Range, and Cookie when the host had browser cookies. No Connection header.
- **Server ignores Range:** one connection carried the whole file. IDM also sent two open-ended Range requests and closed them after 16 KB.
- **Server caps at 4:** extra requests got `503` in tight bursts. IDM kept retrying and the download completed.
- **Integrity:** file size matched in every run; SHA-256 matched in the first two.

**Not yet tested:** resume after killing IDM; wrong bytes in the overlap; a Range request answered with an error status; a stalled connection; a mid-body drop by the server; a changed file between sessions.
