//! Small helpers for the video fields of a download record.

use crate::ipc::protocol::VideoSummary;
use crate::state::{part_path, state_path};
use crate::video::join::joining_path;
use crate::video::{
    parse_quality, Container, Quality, StreamKind, StreamPlan, VideoPlan, VideoStep,
};
use std::path::{Path, PathBuf};

/// The file a stream is saved as: `<stem>.f<format_id>.<ext>`, where `<stem>`
/// is the final name without its container ending. A stream without an ending
/// of its own takes the container's.
pub fn stream_file_name(final_name: &str, container: Container, stream: &StreamPlan) -> String {
    let ending = format!(".{}", container.ext());
    let stem = final_name
        .len()
        .checked_sub(ending.len())
        .filter(|&at| at > 0)
        .and_then(|at| {
            let tail = final_name.get(at..)?;
            tail.eq_ignore_ascii_case(&ending)
                .then(|| &final_name[..at])
        })
        .unwrap_or(final_name);
    let ext = if stream.ext.is_empty() {
        container.ext()
    } else {
        stream.ext.as_str()
    };
    format!("{stem}.f{}.{ext}", stream.format_id)
}

/// Every file a video download may have left beside `final_path`: each
/// stream's `.part`, `.part.rdm` and finished file, and the join's temporary file.
pub fn video_leftovers(plan: &VideoPlan, final_path: &Path) -> Vec<PathBuf> {
    let streams = plan.streams.iter().filter_map(|s| s.file.as_deref());
    let mut files: Vec<PathBuf> = streams
        .flat_map(|file| [part_path(file), state_path(file), file.to_path_buf()])
        .collect();
    files.push(joining_path(final_path));
    files
}

/// What the row says while stream `index` downloads: `video 1 of 2 · 37%`.
pub fn step_text(plan: &VideoPlan, index: usize, percent: u8) -> String {
    let kind = match plan.streams.get(index).map(|s| s.kind) {
        Some(StreamKind::Audio) => "audio",
        _ => "video",
    };
    format!(
        "{kind} {} of {} · {percent}%",
        index + 1,
        plan.streams.len()
    )
}

/// What the row says while the streams are joined: `Joining 64%`, or
/// `Joining…` while ffmpeg has not said how far it is.
pub fn joining_text(percent: Option<u8>) -> String {
    match percent {
        Some(percent) => format!("Joining {percent}%"),
        None => "Joining…".to_string(),
    }
}

/// The free space a video needs before its first stream, without the safety
/// margin: twice its size when its streams are joined (both parts and the
/// joined file are on disk at once), else its size. None when a size is unknown.
pub fn video_space_needed(plan: &VideoPlan) -> Option<u64> {
    let size = video_total(plan)?;
    let copies = if plan.streams.len() > 1 { 2 } else { 1 };
    Some(size.saturating_mul(copies))
}

/// Keep the file the engine named for the stream being downloaded, so a
/// restart resumes it. Returns true when that changed the plan.
pub fn record_stream_file(plan: &mut VideoPlan, file: Option<PathBuf>) -> bool {
    let VideoStep::Stream(index) = plan.step else {
        return false;
    };
    match (plan.streams.get_mut(index), file) {
        (Some(stream), Some(file)) if stream.file.as_ref() != Some(&file) => {
            stream.file = Some(file);
            true
        }
        _ => false,
    }
}

/// After the stream files were deleted: the next run fetches every stream again.
pub fn forget_stream_files(plan: &mut VideoPlan) {
    if !matches!(plan.step, VideoStep::Stream(_) | VideoStep::Join) {
        return;
    }
    for stream in &mut plan.streams {
        stream.file = None;
    }
    plan.step = VideoStep::Stream(0);
}

/// The size of the whole video, when every stream's size is known.
pub fn video_total(plan: &VideoPlan) -> Option<u64> {
    plan.streams
        .iter()
        .try_fold(0u64, |sum, s| Some(sum.saturating_add(s.size?)))
}

/// What a video download shows the user. The choices go out only while the
/// user has to pick one.
pub fn summarize(plan: &VideoPlan, choosing: bool) -> VideoSummary {
    VideoSummary {
        title: plan.title.clone(),
        duration_secs: plan.duration_secs,
        choices: if choosing {
            plan.choices.clone()
        } else {
            Vec::new()
        },
        // Always one of the rows, whatever the queue file says.
        default: choosing.then_some(plan.default.min(plan.choices.len().saturating_sub(1))),
        chosen: plan.choice.clone(),
    }
}

/// Read the quality text a queue file kept. Text that cannot be read is
/// logged and dropped, so the download asks the user instead.
pub fn read_saved_quality(id: &str, text: Option<String>) -> (Option<Quality>, Option<String>) {
    let Some(text) = text else {
        return (None, None);
    };
    match parse_quality(&text) {
        Ok(quality) => (Some(quality), Some(text)),
        Err(_) => {
            tracing::warn!("download {id} has a saved quality that cannot be read; ignoring it");
            (None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::VideoChoice;

    #[test]
    fn the_joining_step_shows_a_percent_once_it_is_known() {
        assert_eq!(joining_text(Some(64)), "Joining 64%");
        assert_eq!(joining_text(None), "Joining…");
    }

    fn plan_with(rows: usize, default: usize) -> VideoPlan {
        let choice = |n: usize| VideoChoice {
            id: format!("{n}-mp4"),
            label: format!("{n}p"),
            container: Container::Mp4,
            height: Some(n as u32),
            size_bytes: None,
            size_is_estimate: false,
            streams: Vec::new(),
        };
        VideoPlan {
            page_url: "https://video.test/watch".to_string(),
            title: "A video".to_string(),
            duration_secs: None,
            choice: None,
            choices: (1..=rows).map(choice).collect(),
            offered: Vec::new(),
            default,
            streams: Vec::new(),
            step: VideoStep::Choosing,
            refreshes: Vec::new(),
        }
    }

    #[test]
    fn the_default_row_is_always_one_of_the_choices() {
        assert_eq!(summarize(&plan_with(3, 1), true).default, Some(1));
        // A queue file edited by hand, or written by a bug, can point past the list.
        assert_eq!(summarize(&plan_with(3, 9), true).default, Some(2));
        assert_eq!(summarize(&plan_with(3, 9), false).default, None);
    }

    fn stream(format_id: &str, kind: StreamKind, ext: &str) -> StreamPlan {
        StreamPlan {
            format_id: format_id.to_string(),
            kind,
            ext: ext.to_string(),
            url: "https://video.test/stream".to_string(),
            size: None,
            headers: Vec::new(),
            request_bytes: None,
            file: None,
        }
    }

    fn chosen_plan() -> VideoPlan {
        VideoPlan {
            streams: vec![
                stream("137", StreamKind::Video, "mp4"),
                stream("140", StreamKind::Audio, "m4a"),
            ],
            step: VideoStep::Stream(0),
            ..plan_with(0, 0)
        }
    }

    #[test]
    fn a_stream_file_is_the_stem_the_format_and_the_streams_ending() {
        let video = stream("137", StreamKind::Video, "mp4");
        let name = |final_name, container| stream_file_name(final_name, container, &video);

        assert_eq!(
            name("Rust in 100 Seconds.mp4", Container::Mp4),
            "Rust in 100 Seconds.f137.mp4"
        );
        assert_eq!(name("Clip.MP4", Container::Mp4), "Clip.f137.mp4");
        assert_eq!(name("clip.mkv.webm", Container::Webm), "clip.mkv.f137.mp4");
        assert_eq!(name(".mp4", Container::Mp4), ".mp4.f137.mp4");
        // A stream without an ending of its own takes the container's, never a bare dot.
        let bare = stream("140", StreamKind::Audio, "");
        assert_eq!(
            stream_file_name("Clip.m4a", Container::M4a, &bare),
            "Clip.f140.m4a"
        );
    }

    #[test]
    fn the_step_names_the_kind_and_the_place_of_the_stream() {
        let two = chosen_plan();
        assert_eq!(step_text(&two, 0, 37), "video 1 of 2 · 37%");
        assert_eq!(step_text(&two, 1, 91), "audio 2 of 2 · 91%");
        let one = VideoPlan {
            streams: vec![stream("140", StreamKind::Audio, "m4a")],
            ..chosen_plan()
        };
        assert_eq!(step_text(&one, 0, 37), "audio 1 of 1 · 37%");
    }

    #[test]
    fn the_leftovers_are_each_streams_files_and_the_join_temp() {
        let mut plan = chosen_plan();
        plan.streams[0].file = Some(PathBuf::from("/d/A.f137.mp4"));

        let files = video_leftovers(&plan, Path::new("/d/A.mp4"));

        let expected = [
            "/d/A.f137.mp4.part",
            "/d/A.f137.mp4.part.rdm",
            "/d/A.f137.mp4",
            "/d/A.mp4.joining",
        ]
        .map(PathBuf::from);
        assert_eq!(files, expected);
    }

    #[test]
    fn the_engines_file_is_kept_for_the_current_stream_only() {
        let mut plan = chosen_plan();
        let file = PathBuf::from("/d/A.f137.mp4");

        assert!(record_stream_file(&mut plan, Some(file.clone())));
        assert!(!record_stream_file(&mut plan, Some(file.clone())));
        assert!(!record_stream_file(&mut plan, None));
        assert_eq!(plan.streams[0].file.as_ref(), Some(&file));
        assert_eq!(plan.streams[1].file, None);

        plan.step = VideoStep::Join;
        assert!(!record_stream_file(&mut plan, Some(PathBuf::from("/d/x"))));
    }

    #[test]
    fn forgetting_the_stream_files_starts_the_streams_over() {
        let mut plan = chosen_plan();
        plan.streams[0].file = Some(PathBuf::from("/d/A.f137.mp4"));
        plan.step = VideoStep::Stream(1);

        forget_stream_files(&mut plan);

        assert_eq!(plan.step, VideoStep::Stream(0));
        assert!(plan.streams.iter().all(|s| s.file.is_none()));
        let mut choosing = plan_with(2, 0);
        forget_stream_files(&mut choosing);
        assert_eq!(choosing.step, VideoStep::Choosing);
    }
}
