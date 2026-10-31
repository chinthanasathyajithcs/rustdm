# rustdm Milestone 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `rdm`, a Windows 11 command-line engine that downloads one HTTP(S) file over several connections, survives pause and crashes, and writes the result safely.

**Architecture:** One Cargo package with a `lib` (the engine) and a thin `bin` (the `rdm` CLI). Pure modules (`ranges`, `template`) do no I/O. The engine runs probe, then plan, then workers, then writer, then state. Tests run against an in-repo raw-TCP test server behind the `testing` feature.

**Tech Stack:** Rust stable; tokio; reqwest (rustls); serde and serde_json; clap; url; thiserror; percent-encoding; tracing and tracing-subscriber; fastrand; tempfile (dev only).

**Spec:** `docs/superpowers/specs/2026-10-13-rustdm-milestone-1-design.md`. Sections 4 to 7 define behavior, section 8 holds the decisions, and section 10 is the IDM wire reference.

**Branch:** `feat/milestone-1-engine`, created from `main` after the spec branch `docs/milestone-1-spec` is merged.

## Global Constraints

- Rust stable; Windows 11 target (`x86_64-pc-windows-msvc`); CI on `windows-latest` and `ubuntu-latest` (spec 2, 6).
- Ranges are half-open `[start, end)` in `u64` (spec 4.2).
- `MIN_SEGMENT` = 1 MiB; work-stealing threshold `2 * MIN_SEGMENT` (spec 4.4).
- Default connections 8, at most 16 (spec 4.2).
- Probe is `GET` with `Range: bytes=0-0` (spec 4.1). Every range request sends `Accept-Encoding: identity` and `If-Range: <validator>` once a validator is known (spec 8).
- Part file is `<final>.part`; state file is `<final>.part.rdm`. Rename to the final name only after every range is complete and the state file is removed (spec 4.3, 4.5).
- Sync data before recording a range as done. Save state every 4 MiB or 2 s, and on pause (spec 4.5).
- Never overwrite. On a name collision, append ` (1)`, ` (2)` (spec 4.7).
- Default template `{downloads}\{name}`. Tokens: `{downloads}`, `{host}`, `{name}`, `{stem}`, `{ext}`, `{date}` (spec 4.7).
- Logs go through `tracing`. No `println!` in library code (spec 5).
- 429 and 503 honor `Retry-After`. 403, 404 and 410 on a previously working link become "link expired or removed", and the download pauses with state saved (spec 4.6).
- Coverage at least 80% on the library crate (spec 6).

## Dependencies and Approval Gates

- **Gate A, before Task 5:** add `tokio`. Core runtime; the author must approve it at this point.
- **Gate B, before Task 6:** add `reqwest` with the `rustls-tls` feature. It does core download work, so the author must approve it at this point.
- **Approved by this plan:** `serde`, `serde_json`, `clap`, `url`, `thiserror`, `percent-encoding`, `tracing`, `tracing-subscriber`, `fastrand`. Dev only: `tempfile`.

## Review Focus

1. A server ignores `Range` (plain `200`) on a file of any size: one connection, a complete byte-identical file, no panic. Owned by Tasks 6 and 8.
2. A hostile file name from `Content-Disposition` (`../../evil.exe`, `..\x`, `C:\x`): the file stays inside the download folder. Owned by Tasks 3 and 4.
3. A reserved Windows device name (`CON`, `nul.txt`, `COM1.log`): gets a safe name and never touches a device. Owned by Task 3.
4. A corrupt or truncated state file on resume: a clear error, never a silent restart from zero. Owned by Task 12.
5. The server file changed between sessions (ETag changed): resume refuses with a clear message and leaves the part file untouched. Owned by Tasks 9 and 12.

## Decisions This Plan Makes (confirm at review)

1. One Cargo package with `src/lib.rs` (engine) and `src/main.rs` (the `rdm` binary).
2. For the single-connection fallback after a probe `200`, drop the probe body and issue a plain `GET`. This costs one extra request and keeps the code simple.
3. Retries: `MAX_SEGMENT_ATTEMPTS = 5`. Backoff is `500 ms * 2^attempt`, capped at 30 s, with ±25% jitter. `Retry-After` takes integer seconds only; HTTP-date form is ignored.
4. User-Agent is `rustdm/<version>`.
5. Default output folder is `%USERPROFILE%\Downloads`, falling back to `$HOME/Downloads` on WSL.
6. The test server lives in `src/testing/` behind the `testing` feature, so tests and the demo share it.
7. `{downloads}` is trusted config and is inserted as written. The other tokens are sanitized (Task 4).
8. `{date}` is the UTC date, computed from `SystemTime`. No date crate is added.
9. Connections start all at once. The ramp (1, 4, 8) is a later option.

## File Structure

```
Cargo.toml                     package rustdm; lib + bin named rdm; feature testing
src/lib.rs                     module list
src/main.rs                    clap CLI and progress printing (thin)
src/error.rs                   Error enum
src/ranges.rs                  ByteRange, RangeSet, initial_segments, split_upper_half   (pure)
src/template.rs                sanitize_component, expand, split_stem_ext, pick_name,
                               is_inside, collision_name                                (pure)
src/probe.rs                   probe, Probe, Validator, parse_content_range_total
src/writer.rs                  PartFile
src/retry.rs                   constants, backoff_delay, retry_after_secs
src/worker.rs                  fetch_range
src/scheduler.rs               Lane, pick_victim, steal
src/state.rs                   State, save_atomic, load, should_checkpoint
src/engine.rs                  Progress, DownloadRequest, Outcome, download, resume
src/testing/mod.rs             TestServer, Mode, pattern_bytes (feature testing)
examples/serve.rs              demo server (Task 14)
tests/download.rs              integration: downloads, pause, expiry
tests/resume.rs                integration: resume
tests/crash.rs                 crash test running the rdm binary
.github/workflows/ci.yml
README.md
```

---

### Task 1: Scaffold and CI

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `tests/smoke.rs`, `.github/workflows/ci.yml`

**Interfaces:**
- Produces: `pub const VERSION: &str` in `src/lib.rs` (equal to `env!("CARGO_PKG_VERSION")`).

- [x] **Step 1: Write the failing test**

```rust
// tests/smoke.rs
#[test]
fn library_exposes_version() {
    assert_eq!(rustdm::VERSION, env!("CARGO_PKG_VERSION"));
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --test smoke`
Expected: FAIL, `rustdm` has no `VERSION`.

- [x] **Step 3: Create the scaffold**

`src/lib.rs` declares `#![deny(clippy::print_stdout, clippy::print_stderr)]` and `pub const VERSION`. `src/main.rs` is a placeholder `fn main() {}`. `Cargo.toml` sets `[[bin]] name = "rdm"` and `[features] testing = []`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test`
Expected: `test result: ok` for `library_exposes_version`.

- [x] **Step 5: Add CI**

`ci.yml` runs on push and pull request, with a matrix of `windows-latest` and `ubuntu-latest`. Jobs: `cargo fmt --check`, `cargo clippy --all-targets --features testing -- -D warnings`, `cargo test --features testing`.

- [x] **Step 6: Commit**

```bash
git add Cargo.toml src/ tests/smoke.rs .github/
git commit -m "chore: scaffold crate and Windows and Linux CI"
```

---

### Task 2: Byte ranges and segments

**Files:**
- Create: `src/ranges.rs`
- Modify: `src/lib.rs` (add `pub mod ranges;`)
- Test: inline `#[cfg(test)]` module in `src/ranges.rs`

**Interfaces:**
- Produces:
  - `pub const MIN_SEGMENT: u64 = 1024 * 1024;`
  - `pub struct ByteRange { pub start: u64, pub end: u64 }` (half-open), with `pub fn len(&self) -> u64` and `pub fn is_empty(&self) -> bool`. Derives `Clone, Copy, Debug, PartialEq, Eq`.
  - `pub struct RangeSet`, with `new()`, `insert(&mut self, r: ByteRange)`, `ranges(&self) -> &[ByteRange]`, `covered(&self) -> u64`, `complement(&self, total: u64) -> Vec<ByteRange>`.
  - `pub fn initial_segments(total: u64, connections: u32) -> Vec<ByteRange>`
  - `pub fn split_upper_half(r: ByteRange) -> Option<(ByteRange, ByteRange)>`, returning `(keep, take)`.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn insert_merges_adjacent_and_overlapping() {
    let mut s = RangeSet::new();
    s.insert(r(0, 5)); s.insert(r(5, 10)); s.insert(r(8, 12));
    assert_eq!(s.ranges(), &[r(0, 12)]);
}
#[test]
fn complement_lists_missing_gaps() {
    let mut s = RangeSet::new();
    s.insert(r(0, 5)); s.insert(r(10, 15));
    assert_eq!(s.complement(20), vec![r(5, 10), r(15, 20)]);
}
#[test]
fn initial_segments_capped_by_connections_and_size() {
    let segs = initial_segments(8 * MIN_SEGMENT, 4);
    assert_eq!(segs.len(), 4);
    assert_eq!(segs.last().unwrap().end, 8 * MIN_SEGMENT); // last absorbs remainder
}
#[test]
fn small_file_gets_one_segment() { assert_eq!(initial_segments(100, 8), vec![r(0, 100)]); }
#[test]
fn zero_length_has_no_segments() { assert!(initial_segments(0, 8).is_empty()); }
#[test]
fn split_needs_two_min_segments() {
    assert_eq!(split_upper_half(r(0, 2 * MIN_SEGMENT - 1)), None);
    assert_eq!(split_upper_half(r(0, 2 * MIN_SEGMENT)),
               Some((r(0, MIN_SEGMENT), r(MIN_SEGMENT, 2 * MIN_SEGMENT))));
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test ranges`
Expected: FAIL, items not defined.

- [x] **Step 3: Implement**

`RangeSet` keeps a sorted `Vec<ByteRange>` with no overlaps and merges on insert. `initial_segments` uses `n = min(connections, total / MIN_SEGMENT)`, at least 1 when `total > 0`. Each segment has `total / n` bytes, and the last one ends at `total`.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test ranges`
Expected: all six tests PASS.

- [x] **Step 5: Commit**

```bash
git add src/ranges.rs src/lib.rs
git commit -m "feat: add byte range set and segment splitting"
```

---

### Task 3: Filename sanitizing

**Files:**
- Create: `src/template.rs`
- Modify: `src/lib.rs` (add `pub mod template;`)
- Test: inline module in `src/template.rs`

**Interfaces:**
- Produces: `pub fn sanitize_component(raw: &str) -> String`

Rules, applied in order (spec 4.7):
1. Strip control characters (U+0000 to U+001F, U+007F).
2. Replace each of `< > : " / \ | ? *` with `_`.
3. If the result is `.` or `..`, return an empty string.
4. Trim leading and trailing dots and spaces.
5. If the part before the first dot is a reserved device name (CON, PRN, AUX, NUL, COM1 to COM9, LPT1 to LPT9, case-insensitive), prefix `_`.
6. Cap at 255 characters.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn sanitize_table() {
    let cases = [
        ("..", ""),
        ("../../evil.exe", "_.._evil.exe"),   // review focus 2
        ("a<b>.txt", "a_b_.txt"),
        ("  x. ", "x"),
        ("a\u{7}b", "ab"),
        ("CON", "_CON"),                     // review focus 3
        ("nul.txt", "_nul.txt"),             // review focus 3
        ("COM1.log", "_COM1.log"),           // review focus 3
        ("console.txt", "console.txt"),      // not reserved: stem is "console"
    ];
    for (raw, want) in cases { assert_eq!(sanitize_component(raw), want, "input {raw:?}"); }
}
#[test]
fn sanitize_caps_length_at_255_chars() {
    assert_eq!(sanitize_component(&"a".repeat(300)).chars().count(), 255);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test template`
Expected: FAIL, `sanitize_component` not defined.

- [x] **Step 3: Implement** `sanitize_component` in the order listed above.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test template`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/template.rs src/lib.rs
git commit -m "feat: sanitize download file name components"
```

---

### Task 4: Path expansion and naming

**Files:**
- Modify: `src/template.rs`, `Cargo.toml` (add `url`, `percent-encoding`)
- Test: inline module in `src/template.rs`

**Interfaces:**
- Consumes: `sanitize_component` (Task 3).
- Produces:
  - `pub struct Tokens { pub downloads: String, pub host: String, pub name: String, pub date: String }`
  - `pub fn split_stem_ext(name: &str) -> (String, String)`. The extension has no dot. A leading dot is not an extension, so `.bashrc` gives `(".bashrc", "")`.
  - `pub fn expand(template: &str, tokens: &Tokens) -> String`. Sanitizes `host`, `name`, `stem`, `ext` and `date`. Inserts `downloads` as written.
  - `pub fn pick_name(content_disposition: Option<&str>, url: &url::Url) -> String`. Priority: `filename*` (RFC 5987 with percent-decoding), then `filename`, then the last URL path segment (percent-decoded), then `download`.
  - `pub fn is_inside(base: &str, candidate: &str) -> bool`. True only if `candidate` starts with `base` followed by `\` or `/`, and has no `..` segment.
  - `pub fn collision_name(file_name: &str, n: u32) -> String`, for example `file (1).iso`.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn hostile_name_stays_inside_base() {                // review focus 2
    let t = tokens("C:\\Users\\me\\Downloads", "../../evil.exe");
    let out = expand("{downloads}\\{name}", &t);
    assert_eq!(out, "C:\\Users\\me\\Downloads\\_.._evil.exe");
    assert!(is_inside("C:\\Users\\me\\Downloads", &out));
}
#[test]
fn is_inside_rejects_escapes_and_prefix_tricks() {
    assert!(!is_inside("C:\\Users\\me\\Downloads", "C:\\Users\\me\\Downloads\\..\\..\\Windows\\x"));
    assert!(!is_inside("C:\\Users\\me\\Downloads", "C:\\Users\\me\\Downloads2\\x"));
}
#[test]
fn split_stem_ext_handles_dotfiles_and_multi_dot() {
    assert_eq!(split_stem_ext("file.tar.gz"), ("file.tar".into(), "gz".into()));
    assert_eq!(split_stem_ext(".bashrc"), (".bashrc".into(), "".into()));
}
#[test]
fn pick_name_priority() {
    let u = url::Url::parse("https://x.test/dir/file%20one.iso").unwrap();
    assert_eq!(pick_name(Some("attachment; filename*=UTF-8''na%C3%AFve.zip"), &u), "naïve.zip");
    assert_eq!(pick_name(Some("attachment; filename=\"a.txt\""), &u), "a.txt");
    assert_eq!(pick_name(None, &u), "file one.iso");
    let root = url::Url::parse("https://x.test/").unwrap();
    assert_eq!(pick_name(None, &root), "download");
}
#[test]
fn collision_names_use_numbered_suffix() {
    assert_eq!(collision_name("file.iso", 1), "file (1).iso");
    assert_eq!(collision_name("README", 2), "README (2)");
}
```

`tokens(downloads, name)` is a small test helper that fills `host`, `date` and `stem`/`ext` with fixed values.

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test template`
Expected: FAIL, functions not defined.

- [x] **Step 3: Implement** the functions above. Add the `url` and `percent-encoding` dependencies (approved in this plan) to `Cargo.toml`. `pick_name` parses `filename*` as `charset'lang'value` and percent-decodes the value.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test template`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/template.rs Cargo.toml
git commit -m "feat: expand path templates and pick file names"
```

---

### Task 5: Test server (Gate A: tokio approval required)

**Files:**
- Create: `src/testing/mod.rs`, `tests/test_server.rs`
- Modify: `src/lib.rs` (`#[cfg(feature = "testing")] pub mod testing;`), `Cargo.toml` (add `tokio`)

**Interfaces:**
- Produces:
  - `pub fn pattern_bytes(len: u64) -> Vec<u8>`: deterministic, with no short period.
  - `pub enum Mode { Normal, IgnoreRange, Status(u16) }`. More modes are added by the tasks that test them.
  - `pub struct RequestLog { pub method: String, pub path: String, pub range: Option<String>, pub if_range: Option<String>, pub accept_encoding: Option<String>, pub user_agent: Option<String> }`
  - `pub struct TestServer`, with `pub async fn start(body: Vec<u8>, mode: Mode) -> TestServer`, `pub fn url(&self) -> url::Url`, `pub fn set_etag(&self, etag: &str)`, `pub fn requests(&self) -> Vec<RequestLog>`.

Server behavior: serves over HTTP/1.1 with `Connection: close`, `Content-Length`, `ETag` (the current value, initially `"v1"`) and `Last-Modified`. HTTP ranges are inclusive, so `bytes=10-19` means 10 bytes starting at 10. In `Normal` mode, a `Range` request returns `206` with `Content-Range`. If `If-Range` is present and does not match the current ETag, the server returns `200` with the full body. `IgnoreRange` always returns `200` with the full body. `Status(c)` returns `c` with an empty body.

- [x] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn ranged_get_returns_slice_and_stale_if_range_returns_full() {
    let body = pattern_bytes(1000);
    let srv = TestServer::start(body.clone(), Mode::Normal).await;
    let slice = raw_get(&srv, "bytes=10-19", None).await;          // helper: raw TCP GET, returns (status, headers, body)
    assert_eq!(slice.status, 206);
    assert_eq!(slice.body, body[10..20]);
    let full = raw_get(&srv, "bytes=10-19", Some("\"stale\"")).await;
    assert_eq!(full.status, 200);
    assert_eq!(full.body.len(), 1000);
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --features testing --test test_server`
Expected: FAIL, `testing` module not defined.

- [x] **Step 3: Implement** the server on a `tokio::net::TcpListener`. Read the request head until `\r\n\r\n`, parse the request line and the headers used in `RequestLog`, record the log, then respond.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test --features testing --test test_server`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/testing/ src/lib.rs tests/test_server.rs Cargo.toml Cargo.lock
git commit -m "test: add raw TCP test server with range and If-Range handling"
```

---

### Task 6: Probe (Gate B: reqwest approval required)

**Files:**
- Create: `src/error.rs`, `src/probe.rs`
- Modify: `src/testing/mod.rs` (add `Mode::Redirect`), `src/lib.rs`, `Cargo.toml` (add `reqwest`, `thiserror`, `serde`, `serde_json`, `clap`, `tracing`, `tracing-subscriber`, `fastrand` now so later tasks can rely on them)
- Test: `tests/probe.rs`

**Interfaces:**
- Produces:
  - `pub enum Error` (thiserror): `LinkExpired { status: u16 }`, `FileChanged`, `Protocol(String)`, `State(String)`, `Http(reqwest::Error)`, `Io(std::io::Error)`. `From` is implemented for `reqwest::Error` and `io::Error`.
  - `pub struct Validator { pub etag: Option<String>, pub last_modified: Option<String> }`. Derives `Clone, Debug, Default, PartialEq`.
  - `pub struct Probe { pub final_url: url::Url, pub total: Option<u64>, pub ranges_supported: bool, pub validator: Validator, pub content_disposition: Option<String> }`
  - `pub async fn probe(client: &reqwest::Client, url: &url::Url) -> Result<Probe, Error>`
  - `pub fn parse_content_range_total(value: &str) -> Option<u64>`: reads the total from `bytes 0-0/TOTAL`, and returns `None` for `*`.

Behavior by status: `206` means ranges are supported and the total comes from `Content-Range`. `200` means ranges are not supported and the total comes from `Content-Length`. `416` means no total and no ranges. `403`, `404` and `410` return `Error::LinkExpired`. Other errors return `Error::Protocol`. Only a strong ETag (one that does not start with `W/`) is recorded.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn probe_reports_size_and_range_support_on_206() { /* Mode::Normal, 1000 bytes -> total 1000, ranges true */ }
#[tokio::test] async fn probe_falls_back_when_server_ignores_range() { /* Mode::IgnoreRange -> ranges false, total 1000 */ } // review focus 1
#[tokio::test] async fn probe_records_strong_etag_and_last_modified() { /* validator.etag == "\"v1\"" */ }
#[tokio::test] async fn probe_follows_redirect_and_records_final_url() { /* Mode::Redirect: final_url path is /moved/... */ }
#[tokio::test] async fn probe_sends_bytes_0_0_and_identity_encoding() { /* log: range == "bytes=0-0", accept_encoding == "identity" */ }
#[tokio::test] async fn probe_reports_link_expired_on_403() { /* Mode::Status(403) -> Err(LinkExpired { status: 403 }) */ }
#[test]
fn content_range_total_parses_only_numeric_totals() {
    assert_eq!(parse_content_range_total("bytes 0-0/1000"), Some(1000));
    assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --features testing --test probe`
Expected: FAIL, `probe` not defined.

- [x] **Step 3: Implement** `probe`. Build the request with `Range: bytes=0-0` and `Accept-Encoding: identity`, and do not read the body. `Redirect` mode returns `302` to `/moved/<path>` on the first request and serves normally under `/moved/`.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --features testing --test probe`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/error.rs src/probe.rs src/testing/ src/lib.rs tests/probe.rs Cargo.toml Cargo.lock
git commit -m "feat: probe file size, range support and validators"
```

---

### Task 7: Part file writer

**Files:**
- Create: `src/writer.rs`
- Modify: `src/lib.rs`
- Test: `tests/writer.rs`; `tempfile` as a dev dependency

**Interfaces:**
- Produces:
  - `pub struct PartFile` (holds a `std::fs::File` and the part path)
  - `pub fn create(part_path: &Path, total: Option<u64>) -> io::Result<PartFile>`. Creates the file with `create_new`, and calls `set_len(total)` when the total is known.
  - `pub fn open(part_path: &Path) -> io::Result<PartFile>`, for workers, each with its own handle.
  - `pub fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()>`, looping until all bytes are written.
  - `pub fn sync(&self) -> io::Result<()>`, using `sync_data`.
  - `pub fn finish(self, final_path: &Path) -> io::Result<()>`. Syncs, then renames. Returns `AlreadyExists` if `final_path` exists.

Platform: `std::os::windows::fs::FileExt::seek_write` on Windows, and `std::os::unix::fs::FileExt::write_at` elsewhere, behind `cfg`.

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn create_preallocates_to_total() { /* metadata().len() == total */ }
#[test] fn writes_out_of_order_read_back_correctly() { /* write [500..1000) then [0..500), read full file */ }
#[test] fn second_handle_sees_writes() { /* open() after create() and write, read via the other handle */ }
#[test] fn finish_renames_and_removes_part() { /* part gone, final has bytes */ }
#[test] fn finish_refuses_to_overwrite_existing_target() { /* Err(AlreadyExists), existing file unchanged */ }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --test writer`
Expected: FAIL.

- [x] **Step 3: Implement** the methods above. `write_at` loops because `seek_write` and `write_at` may write fewer bytes than requested.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --test writer`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/writer.rs src/lib.rs tests/writer.rs Cargo.toml Cargo.lock
git commit -m "feat: add preallocated part file with offset writes"
```

---

### Task 8: Single-connection download

**Files:**
- Create: `src/engine.rs`
- Modify: `src/lib.rs`, `src/testing/mod.rs` (add `Mode::Chunked`: no `Content-Length`, sent with `Transfer-Encoding: chunked`)
- Test: `tests/download.rs`

**Interfaces:**
- Consumes: `probe` (Task 6), `PartFile` (Task 7), `pick_name`, `expand`, `split_stem_ext`, `collision_name` (Tasks 3 and 4).
- Produces:
  - `pub struct Progress { pub downloaded: AtomicU64, pub completed: Mutex<RangeSet> }`. Workers update both. `completed` is used from Task 12.
  - `pub struct DownloadRequest { pub url: url::Url, pub template: String, pub downloads_dir: PathBuf, pub connections: u32, pub progress: Arc<Progress> }`
  - `pub enum Outcome { Completed { path: PathBuf, steals: u32 } }`. Task 13 adds `Paused`, and Task 11 fills in `steals`.
  - `pub fn build_client() -> reqwest::Client`, with User-Agent `rustdm/<version>`.
  - `pub async fn download(client: &reqwest::Client, req: DownloadRequest) -> Result<Outcome, Error>`
  - `fn resolve_output(req: &DownloadRequest, probe: &Probe) -> PathBuf`. Expands the template and applies `collision_name` until the path is free.

This task uses one plain `GET` with no `Range` header, whatever the probe said. Chunks are written sequentially through `Arc<PartFile>` with `spawn_blocking`. On completion, if the total is known and the byte count differs, return `Protocol`. Otherwise `finish` the part file.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn downloads_whole_file_when_range_ignored() { /* IgnoreRange, 1 MiB + 17 bytes, output bytes == source */ }
#[tokio::test] async fn downloads_unknown_length_stream() { /* Chunked, bytes == source */ }
#[tokio::test] async fn progress_reaches_total() { /* progress.downloaded == total */ }
#[tokio::test] async fn never_overwrites_existing_file() { /* existing "x.bin" -> output "x (1).bin", old file unchanged */ }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --features testing --test download`
Expected: FAIL.

- [x] **Step 3: Implement** `build_client`, `resolve_output`, and `download` as described. Keep the single-connection path in its own function, so Task 9 can add the multi-connection path beside it.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --features testing --test download`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/engine.rs src/lib.rs src/testing/ tests/download.rs
git commit -m "feat: download a whole file over one connection"
```

---

### Task 9: Parallel ranges with If-Range checks

**Files:**
- Create: `src/worker.rs`
- Modify: `src/engine.rs` (multi-connection path), `src/testing/mod.rs` (`set_etag` is already present; no new mode)
- Test: `tests/download.rs` (add), `src/worker.rs` (inline test for `fetch_range`)

**Interfaces:**
- Consumes: `initial_segments` (Task 2), `PartFile` (Task 7), `Progress` (Task 8).
- Produces:
  - `pub async fn fetch_range(client: &reqwest::Client, url: &url::Url, validator: &Validator, range: ByteRange, part: Arc<PartFile>, progress: Arc<Progress>) -> Result<(), Error>`. Sends `Range: bytes=start-(end-1)`, `Accept-Encoding: identity`, and `If-Range`, using the ETag when present and the Last-Modified otherwise. Expects `206`. A `200` answer to a request that carried `If-Range` returns `FileChanged`. Each chunk is written at its offset and recorded in `progress`.
  - `engine::download` takes the multi-connection path when `ranges_supported`, the total is known and greater than zero, and `connections > 1`. It clamps `connections` to `1..=16` and uses `DEFAULT_CONNECTIONS = 8` as the default. It starts one task per segment with `JoinSet` and aborts the others on the first error.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn multi_connection_download_matches_source() { /* 8 MiB, connections in [1, 2, 4, 8, 16], bytes == source */ }
#[tokio::test] async fn small_file_uses_one_range_request() { /* 100 KiB, 8 connections: log has one Range request */ }
#[tokio::test] async fn every_range_request_sends_if_range_and_identity() { /* all Range logs: if_range Some, accept_encoding "identity" */ }
#[tokio::test] async fn fetch_range_reports_file_changed_when_validator_stale() { /* set_etag("v2"), validator "\"v1\"": Err(FileChanged) */ } // review focus 5
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --features testing`
Expected: FAIL on the new tests.

- [x] **Step 3: Implement** `fetch_range` and the multi-connection path. The worker counts a range as done only after its bytes are written.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --features testing`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/worker.rs src/engine.rs tests/download.rs
git commit -m "feat: download in parallel ranges with If-Range checks"
```

---

### Task 10: Failure policy

**Files:**
- Create: `src/retry.rs`
- Modify: `src/worker.rs` (retry loop), `src/testing/mod.rs` (add modes below)
- Test: `src/retry.rs` (inline), `tests/download.rs` (add)

**Interfaces:**
- Produces:
  - `pub const MAX_SEGMENT_ATTEMPTS: u32 = 5;`, `pub const BACKOFF_BASE: Duration = 500 ms;`, `pub const BACKOFF_CAP: Duration = 30 s;`
  - `pub fn backoff_delay(attempt: u32, jitter: f64) -> Duration`. Multiplier is `0.75 + 0.5 * jitter`, with `jitter` in `[0, 1)`.
  - `pub fn retry_after_secs(value: &str) -> Option<Duration>`. Integer seconds only.
- Worker: on a dropped connection, a short body, a wrong `Content-Range`, or a `429` or `503`, wait (`Retry-After` if present, otherwise backoff) and retry from the last written byte. After `MAX_SEGMENT_ATTEMPTS`, return `Protocol`. A `403`, `404` or `410` returns `LinkExpired` at once, with no retry.
- New test modes: `FailThenOk { status: u16, times: u32, retry_after: Option<u32> }`, `ShortBody`, `WrongContentRange`, `DropMidBodyOnce`.

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn backoff_grows_then_caps() {
    assert_eq!(backoff_delay(0, 0.5), Duration::from_millis(500));
    assert_eq!(backoff_delay(20, 0.5), BACKOFF_CAP);
}
#[test] fn retry_after_takes_integer_seconds_only() {
    assert_eq!(retry_after_secs("2"), Some(Duration::from_secs(2)));
    assert_eq!(retry_after_secs("soon"), None);
}
#[tokio::test] async fn honors_retry_after_on_503() { /* FailThenOk{503, 2, Some(0)}: completes */ }
#[tokio::test] async fn link_expired_fails_without_retry() { /* fetch_range against Status(403): LinkExpired, one request */ }
#[tokio::test] async fn short_body_fails_at_attempt_cap() { /* ShortBody: Protocol, MAX_SEGMENT_ATTEMPTS range requests */ }
#[tokio::test] async fn wrong_content_range_is_protocol_error() { /* WrongContentRange: Protocol after the cap */ }
#[tokio::test] async fn resumes_from_last_written_byte_after_drop() { /* DropMidBodyOnce: second Range start > segment start, bytes == source */ } // review focus 4 prep
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --features testing`
Expected: FAIL.

- [x] **Step 3: Implement** the retry loop in `worker.rs`, with the delay chosen by `backoff_delay(attempt, fastrand::f64())`.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --features testing`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/retry.rs src/worker.rs src/testing/ tests/download.rs
git commit -m "feat: retry segments with backoff and honor Retry-After"
```

---

### Task 11: Work stealing

**Files:**
- Create: `src/scheduler.rs`
- Modify: `src/worker.rs` (chunks clamped to the shared `end`), `src/engine.rs` (multi-connection path uses lanes), `src/testing/mod.rs` (add `SlowLowHalf { bytes_per_sec: u64 }`: throttles only requests whose start is below total/2)
- Test: `src/scheduler.rs` (inline), `tests/download.rs` (add)

**Interfaces:**
- Consumes: `split_upper_half` and `MIN_SEGMENT` (Task 2).
- Produces:
  - `pub struct Lane { pub cursor: Arc<AtomicU64>, pub end: Arc<AtomicU64>, pub started: Instant, pub bytes_done: Arc<AtomicU64> }`
  - `pub fn pick_victim(lanes: &[Lane], now: Instant) -> Option<usize>`. Chooses the splittable lane with the longest estimated time left, where ETA is `remaining / speed` and speed is `bytes_done / elapsed`.
  - `pub fn steal(lane: &Lane) -> Option<ByteRange>`. Reads `end` and `cursor`. If `end - cursor >= 2 * MIN_SEGMENT`, it sets `end` to the midpoint and returns `[midpoint, old_end)`. A compare-and-swap loop on `end` makes concurrent thieves safe.
  - `Outcome::Completed.steals` counts the successful steals.

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn pick_victim_chooses_longest_eta() { /* two lanes, equal speed, larger remaining wins */ }
#[test] fn steal_halves_remaining_and_lowers_victim_end() { /* cursor 0, end 4 MiB: victim end 2 MiB, returns [2 MiB, 4 MiB) */ }
#[test] fn steal_refuses_below_threshold() { /* end = 2*MIN - 1 -> None */ }
#[test] fn repeated_steals_return_disjoint_ranges() { /* two steals on one lane never overlap */ }
#[tokio::test] async fn stealing_with_slow_low_half_is_byte_identical() { /* SlowLowHalf, 8 MiB, 2 connections: bytes == source, steals >= 1 */ } // spec 6
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --features testing`
Expected: FAIL.

- [x] **Step 3: Implement** `scheduler.rs`. When a worker finishes, it calls `pick_victim` and then `steal`, and registers the new lane. The worker's chunk loop reads `end` before each write and clamps the chunk to it.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --features testing`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/scheduler.rs src/worker.rs src/engine.rs src/testing/ tests/download.rs
git commit -m "feat: work stealing for idle connections"
```

---

### Task 12: State and resume

**Files:**
- Create: `src/state.rs`
- Modify: `src/engine.rs` (checkpoints, `resume`), `src/error.rs` (no change expected)
- Test: `src/state.rs` (inline), `tests/resume.rs`

**Interfaces:**
- Produces:
  - `pub const STATE_VERSION: u32 = 1;`, `pub const CHECKPOINT_BYTES: u64 = 4 MiB;`, `pub const CHECKPOINT_INTERVAL: Duration = 2 s;`
  - `#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] pub struct State { pub version: u32, pub url: String, pub final_url: String, pub total: u64, pub validator: Validator, pub completed: Vec<ByteRange>, pub output: PathBuf }`. `Validator` and `ByteRange` derive `Serialize` and `Deserialize`.
  - `pub fn state_path(output: &Path) -> PathBuf`, which appends `.part.rdm` to the full file name. `pub fn part_path(output: &Path) -> PathBuf`, which appends `.part`.
  - `pub fn save_atomic(path: &Path, state: &State) -> Result<(), Error>`. Writes `<path>.tmp`, calls `sync_all`, then renames over `path`.
  - `pub fn load(path: &Path) -> Result<State, Error>`. Unparseable JSON or a version other than `STATE_VERSION` returns `Error::State` with a clear message.
  - `pub fn should_checkpoint(bytes_since_last: u64, since_last: Duration) -> bool`.
  - `pub async fn resume(client: &reqwest::Client, state_path: &Path, url_override: Option<url::Url>) -> Result<Outcome, Error>`. Loads the state, probes again, and returns `FileChanged` if the total or the validator differs. Downloads the complement of `completed`, then finishes and deletes the state file.
- Ordering rule (spec 4.5): snapshot `completed`, then `sync` the part file, then `save_atomic`. Only ranges in the snapshot are ever recorded as done.

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn state_round_trips_through_json() { /* save_atomic then load == original */ }
#[test] fn save_atomic_leaves_no_temp_file() { /* after save, only the state path exists */ }
#[test] fn corrupt_state_file_is_reported_not_ignored() { /* "{not json" -> Err(State(_)) */ } // review focus 4
#[test] fn truncated_state_file_is_reported() { /* first 40 bytes of valid JSON -> Err(State(_)) */ } // review focus 4
#[test] fn unknown_state_version_is_rejected() { /* version 99 -> Err(State(_)) */ }
#[test] fn checkpoint_fires_on_bytes_or_time() { /* table: (4 MiB, 0) true; (0, 2 s) true; (1, 1 s) false */ }
#[tokio::test] async fn resume_completes_partial_download() { /* first half in part + state; resume; bytes == source; range requests start >= half */ }
#[tokio::test] async fn resume_refuses_when_file_changed() { /* set_etag("v2") before resume: Err(FileChanged), part bytes unchanged */ } // review focus 5
#[tokio::test] async fn resume_errors_when_part_file_missing() { /* state exists, part deleted: Err(Io(_)) */ }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --features testing`
Expected: FAIL.

- [x] **Step 3: Implement** `state.rs`. In `engine.rs`, `download` writes the state file right after the part file is created, with `completed` empty. That way a kill at any moment leaves something to resume from. Checkpoints follow the ordering rule above.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --features testing`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/state.rs src/engine.rs tests/resume.rs
git commit -m "feat: save resumable state and resume downloads"
```

---

### Task 13: Pause, expired links, the rdm CLI, and the crash test

**Files:**
- Modify: `src/engine.rs` (cancel and expiry produce a pause), `src/main.rs` (clap CLI), `src/testing/mod.rs` (add `ExpireAfter(u32)`: 403 once the request count passes N)
- Create: `tests/crash.rs`
- Test: `tests/download.rs` (add)

**Interfaces:**
- Produces:
  - `pub enum PauseReason { Cancelled, LinkExpired { status: u16 } }`
  - `pub enum Outcome { Completed { path: PathBuf, steals: u32 }, Paused { reason: PauseReason, state: PathBuf } }`
  - `DownloadRequest` gains `pub cancel: tokio::sync::watch::Receiver<bool>`. Workers check it between chunks.
  - `DownloadRequest.output: PathBuf` is replaced by `template: String` (default `{downloads}\{name}`), which Task 8 already resolves through `resolve_output`.
  - CLI: `rdm download <URL> [--out <template>] [--connections N]` and `rdm resume <STATE> [--url <URL>]`. Ctrl+C sends `true` on the cancel channel.
- On cancel or `LinkExpired`: stop the workers, snapshot `completed`, `sync`, `save_atomic`, and return `Paused`.
- The crash test runs the binary through `env!("CARGO_BIN_EXE_rdm")`. Each run starts a server with `Mode::Throttle`, starts `download`, waits a random 300 to 1500 ms (`fastrand`), kills the process with `Child::kill`, runs `resume`, and compares bytes. Run count is `RDM_CRASH_RUNS`, default 5.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn cancel_pauses_and_state_resumes() { /* cancel after 300 ms: Paused{Cancelled}; state exists; resume == source */ }
#[tokio::test] async fn link_expired_pauses_with_state_saved() { /* ExpireAfter(3): Paused{LinkExpired{403}}, state exists */ } // review focus 4 context
#[test] fn crash_then_resume_is_byte_identical() { /* tests/crash.rs: RDM_CRASH_RUNS runs of kill-and-resume */ }
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test --features testing`
Expected: FAIL.

- [x] **Step 3: Implement** the cancel path and the expiry path. Write the state file after the part file is created, as in Task 12. Then add the clap CLI in `main.rs`. Call `tracing_subscriber` once in `main`, with `RUST_LOG` respected. Default output is `%USERPROFILE%\Downloads` (or `$HOME/Downloads` on WSL), with `{date}` filled from `SystemTime` in UTC.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test --features testing`
Expected: PASS. Also run `RDM_CRASH_RUNS=20 cargo test --features testing --test crash` once, locally, to check for flakes.

- [x] **Step 5: Commit**

```bash
git add src/engine.rs src/main.rs src/testing/ tests/download.rs tests/crash.rs
git commit -m "feat: pause on cancel and expired links, add rdm CLI and crash test"
```

---

### Task 14: Polish, coverage gate, README and demo

**Files:**
- Modify: `src/main.rs` (progress line every 500 ms: bytes, MB/s, active connections, printed to stderr), `.github/workflows/ci.yml` (coverage job)
- Create: `examples/serve.rs`, `README.md`

**Interfaces:**
- Consumes: `testing::TestServer` and `Mode::Throttle { bytes_per_sec: u64 }` (added here, serves a `Normal` file with throttling on every connection).

- [x] **Step 1: Write the failing check**

Run: `cargo llvm-cov --features testing --fail-under-lines 80`
Expected: FAIL if line coverage is below 80%. Write the missing tests for whatever it reports.

- [x] **Step 2: Run the check**

Run: `cargo llvm-cov --features testing --fail-under-lines 80`
Expected: PASS.

- [x] **Step 3: Implement** the progress line, the `serve` example (`cargo run --features testing --example serve`, which serves a 64 MiB file at `127.0.0.1:8080`), and the README. The README covers: what rdm is, build (`cargo build --release`), test (`cargo test --features testing`), the demo (serve, then `rdm download http://127.0.0.1:8080/demo.bin`), and the limits of this milestone (no GUI, no speed limit, no expired-link refresh).

- [x] **Step 4: Run the full suite**

Run: `cargo fmt --check && cargo clippy --all-targets --features testing -- -D warnings && cargo test --features testing`
Expected: all pass, on both CI operating systems.

- [x] **Step 5: Commit**

```bash
git add src/main.rs examples/serve.rs README.md .github/workflows/ci.yml src/testing/
git commit -m "docs: add README, demo server, and 80% coverage gate"
```

---

## Self-Review

- **Spec coverage:** 4.1 probe (Task 6). 4.2 ranges and segments (Tasks 2, 9). 4.3 writer (Task 7). 4.4 work stealing (Task 11). 4.5 state and crash safety (Tasks 12, 13). 4.6 pause, resume and failures (Tasks 10, 13). 4.7 templating (Tasks 3, 4). 5 CLI (Task 13, polished in Task 14). 6 testing and CI (Tasks 1, 5 to 14). 7 build order maps to Tasks 2 to 14, with Task 1 added for the scaffold.
- **Types:** `ByteRange`, `Validator`, `Probe`, `PartFile`, `Progress`, `DownloadRequest`, `Outcome` and `Lane` are each defined once and used with the same names in later tasks. `Outcome` gains `steals` in Task 11 and `Paused` in Task 13, both stated in its own task.
- **Review focus:** all five lines have tests in their owning tasks (Tasks 3, 4, 6, 9, 12).
