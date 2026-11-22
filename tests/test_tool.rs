//! The stand-in yt-dlp program (`rdm-test-tool`) and the video fixtures that
//! later tests build on.

use rustdm::testing::video::{
    recorded_args, video_page_json, write_script, youtube_like, ToolResponse, ToolScript,
    VideoWorld,
};
use std::path::Path;
use std::process::{Command, Output};

fn run(exe: &Path, args: &[&str]) -> Output {
    Command::new(exe).args(args).output().unwrap()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[tokio::test]
async fn the_test_tool_answers_its_version() {
    let world = VideoWorld::new().await;

    let output = run(&world.ytdlp.exe, &["--version"]);

    assert_eq!(stdout_of(&output).trim(), "2026.11.10");
    assert_eq!(world.ytdlp.version, "2026.11.10");
    assert_eq!(world.deno.version, "2.9.7");
}

#[tokio::test]
async fn the_test_tool_plays_its_responses_in_order_and_records_args() {
    let world = VideoWorld::new().await;
    let exe = &world.ytdlp.exe;
    write_script(
        exe,
        &ToolScript {
            responses: vec![
                ToolResponse {
                    stdout: "one".to_string(),
                    ..ToolResponse::default()
                },
                ToolResponse {
                    stdout: "two".to_string(),
                    exit_code: 3,
                    ..ToolResponse::default()
                },
            ],
        },
    );

    let first = run(exe, &["-J", "https://example.test/a"]);
    let second = run(exe, &["--flat-playlist", "x y"]);
    let third = run(exe, &[]);

    assert_eq!(stdout_of(&first), "one");
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(stdout_of(&second), "two");
    assert_eq!(second.status.code(), Some(3));
    assert_eq!(stdout_of(&third), "two");
    assert_eq!(third.status.code(), Some(3));
    assert_eq!(
        recorded_args(exe),
        vec![
            vec!["-J".to_string(), "https://example.test/a".to_string()],
            vec!["--flat-playlist".to_string(), "x y".to_string()],
            Vec::<String>::new(),
        ]
    );
}

#[tokio::test]
async fn the_test_tool_repeats_its_output_and_writes_stderr() {
    let world = VideoWorld::new().await;
    let exe = &world.ytdlp.exe;
    write_script(
        exe,
        &ToolScript {
            responses: vec![ToolResponse {
                stdout: "ab".to_string(),
                stderr: "oops".to_string(),
                stdout_repeat: 3,
                ..ToolResponse::default()
            }],
        },
    );

    let output = run(exe, &["-J"]);

    assert_eq!(stdout_of(&output), "ababab");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "oops");
}

#[tokio::test]
async fn the_youtube_like_fixture_parses_as_json_with_all_formats() {
    let world = VideoWorld::new().await;
    let spec = youtube_like(&world.server.url());

    let json: serde_json::Value = serde_json::from_str(&video_page_json(&spec)).unwrap();

    let formats = json["formats"].as_array().unwrap();
    assert_eq!(formats.len(), spec.formats.len());
    assert_eq!(json["_type"], "video");
    assert_eq!(json["extractor_key"], spec.extractor_key);
    let dub = formats.iter().find(|f| f["format_id"] == "140-3").unwrap();
    assert!(dub["filesize"].is_null());
    assert!(dub["filesize_approx"].is_u64());
    assert_eq!(dub["vcodec"], "none");
    let video_only = formats.iter().find(|f| f["height"] == 2160).unwrap();
    assert_eq!(video_only["acodec"], "none");
    assert_eq!(
        formats.iter().find(|f| f["protocol"] == "https").unwrap()["downloader_options"]
            ["http_chunk_size"],
        10485760
    );
}
