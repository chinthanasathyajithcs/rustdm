//! Small helpers for the video fields of a download record.

use crate::ipc::protocol::VideoSummary;
use crate::video::{parse_quality, Quality, VideoPlan};

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
    use crate::video::{Container, VideoChoice, VideoStep};

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
}
