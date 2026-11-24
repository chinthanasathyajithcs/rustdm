//! The pieces downloader on its own: several pieces at once, written in
//! order, with retries, speed caps and stops (spec 1c, section 7).

use rustdm::engine::Progress;
use rustdm::error::Error;
use rustdm::limiter::RateLimiter;
use rustdm::pieces::{download, state_path, Piece, PieceCounter, PiecesRequest, Slice};
use rustdm::testing::{Route, TestServer};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

fn body(i: usize, len: usize) -> Vec<u8> {
    (0..len).map(|j| (i * 7 + j) as u8).collect()
}

const PIECE_BYTES: usize = 2_000;

async fn server_with(n: usize, route: impl Fn(usize) -> Route) -> TestServer {
    let routes = (0..n).map(|i| (format!("/p/{i}"), route(i))).collect();
    TestServer::start_routes(routes).await
}

fn pieces_of(server: &TestServer, n: usize) -> Vec<Piece> {
    (0..n)
        .map(|i| Piece {
            url: server.url().join(&format!("/p/{i}")).unwrap(),
            range: None,
            duration_ms: 4000,
            key: None,
        })
        .collect()
}

fn whole(n: usize) -> Vec<u8> {
    (0..n).flat_map(|i| body(i, PIECE_BYTES)).collect()
}

struct Run {
    progress: Arc<Progress>,
    counter: Arc<PieceCounter>,
    cancel: watch::Sender<bool>,
    request: PiecesRequest,
}

fn run(pieces: Vec<Piece>, file: &Path, connections: u32, progress: Progress) -> Run {
    let progress = Arc::new(progress);
    let counter = Arc::new(PieceCounter::default());
    let (cancel, cancel_rx) = watch::channel(false);
    let request = PiecesRequest {
        client: reqwest::Client::new(),
        pieces,
        file: file.to_path_buf(),
        connections,
        progress: Arc::clone(&progress),
        counter: Arc::clone(&counter),
        cancel: cancel_rx,
    };
    Run {
        progress,
        counter,
        cancel,
        request,
    }
}

fn asked(server: &TestServer, path: &str) -> usize {
    server.requests().iter().filter(|r| r.path == path).count()
}

fn folder_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn pieces_end_as_one_file_in_order() {
    let server = server_with(12, |i| Route::ok(body(i, PIECE_BYTES))).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("A.f1.mp4");
    let r = run(pieces_of(&server, 12), &file, 3, Progress::new());
    let (progress, counter) = (Arc::clone(&r.progress), Arc::clone(&r.counter));

    let done = download(r.request).await.unwrap();

    assert_eq!(done, file);
    assert_eq!(std::fs::read(&file).unwrap(), whole(12));
    assert_eq!(counter.done.load(Ordering::Relaxed), 12);
    assert_eq!(counter.total.load(Ordering::Relaxed), 12);
    assert_eq!(
        progress.downloaded.load(Ordering::Relaxed),
        (12 * PIECE_BYTES) as u64
    );
    assert_eq!(*progress.output.lock().unwrap(), Some(file.clone()));
    assert_eq!(folder_names(dir.path()), ["A.f1.mp4"]);
}

#[tokio::test]
async fn no_more_than_the_look_ahead_runs() {
    // Piece 0 takes about two seconds; the rest are quick.
    let server = server_with(10, |i| {
        let route = Route::ok(body(i, PIECE_BYTES));
        if i == 0 {
            route.throttled(1_000)
        } else {
            route
        }
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("A.f1.mp4");
    let r = run(pieces_of(&server, 10), &file, 2, Progress::new());

    let running = tokio::spawn(download(r.request));
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let asked_early: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
    running.await.unwrap().unwrap();

    for path in &asked_early {
        let index: usize = path.trim_start_matches("/p/").parse().unwrap();
        assert!(index < 4, "piece {index} started while piece 0 was missing");
    }
    assert_eq!(std::fs::read(&file).unwrap(), whole(10));
}

#[tokio::test]
async fn a_slice_piece_asks_for_its_range() {
    let main: Vec<u8> = body(3, 400);
    let server =
        TestServer::start_routes(vec![("/p/main".to_string(), Route::ok(main.clone()))]).await;
    let url = server.url().join("/p/main").unwrap();
    let slice = |offset, len| Piece {
        url: url.clone(),
        range: Some(Slice { offset, len }),
        duration_ms: 1000,
        key: None,
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("A.f1.mp4");
    let r = run(
        vec![slice(0, 100), slice(100, 150), slice(250, 150)],
        &file,
        2,
        Progress::new(),
    );

    download(r.request).await.unwrap();

    let mut ranges: Vec<String> = server
        .requests()
        .iter()
        .filter_map(|r| r.range.clone())
        .collect();
    ranges.sort();
    assert_eq!(ranges, ["bytes=0-99", "bytes=100-249", "bytes=250-399"]);
    assert_eq!(std::fs::read(&file).unwrap(), main);
}

#[tokio::test]
async fn a_slice_answered_with_the_whole_file_is_refused() {
    let server = TestServer::start_routes(vec![(
        "/p/main".to_string(),
        Route::ok(body(3, 400)).ignoring_range(),
    )])
    .await;
    let piece = Piece {
        url: server.url().join("/p/main").unwrap(),
        range: Some(Slice {
            offset: 100,
            len: 50,
        }),
        duration_ms: 1000,
        key: None,
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("A.f1.mp4");
    let r = run(vec![piece], &file, 1, Progress::new());

    let result = download(r.request).await;

    assert!(matches!(result, Err(Error::Protocol(_))), "{result:?}");
    assert!(!file.exists());
}

#[tokio::test]
async fn a_failing_piece_is_retried_alone() {
    let server = server_with(8, |i| {
        if i == 5 {
            Route::status(503)
        } else {
            Route::ok(body(i, PIECE_BYTES))
        }
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("A.f1.mp4");
    let r = run(pieces_of(&server, 8), &file, 3, Progress::new());

    let running = tokio::spawn(download(r.request));
    let deadline = Instant::now() + Duration::from_secs(20);
    while asked(&server, "/p/5") < 2 {
        assert!(Instant::now() < deadline, "piece 5 was not tried twice");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.set_route("/p/5", Route::ok(body(5, PIECE_BYTES)));
    running.await.unwrap().unwrap();

    assert_eq!(asked(&server, "/p/5"), 3);
    for i in (0..8).filter(|&i| i != 5) {
        assert_eq!(asked(&server, &format!("/p/{i}")), 1, "piece {i}");
    }
    assert_eq!(std::fs::read(&file).unwrap(), whole(8));
}

#[tokio::test]
async fn an_expired_piece_stops_with_link_expired() {
    let server = server_with(8, |i| {
        if i == 4 {
            Route::status(403)
        } else {
            Route::ok(body(i, PIECE_BYTES))
        }
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("A.f1.mp4");
    let r = run(pieces_of(&server, 8), &file, 1, Progress::new());

    let result = download(r.request).await;

    assert!(
        matches!(result, Err(Error::LinkExpired { status: 403 })),
        "{result:?}"
    );
    assert!(
        state_path(&file).exists(),
        "the state is kept for a refresh"
    );
    let part = PathBuf::from(format!("{}.part", file.display()));
    assert_eq!(std::fs::read(part).unwrap(), whole(4));
}

#[tokio::test]
async fn cancel_stops_and_keeps_the_state() {
    // Each piece takes about half a second.
    let server = server_with(10, |i| Route::ok(body(i, PIECE_BYTES)).throttled(4_000)).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("A.f1.mp4");
    let r = run(pieces_of(&server, 10), &file, 2, Progress::new());
    let cancel = r.cancel;

    let running = tokio::spawn(download(r.request));
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    cancel.send(true).unwrap();
    let stopped = running.await.unwrap();
    assert!(matches!(stopped, Err(Error::Cancelled)), "{stopped:?}");
    assert!(state_path(&file).exists());

    let again = run(pieces_of(&server, 10), &file, 2, Progress::new());
    download(again.request).await.unwrap();

    assert_eq!(std::fs::read(&file).unwrap(), whole(10));
    let total: usize = (0..10).map(|i| asked(&server, &format!("/p/{i}"))).sum();
    // Only the pieces cut short by the stop are asked for again.
    assert!(total <= 10 + 2, "{total} requests for 10 pieces");
}

#[tokio::test]
async fn the_speed_cap_holds_across_pieces() {
    let server = server_with(20, |i| Route::ok(body(i, 50_000))).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("A.f1.mp4");
    let progress = Progress::with_rate_limiters(
        Arc::new(RateLimiter::new(0)),
        Arc::new(RateLimiter::new(200_000)),
    );
    let r = run(pieces_of(&server, 20), &file, 4, progress);

    let started = Instant::now();
    download(r.request).await.unwrap();

    // 1 MB at 200 KB/s, less the one-second burst the bucket starts with.
    assert!(
        started.elapsed() >= Duration::from_millis(3_500),
        "took {:?}",
        started.elapsed()
    );
}

mod encrypted {
    use super::*;
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    use rustdm::pieces::Key;

    const KEY: [u8; 16] = [0x42; 16];

    fn sequence_iv(n: usize) -> [u8; 16] {
        (n as u128).to_be_bytes()
    }

    fn sealed(i: usize) -> Vec<u8> {
        cbc::Encryptor::<aes::Aes128>::new(&KEY.into(), &sequence_iv(i).into())
            .encrypt_padded_vec_mut::<Pkcs7>(&body(i, PIECE_BYTES))
    }

    async fn encrypted_server(key: Vec<u8>) -> TestServer {
        let mut routes: Vec<(String, Route)> = (0..6)
            .map(|i| (format!("/p/{i}"), Route::ok(sealed(i))))
            .collect();
        routes.push(("/k.bin".to_string(), Route::ok(key)));
        TestServer::start_routes(routes).await
    }

    fn keyed(server: &TestServer) -> Vec<Piece> {
        let key_url = server.url().join("/k.bin").unwrap();
        pieces_of(server, 6)
            .into_iter()
            .enumerate()
            .map(|(i, piece)| Piece {
                key: Some(Key {
                    url: key_url.clone(),
                    iv: sequence_iv(i),
                }),
                ..piece
            })
            .collect()
    }

    #[tokio::test]
    async fn an_encrypted_stream_downloads_decrypted() {
        let server = encrypted_server(KEY.to_vec()).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        let r = run(keyed(&server), &file, 3, Progress::new());

        download(r.request).await.unwrap();

        assert_eq!(std::fs::read(&file).unwrap(), whole(6));
        assert_eq!(asked(&server, "/k.bin"), 1, "the key is fetched once");
    }

    #[tokio::test]
    async fn a_short_key_is_unreadable() {
        let server = encrypted_server(vec![0x42; 15]).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        let r = run(keyed(&server), &file, 2, Progress::new());

        let result = download(r.request).await;

        match result {
            Err(Error::Video { message, .. }) => {
                assert_eq!(message, "This video's playlist couldn't be read.")
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_expired_key_stops_with_link_expired() {
        let server = encrypted_server(KEY.to_vec()).await;
        server.set_route("/k.bin", Route::status(403));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("A.f1.mp4");
        let r = run(keyed(&server), &file, 2, Progress::new());

        let result = download(r.request).await;

        assert!(
            matches!(result, Err(Error::LinkExpired { status: 403 })),
            "{result:?}"
        );
    }
}
