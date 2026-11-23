//! Looking a video page up with yt-dlp, using the stand-in program.

use rustdm::request_context::ContextCookie;
use rustdm::testing::video::{
    recorded_args, recorded_cookies, video_page_json, write_script, youtube_like, ToolResponse,
    ToolScript, VideoWorld,
};
use rustdm::video::errors::LookupFailure;
use rustdm::video::ytdlp::{
    look_up, look_up_with_limit, lookup_args, LookupRequest, MAX_LOOKUP_OUTPUT,
};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use url::Url;

fn page_url() -> Url {
    Url::parse("https://video.test/watch?v=testVideo01").unwrap()
}

fn answer(world: &VideoWorld, response: ToolResponse) {
    write_script(
        &world.ytdlp.exe,
        &ToolScript {
            responses: vec![response],
        },
    );
}

fn page_answer() -> ToolResponse {
    let base = Url::parse("https://video.test/").unwrap();
    ToolResponse {
        stdout: video_page_json(&youtube_like(&base)),
        ..ToolResponse::default()
    }
}

fn request<'a>(
    world: &'a VideoWorld,
    url: &'a Url,
    cookies: &'a [ContextCookie],
    dir: &'a Path,
) -> LookupRequest<'a> {
    LookupRequest {
        ytdlp: &world.ytdlp,
        deno: &world.deno,
        page_url: url,
        cookies,
        user_agent: None,
        referer: None,
        cookies_dir: dir,
        cancel: None,
    }
}

fn cookie(name: &str, value: &str, domain: &str, host_only: bool) -> ContextCookie {
    ContextCookie {
        name: name.to_string(),
        value: value.to_string(),
        domain: domain.to_string(),
        path: "/".to_string(),
        secure: true,
        host_only,
        expires: Some(1_900_000_000),
    }
}

fn files_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
async fn the_lookup_returns_the_page() {
    let world = VideoWorld::new().await;
    answer(&world, page_answer());
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();

    let info = look_up(request(&world, &url, &[], dir.path()))
        .await
        .unwrap();

    assert_eq!(info.title, "A test video");
    assert_eq!(info.extractor_key.as_deref(), Some("Youtube"));
}

#[tokio::test]
async fn the_args_are_fixed_and_the_link_comes_after_the_double_dash() {
    let world = VideoWorld::new().await;
    answer(&world, page_answer());
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let mut req = request(&world, &url, &[], dir.path());
    req.user_agent = Some("Agent/1.0");
    req.referer = Some("https://video.test/");

    let expected: Vec<String> = lookup_args(&req, None)
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    look_up(req).await.unwrap();

    let calls = recorded_args(&world.ytdlp.exe);
    assert_eq!(calls, vec![expected.clone()]);
    let args = &calls[0];
    assert_eq!(args[args.len() - 2], "--");
    assert_eq!(args[args.len() - 1], url.as_str());
    let deno = format!("deno:{}", world.deno.exe.display());
    assert!(args
        .windows(2)
        .any(|w| w[0] == "--js-runtimes" && w[1] == deno));
    for flag in [
        "--ignore-config",
        "--no-js-runtimes",
        "-J",
        "--no-playlist",
        "--flat-playlist",
        "--no-warnings",
    ] {
        assert!(args.iter().any(|a| a == flag), "{flag}");
    }
    assert!(args
        .windows(2)
        .any(|w| w[0] == "--user-agent" && w[1] == "Agent/1.0"));
    assert!(args
        .windows(2)
        .any(|w| w[0] == "--referer" && w[1] == "https://video.test/"));
    assert!(!args.iter().any(|a| a == "-U" || a == "--cookies"));
    assert!(!args.iter().any(|a| a == "--ffmpeg-location"));
}

#[tokio::test]
async fn the_options_come_in_the_fixed_order() {
    let world = VideoWorld::new().await;
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let mut req = request(&world, &url, &[], dir.path());
    req.user_agent = Some("Agent/1.0");
    req.referer = Some("https://video.test/");
    let file = dir.path().join("lookup-x.txt");

    let args: Vec<String> = lookup_args(&req, Some(&file))
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let deno = format!("deno:{}", world.deno.exe.display());
    let expected_start = [
        "--ignore-config",
        "--no-js-runtimes",
        "--js-runtimes",
        deno.as_str(),
        "-J",
        "--no-playlist",
        // A playlist is listed, not opened video by video: a 38-video
        // playlist took over a minute without it and 2 s with it.
        "--flat-playlist",
        "--no-warnings",
        "--cookies",
    ];
    assert_eq!(args[..9], expected_start);
    assert_eq!(args[9], file.to_string_lossy());
    assert_eq!(
        args[10..],
        [
            "--user-agent",
            "Agent/1.0",
            "--referer",
            "https://video.test/",
            "--",
            url.as_str()
        ]
    );
}

#[tokio::test]
async fn a_user_agent_or_referer_with_control_characters_is_left_out() {
    let world = VideoWorld::new().await;
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let mut req = request(&world, &url, &[], dir.path());
    req.user_agent = Some("Agent\r\nX-Evil: 1");
    req.referer = Some("https://video.test/\n");

    let args: Vec<String> = lookup_args(&req, None)
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    assert!(!args.iter().any(|a| a == "--user-agent" || a == "--referer"));
    assert!(!args.iter().any(|a| a.contains("Evil")));
    assert_eq!(args[args.len() - 2], "--");
}

#[tokio::test]
async fn a_link_that_starts_with_a_dash_stays_a_link() {
    let world = VideoWorld::new().await;
    answer(&world, page_answer());
    let dir = tempfile::tempdir().unwrap();
    let url = Url::parse("https://example.com/-U").unwrap();

    look_up(request(&world, &url, &[], dir.path()))
        .await
        .unwrap();

    let args = recorded_args(&world.ytdlp.exe).remove(0);
    assert_eq!(args[args.len() - 2], "--");
    assert_eq!(args[args.len() - 1], "https://example.com/-U");
    assert!(!args.iter().any(|a| a == "-U"));
}

#[tokio::test]
async fn a_slow_yt_dlp_is_stopped_at_the_time_limit() {
    let world = VideoWorld::new().await;
    answer(
        &world,
        ToolResponse {
            sleep_ms: 5000,
            ..ToolResponse::default()
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let started = Instant::now();

    let result = look_up_with_limit(
        request(&world, &url, &[], dir.path()),
        Duration::from_millis(300),
    )
    .await;

    assert!(matches!(result, Err(LookupFailure::TimedOut)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn too_much_output_is_refused() {
    let world = VideoWorld::new().await;
    let chunk = "x".repeat(1_000_000);
    answer(
        &world,
        ToolResponse {
            stdout: chunk,
            stdout_repeat: (MAX_LOOKUP_OUTPUT / 1_000_000 + 2) as u64,
            ..ToolResponse::default()
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();

    let result = look_up(request(&world, &url, &[], dir.path())).await;

    assert!(
        matches!(result, Err(LookupFailure::TooMuchOutput)),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_failing_yt_dlp_reports_its_stderr_and_code() {
    let world = VideoWorld::new().await;
    let stderr = "ERROR: [youtube] x: Private video";
    answer(
        &world,
        ToolResponse {
            stderr: stderr.to_string(),
            exit_code: 1,
            ..ToolResponse::default()
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();

    let result = look_up(request(&world, &url, &[], dir.path())).await;

    assert_eq!(
        result.unwrap_err(),
        LookupFailure::Exited {
            code: Some(1),
            stderr: stderr.to_string()
        }
    );
}

#[tokio::test]
async fn an_answer_that_is_not_json_is_reported() {
    let world = VideoWorld::new().await;
    answer(
        &world,
        ToolResponse {
            stdout: "not json".to_string(),
            ..ToolResponse::default()
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();

    let result = look_up(request(&world, &url, &[], dir.path())).await;

    assert!(
        matches!(result, Err(LookupFailure::NotJson(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_program_that_cannot_start_is_a_spawn_failure() {
    let world = VideoWorld::new().await;
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let mut gone = world.ytdlp.clone();
    gone.exe = dir.path().join("no-such-yt-dlp");
    let mut req = request(&world, &url, &[], dir.path());
    req.ytdlp = &gone;

    let result = look_up(req).await;

    assert!(matches!(result, Err(LookupFailure::Spawn(_))), "{result:?}");
}

#[tokio::test]
async fn without_cookies_no_file_is_made() {
    let world = VideoWorld::new().await;
    answer(&world, page_answer());
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();

    look_up(request(&world, &url, &[], dir.path()))
        .await
        .unwrap();

    assert!(files_in(dir.path()).is_empty());
    assert!(recorded_cookies(&world.ytdlp.exe).is_empty());
}

#[tokio::test]
async fn the_cookies_file_exists_only_while_yt_dlp_runs() {
    let world = VideoWorld::new().await;
    let mut response = page_answer();
    response.sleep_ms = 50;
    answer(&world, response);
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let cookies = [
        cookie("SID", "secret-1", ".video.test", false),
        ContextCookie {
            expires: None,
            secure: false,
            ..cookie("host", "secret-2", ".www.video.test", true)
        },
        cookie("evil", "a\nb\tc", ".video.test", false),
    ];

    look_up(request(&world, &url, &cookies, dir.path()))
        .await
        .unwrap();

    let seen = recorded_cookies(&world.ytdlp.exe);
    assert_eq!(seen.len(), 1);
    let name = Path::new(&seen[0].cookies_file)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        name.starts_with("lookup-") && name.ends_with(".txt"),
        "{name}"
    );
    let hex = &name["lookup-".len()..name.len() - ".txt".len()];
    assert_eq!(hex.len(), 16);
    assert!(
        hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
        "{hex}"
    );
    assert_eq!(
        seen[0].content,
        "# Netscape HTTP Cookie File\n\
         .video.test\tTRUE\t/\tTRUE\t1900000000\tSID\tsecret-1\n\
         www.video.test\tFALSE\t/\tFALSE\t0\thost\tsecret-2\n"
    );
    #[cfg(unix)]
    assert_eq!(seen[0].mode & 0o777, 0o600);
    assert!(!Path::new(&seen[0].cookies_file).exists());
    assert!(files_in(dir.path()).is_empty());
}

#[tokio::test]
async fn the_cookies_file_is_removed_when_yt_dlp_fails() {
    let world = VideoWorld::new().await;
    answer(
        &world,
        ToolResponse {
            exit_code: 1,
            sleep_ms: 50,
            ..ToolResponse::default()
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let cookies = [cookie("SID", "secret", ".video.test", false)];

    let result = look_up(request(&world, &url, &cookies, dir.path())).await;

    assert!(result.is_err());
    assert_eq!(recorded_cookies(&world.ytdlp.exe).len(), 1);
    assert!(files_in(dir.path()).is_empty());
}

#[tokio::test]
async fn the_cookies_file_is_removed_when_the_lookup_times_out() {
    let world = VideoWorld::new().await;
    answer(
        &world,
        ToolResponse {
            sleep_ms: 5000,
            ..ToolResponse::default()
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let cookies = [cookie("SID", "secret", ".video.test", false)];

    let result = look_up_with_limit(
        request(&world, &url, &cookies, dir.path()),
        Duration::from_millis(500),
    )
    .await;

    assert!(matches!(result, Err(LookupFailure::TimedOut)));
    assert!(files_in(dir.path()).is_empty());
}

#[tokio::test]
async fn cancel_kills_yt_dlp_and_removes_the_cookies_file() {
    let world = VideoWorld::new().await;
    answer(
        &world,
        ToolResponse {
            sleep_ms: 5000,
            ..ToolResponse::default()
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let url = page_url();
    let cookies = [cookie("SID", "secret", ".video.test", false)];
    let (tx, rx) = watch::channel(false);
    let mut req = request(&world, &url, &cookies, dir.path());
    req.cancel = Some(rx);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = tx.send(true);
    });
    let started = Instant::now();

    let result = look_up(req).await;

    assert_eq!(
        result.unwrap_err(),
        LookupFailure::Spawn("cancelled".to_string())
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(files_in(dir.path()).is_empty());
}
