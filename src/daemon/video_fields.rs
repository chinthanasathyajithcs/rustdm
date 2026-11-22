//! Small helpers for the video fields of a download record.

use crate::ipc::protocol::VideoSummary;
use crate::video::{parse_quality, Quality, VideoPlan};

/// What a video download shows the user. The choices go out only while the
/// user has to pick one.
pub fn summarize(plan: &VideoPlan, choosing: bool) -> VideoSummary {
    VideoSummary {
        title: plan.title.clone(),
        choices: if choosing {
            plan.choices.clone()
        } else {
            Vec::new()
        },
        default: None,
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
