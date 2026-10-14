# IDM behavior checklist (black-box)

Behaviors of IDM 6.43 observed on the wire against the local test server: three runs with default settings, one where the server ignores Range, and one where the server caps connections at 4. Only black-box observations are recorded here. Five runs is a small sample; items under "Not yet tested" are open.

The test server is described in `docs/superpowers/specs/2026-10-13-rustdm-milestone-1-design.md`.

## Observed (five runs)
- The first request for a file is a plain GET with no Range header, not a small probe. IDM cut it off after about 240 KB, once it had started the other connections. This differs from our spec's `bytes=0-0` probe, so the spec needs a decision.
- Ramp-up: 1 connection, then 4 almost at once, then 8 about 1.4 to 1.9 s later. The 1 to 2 step was never seen. One extra connection opened about 0.9 s after the first group, with no clear cause.
- Split points move by a few KB between runs (about 233 KB and 237 KB on a 16 MB file). Do not hard-code them.
- Ranges can overlap at the tail: two requests both ran to end of file.
- User-Agent was the built-in default on every request. No browser string was involved.
- Headers: Host, User-Agent, Accept, `Accept-Encoding: identity`, Accept-Language, Accept-Charset, Referer, Range, and Cookie when the host had browser cookies. No Connection header.
- Server ignores Range: IDM downloads the whole file on one connection. It also sends two open-ended Range requests and closes them after 16 KB.
- Server caps at 4 connections: extra requests get 503 in tight bursts. IDM keeps retrying every few seconds and the download still completes.
- File size matched in every run. SHA-256 matched in the first two.

## Not yet tested
- Resume after killing IDM mid-download.
- Server returns wrong bytes in the overlap (expected: error and fallback).
- Server answers a Range request with an error status.
- Slow or stalled connection.
- Server drops the connection mid-body.
- File changes between sessions (ETag change).
