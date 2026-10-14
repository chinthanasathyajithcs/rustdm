# Improvement ideas

Ideas for making rustdm better than IDM where we can measure it. None of these are scheduled. Milestone 1 scope is in `docs/superpowers/specs/2026-10-13-rustdm-milestone-1-design.md`; items marked **(later)** need work outside it.

Where an idea comes from IDM's observed behavior, the source is `docs/idm-behavior-checklist.md`.

## Engine

- **No repeated bytes.** Stop the victim exactly at the split point, or skip bytes already on disk, so the thief and victim never download the same data. IDM re-downloads overlapping bytes. Measure: bytes received divided by file size.
- **Adaptive connection count.** After repeated `503` responses, lower the connection count to what the server accepts. IDM keeps the same count and retries in bursts. Measure: refused requests per download.
- **Small first request.** Probe with a few bytes (the `bytes=0-0` probe in the spec) instead of a full GET that gets cut off. IDM's first request was cut off after about 240 KB in the wire runs.
- **Configurable ramp-up.** Allow starting with all connections, or stepping up like IDM does. Measure: time to full speed against refusal rate.
- **Splits for small files.** Revisit `MIN_SEGMENT` (1 MiB in the spec). Measure where extra splits stop helping.
- **Hash check.** Verify a finished file against a SHA-256 the user supplies.

## Trust and privacy

- **No phone-home.** Make no network calls except to the download servers.
- **Portable state.** Keep settings and state in one user folder. Don't write the registry.
- **Signed releases.** Sign every release, and never self-update without a signature check.
- **Honest user agent.** Send `rustdm/<version>` by default, with an override for servers that refuse unknown agents.

## Control and transparency

- **Per-host connection limits.** Keep a large download from hammering one server.
- **Speed limits.** Deferred from milestone 1 in the spec; add later.
- **Machine-readable output.** A `--json` status output and a request log, so scripts and bug reports can use the same data.
- **Rules for finished files.** Sort downloads into folders by type or site, and show where each file went.

## Browser side (later)

- **Cookies from the page.** The extension sends the cookies and headers the page is already using. Don't read browser cookie stores.
- **"Send to rustdm" control.** Show it on file links, with the page's referrer and cookies.
- **File list.** List the downloadable files on the current page, so the user can choose.
- **Fresh links.** The extension hands over a new URL when a link expires. This solves the expired-link case the spec defers.

## Architecture

- **Engine as a library.** A GUI, the CLI, and the extension all call the same code. The spec already splits a `lib` crate from a `bin`.
- **Completion hook.** Run a script or command when a download finishes.

## From the external list

**Already in the spec, so no new work**
- Writing each segment at its own offset into one pre-allocated file (spec 4.3). This removes the merge step that the list describes IDM doing. The list's merge cost for IDM is not verified; measure before claiming it.
- Splitting a slow segment when a fast worker is idle (spec 4.4, work-stealing).
- Keep-alive and connection reuse (reqwest, spec section 2).

**Conflicts with a spec decision**
- `SetFileInformationByHandle` for instant allocation. The spec uses `set_len` and rejects `SetFileValidData`, because the same kind of call can expose stale disk data. We keep `set_len` and measure its cost first. This is the author's call to revisit.

**New, later**
- **Magic-byte sniffing.** Read the first 512 bytes to pick the extension when the name is missing or wrong. Small, and it fits the `template` module. Candidate for milestone 1 step 2.
- **In-flight SHA-256.** Segments arrive out of order, so a streaming hash must read in file order. Options: hash each segment once it completes and combine the results, or re-read in order. This needs a design before it's added. BLAKE3 is out of scope for milestone 1.
- **Named pipes for the daemon.** Prefer them to local WebSockets, which need origin checks to be safe. Belongs with the daemon, after the CLI.
- **Archive extraction and playlist merging as completion hooks.** Archives are out of milestone 1 scope. Merging HLS or DASH playlists only makes sense for unprotected streams. We do no DRM handling.
- **Hash deduplication with SQLite.** Needs hashing of existing files and a new dependency, so it needs your approval.
- **Richer templates.** Add `{Category}`, `{Year}`, and `{Month}` tokens. The spec has `{host}` but not these.
- **Zero-copy I/O.** IOCP is the one that matters for Windows; io_uring is Linux only. Deferred in the spec. Measure first.
- **HTTP/3.** Deferred in the spec. reqwest's HTTP/3 support is experimental, so check it before planning. It only helps if the server supports HTTP/3.
- **Native UI.** Slint or Tauri later. A webview is lighter than Electron, but check the memory number before claiming "sub-20 MB". Treat that figure as a target to measure.
- **Engine as a daemon.** Runs as a background service after the CLI, with named pipes or a local API.
