//! The dashboard driven end to end: real daemon, real test server, scripted
//! terminal events, and ratatui's in-memory backend.

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use rustdm::daemon::{DaemonCore, DaemonServer};
use rustdm::ipc::client::{ClientError, DaemonClient};
use rustdm::ipc::protocol::{
    AddDownloadParams, DownloadStatus, DownloadSummary, SetDefaultsParams, SetLimitsParams,
};
use rustdm::ipc::transport::IpcEndpoint;
use rustdm::testing::{pattern_bytes, Mode, TestServer};
use rustdm::tui::{run_app, run_plain};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
const WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// A daemon whose default folder is `folder`, so no test writes to the real Downloads.
async fn serve_into(folder: &Path) -> (IpcEndpoint, DaemonClient) {
    let (endpoint, mut client) = serve().await;
    client
        .set_defaults(SetDefaultsParams {
            folder: Some(folder.to_path_buf()),
            ..Default::default()
        })
        .await
        .expect("set defaults");
    (endpoint, client)
}

async fn serve() -> (IpcEndpoint, DaemonClient) {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let endpoint = IpcEndpoint::test_endpoint(&format!("tui_{id}"));
    let server = DaemonServer::bind_with_core(&endpoint, DaemonCore::new())
        .await
        .expect("bind daemon");
    tokio::spawn(async move { server.run().await.unwrap() });
    let client = DaemonClient::connect(&endpoint).await.expect("connect");
    (endpoint, client)
}

fn add_params(mock: &TestServer, name: &str, dir: &Path) -> AddDownloadParams {
    AddDownloadParams {
        url: mock.url().join(name).unwrap().to_string(),
        destination: Some(dir.to_path_buf()),
        filename: Some(name.to_string()),
        connections: Some(1),
        max_bytes_per_sec: None,
    }
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn shift(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT))
}

async fn wait_for<F>(client: &mut DaemonClient, what: &str, mut done: F) -> Vec<DownloadSummary>
where
    F: FnMut(&[DownloadSummary]) -> bool,
{
    let start = Instant::now();
    loop {
        let list = client.list().await.expect("list");
        if done(&list) {
            return list;
        }
        assert!(start.elapsed() < WAIT_TIMEOUT, "{what}: {list:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

/// Start the dashboard in the background; it returns its terminal when it quits.
fn start_dashboard(
    endpoint: &IpcEndpoint,
) -> (
    mpsc::Sender<Event>,
    tokio::task::JoinHandle<Terminal<TestBackend>>,
) {
    let (events_tx, events_rx) = mpsc::channel(64);
    let endpoint = endpoint.clone();
    let dashboard = tokio::spawn(async move {
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        run_app(&mut terminal, events_rx, &endpoint)
            .await
            .expect("dashboard");
        terminal
    });
    (events_tx, dashboard)
}

#[tokio::test]
async fn the_dashboard_adds_pauses_and_shows_a_download() {
    let mock = TestServer::start(
        pattern_bytes(4 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 64 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, mut client) = serve_into(dir.path()).await;
    let (events, dashboard) = start_dashboard(&endpoint);

    // Press "a", paste the URL, and confirm with Enter.
    let url = mock.url().join("from-tui.bin").unwrap().to_string();
    events.send(key(KeyCode::Char('a'))).await.unwrap();
    events.send(Event::Paste(url.clone())).await.unwrap();
    events.send(key(KeyCode::Enter)).await.unwrap();
    let list = wait_for(&mut client, "the dashboard never added it", |list| {
        list.iter().any(|d| d.url == url && d.downloaded_bytes > 0)
    })
    .await;
    assert_eq!(list[0].destination.parent(), Some(dir.path()));

    // The new download is selected, so "p" pauses it.
    events.send(key(KeyCode::Char('p'))).await.unwrap();
    wait_for(&mut client, "the dashboard never paused it", |list| {
        list[0].status == DownloadStatus::Paused
    })
    .await;

    // Let a refresh redraw the screen, then quit.
    tokio::time::sleep(Duration::from_millis(700)).await;
    events.send(key(KeyCode::Char('q'))).await.unwrap();
    let terminal = tokio::time::timeout(WAIT_TIMEOUT, dashboard)
        .await
        .expect("dashboard did not quit")
        .unwrap();
    let screen = screen_text(&terminal);
    for expected in ["dl_1", "from-tui.bin", "paused", "Stopped (1)"] {
        assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
    }
}

#[tokio::test]
async fn the_dashboard_reorders_the_queue_and_resumes_downloads() {
    let mock = TestServer::start(
        pattern_bytes(256 * 1024),
        Mode::Throttle {
            bytes_per_sec: 256 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, mut client) = serve_into(dir.path()).await;
    client
        .set_limits(SetLimitsParams {
            max_active: Some(1),
            max_bytes_per_sec: None,
        })
        .await
        .unwrap();
    for name in ["a.bin", "b.bin"] {
        let added = client
            .add(add_params(&mock, name, dir.path()))
            .await
            .unwrap();
        client.pause(&added.id).await.unwrap();
    }
    wait_for(&mut client, "pauses never settled", |list| {
        list.iter().all(|d| d.status == DownloadStatus::Paused)
    })
    .await;

    let (events, dashboard) = start_dashboard(&endpoint);
    // dl_1 is selected first; move it below dl_2.
    events.send(shift(KeyCode::Down)).await.unwrap();
    let list = wait_for(&mut client, "the dashboard never moved it", |list| {
        list[0].id == "dl_2"
    })
    .await;
    assert_eq!(list[1].id, "dl_1");

    // The highlight follows the moved download, so "r" resumes dl_1; then
    // select dl_2 above it and resume that too.
    events.send(key(KeyCode::Char('r'))).await.unwrap();
    events.send(key(KeyCode::Up)).await.unwrap();
    events.send(key(KeyCode::Char('r'))).await.unwrap();
    wait_for(&mut client, "resumed downloads never finished", |list| {
        list.iter().all(|d| d.status == DownloadStatus::Completed)
    })
    .await;

    events.send(key(KeyCode::Char('q'))).await.unwrap();
    tokio::time::timeout(WAIT_TIMEOUT, dashboard)
        .await
        .expect("dashboard did not quit")
        .unwrap();
}

#[tokio::test]
async fn the_add_dialog_adds_with_the_daemon_defaults() {
    let body = pattern_bytes(256 * 1024);
    let mock = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, mut client) = serve_into(dir.path()).await;
    client
        .set_defaults(SetDefaultsParams {
            connections: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    let (events, dashboard) = start_dashboard(&endpoint);
    // Let the first refresh load the defaults before the dialog opens.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let url = mock.url().join("with-defaults.bin").unwrap().to_string();
    events.send(key(KeyCode::Char('a'))).await.unwrap();
    events.send(Event::Paste(url.clone())).await.unwrap();
    events.send(key(KeyCode::Enter)).await.unwrap();
    wait_for(&mut client, "the dialog never added it", |list| {
        list.iter()
            .any(|d| d.url == url && d.status == DownloadStatus::Completed)
    })
    .await;
    assert_eq!(
        std::fs::read(dir.path().join("with-defaults.bin")).unwrap(),
        body
    );

    events.send(key(KeyCode::Char('q'))).await.unwrap();
    tokio::time::timeout(WAIT_TIMEOUT, dashboard)
        .await
        .expect("dashboard did not quit")
        .unwrap();
}

#[tokio::test]
async fn stopping_the_daemon_from_the_tui_pauses_downloads() {
    let mock = TestServer::start(
        pattern_bytes(4 * 1024 * 1024),
        Mode::Throttle {
            bytes_per_sec: 64 * 1024,
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let endpoint = IpcEndpoint::test_endpoint(&format!("tui_{id}"));
    let core = DaemonCore::new();
    let server = DaemonServer::bind_with_core(&endpoint, core.clone())
        .await
        .expect("bind daemon");
    let daemon = tokio::spawn(async move { server.run().await.unwrap() });
    let mut client = DaemonClient::connect(&endpoint).await.expect("connect");
    let added = client
        .add(add_params(&mock, "slow.bin", dir.path()))
        .await
        .unwrap();
    wait_for(&mut client, "the download never started", |list| {
        list[0].downloaded_bytes > 0
    })
    .await;

    let (events, dashboard) = start_dashboard(&endpoint);
    events
        .send(Event::Key(KeyEvent::new(
            KeyCode::Char('Q'),
            KeyModifiers::SHIFT,
        )))
        .await
        .unwrap();
    events.send(key(KeyCode::Char('y'))).await.unwrap();
    tokio::time::timeout(WAIT_TIMEOUT, dashboard)
        .await
        .expect("the dashboard quits after stopping rdmd")
        .unwrap();
    tokio::time::timeout(WAIT_TIMEOUT, daemon)
        .await
        .expect("rdmd stopped")
        .unwrap();

    assert!(matches!(
        DaemonClient::connect(&endpoint).await,
        Err(ClientError::NotRunning)
    ));
    let status = core.get_status(&added.id).unwrap().status;
    assert!(
        !matches!(status, DownloadStatus::Active | DownloadStatus::Completed),
        "{status:?}"
    );
}

#[tokio::test]
async fn esc_no_longer_quits_the_dashboard() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, _client) = serve_into(dir.path()).await;
    let (events, dashboard) = start_dashboard(&endpoint);

    events.send(key(KeyCode::Esc)).await.unwrap();
    events.send(key(KeyCode::Char('s'))).await.unwrap();
    events.send(key(KeyCode::Esc)).await.unwrap();
    events.send(key(KeyCode::Esc)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!dashboard.is_finished(), "Esc must not quit");

    events.send(key(KeyCode::Char('q'))).await.unwrap();
    tokio::time::timeout(WAIT_TIMEOUT, dashboard)
        .await
        .expect("q quits")
        .unwrap();
}

#[tokio::test]
async fn settings_saves_the_default_folder_to_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, mut client) = serve_into(dir.path()).await;
    let new_folder = dir.path().join("from-settings");
    let (events, dashboard) = start_dashboard(&endpoint);
    // Let the first refresh load the defaults, so editing starts from them.
    tokio::time::sleep(Duration::from_millis(200)).await;

    events.send(key(KeyCode::Char('s'))).await.unwrap();
    events.send(key(KeyCode::Down)).await.unwrap();
    events.send(key(KeyCode::Down)).await.unwrap();
    events.send(key(KeyCode::Enter)).await.unwrap();
    let clear = Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    events.send(clear).await.unwrap();
    events
        .send(Event::Paste(new_folder.display().to_string()))
        .await
        .unwrap();
    events.send(key(KeyCode::Enter)).await.unwrap();

    let start = Instant::now();
    while client.get_defaults().await.unwrap().folder != new_folder {
        assert!(start.elapsed() < WAIT_TIMEOUT, "the folder was never saved");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    events.send(key(KeyCode::Char('q'))).await.unwrap();
    let terminal = tokio::time::timeout(WAIT_TIMEOUT, dashboard)
        .await
        .expect("dashboard did not quit")
        .unwrap();
    let text = screen_text(&terminal);
    assert!(text.contains("Defaults for new downloads saved"), "{text}");
    assert!(text.contains("from-settings"), "{text}");
}

#[tokio::test]
async fn the_dashboard_quits_when_its_event_stream_ends() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, _client) = serve_into(dir.path()).await;
    let (events, dashboard) = start_dashboard(&endpoint);
    drop(events);
    let terminal = tokio::time::timeout(WAIT_TIMEOUT, dashboard)
        .await
        .expect("dashboard did not quit")
        .unwrap();
    assert!(screen_text(&terminal).contains("[ + Add ]"));
}

#[tokio::test]
async fn plain_mode_prints_changing_rows_until_nothing_is_left_to_do() {
    let body = pattern_bytes(128 * 1024);
    let mock = TestServer::start(body.clone(), Mode::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let (_endpoint, mut client) = serve().await;

    let mut out = Vec::new();
    run_plain(&mut client, &mut out, Duration::from_millis(20))
        .await
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "No downloads.\n");

    client
        .add(add_params(&mock, "plain.bin", dir.path()))
        .await
        .unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(
        WAIT_TIMEOUT,
        run_plain(&mut client, &mut out, Duration::from_millis(20)),
    )
    .await
    .expect("plain mode never finished")
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    let last = text.lines().last().unwrap_or_default();
    assert!(last.starts_with("dl_1"), "{text}");
    assert!(last.contains("completed"), "{text}");
    assert!(last.contains("plain.bin"), "{text}");
    assert_eq!(std::fs::read(dir.path().join("plain.bin")).unwrap(), body);
}
