//! A video stream whose link expired gets fresh links from yt-dlp, with the
//! same cookies, and the run goes on where the stream stopped (spec 6.4).

use super::core::DaemonCore;
use super::task::TaskRecord;
use super::video_task::LookupOutcome;
use crate::engine::{Outcome, PauseReason};
use crate::error::Error;
use crate::ipc::protocol::DownloadStatus;
use crate::video::refresh::{
    may_refresh, may_refresh_playlist, note_refresh, refresh_limit_message, refreshed_streams,
    DIRECT_EXPIRED, REFRESH_STEP, VIDEO_CHANGED,
};
use crate::video::streams::stream_plans;
use crate::video::{site_name, PlanOrigin, VideoPlan};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use url::Url;

/// How a refresh ended.
pub(super) enum Refreshed {
    /// The streams have fresh links: run the same step again.
    Again,
    /// Too many refreshes lately: the download pauses with this message.
    Pause(String),
    /// The download settles with this error.
    Settle(Error),
}

impl DaemonCore {
    /// The current stream's link expired: count a refresh and get fresh
    /// links, unless the site has refused too often lately.
    pub(super) async fn refresh_links(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        cancel: &watch::Receiver<bool>,
    ) -> Refreshed {
        let now = unix_now();
        let (id, from_playlist) = {
            let mut guard = record.lock().unwrap();
            let rec = &mut *guard;
            // The stream's file must be in the plan before it is run again,
            // or the next run would start a new file instead of resuming.
            rec.sync_from_progress();
            let Some(plan) = rec.video.as_mut() else {
                return Refreshed::Settle(Error::Cancelled);
            };
            let from_playlist = plan.origin == PlanOrigin::Playlist;
            if from_playlist && !may_refresh_playlist(&plan.refreshes, now) {
                return Refreshed::Pause(DIRECT_EXPIRED.to_string());
            }
            if !from_playlist && !may_refresh(&plan.refreshes, now) {
                return Refreshed::Pause(refresh_limit_message(&plan_site(plan)));
            }
            note_refresh(&mut plan.refreshes, now);
            (rec.id.clone(), from_playlist)
        };
        self.persist();
        // A direct link has no page to ask: the next run reads its playlist again.
        if from_playlist {
            return Refreshed::Again;
        }
        let cookies = self.cookies_for(&id);
        let found = self
            .look_up_video(record, cookies, REFRESH_STEP, cancel.clone())
            .await;
        let refreshed = self.take_fresh_streams(record, found);
        record.lock().unwrap().step = None;
        refreshed
    }

    /// Settle a run whose refresh limit was reached: paused, with `message`.
    pub(super) fn pause_with(&self, record: &Arc<Mutex<TaskRecord>>, message: String) {
        record.lock().unwrap().error_message = Some(message);
        // A stop the user did not ask for settles as Paused, keeping the message.
        self.finish_task(record, Err(Error::Cancelled));
    }

    fn take_fresh_streams(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        found: LookupOutcome,
    ) -> Refreshed {
        let info = match found {
            LookupOutcome::Choices(info, _) => info,
            LookupOutcome::Message { text, transient } => {
                return Refreshed::Settle(Error::Video {
                    message: text,
                    transient,
                })
            }
            LookupOutcome::NoTools(e) => {
                return Refreshed::Settle(Error::Tool {
                    message: e.to_string(),
                    transient: e.is_transient(),
                })
            }
            LookupOutcome::Cancelled => return Refreshed::Settle(Error::Cancelled),
        };
        {
            let mut rec = record.lock().unwrap();
            let Some(plan) = rec.video.as_mut() else {
                return Refreshed::Settle(Error::Cancelled);
            };
            match refreshed_streams(&plan.streams, &stream_plans(&info)) {
                Ok(streams) => plan.streams = streams,
                Err(_) => return Refreshed::Settle(video_changed()),
            }
        }
        self.persist();
        Refreshed::Again
    }
}

/// Whether a stream run ended on an expired link, while the download is still
/// meant to run (not stopped by the user meanwhile).
pub(super) fn link_expired_while_running(
    outcome: &Result<Outcome, Error>,
    record: &Arc<Mutex<TaskRecord>>,
    cancel: &watch::Receiver<bool>,
) -> bool {
    let expired = matches!(
        outcome,
        Ok(Outcome::Paused {
            reason: PauseReason::LinkExpired { .. },
            ..
        }) | Err(Error::LinkExpired { .. })
    );
    expired && !*cancel.borrow() && record.lock().unwrap().status == DownloadStatus::Active
}

/// The stream on the site is not the one partly downloaded.
pub(super) fn video_changed() -> Error {
    Error::Video {
        message: VIDEO_CHANGED.to_string(),
        transient: false,
    }
}

/// The site's name for the refresh-limit message; plans saved by 1a have
/// none, so their page's host stands in.
fn plan_site(plan: &VideoPlan) -> String {
    if !plan.site.is_empty() {
        return plan.site.clone();
    }
    Url::parse(&plan.page_url)
        .map(|url| site_name(&Default::default(), &url))
        .unwrap_or_default()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_without_a_site_names_its_page_host() {
        let plan = VideoPlan {
            page_url: "https://www.example.com/watch?v=1".to_string(),
            title: String::new(),
            duration_secs: None,
            choice: None,
            choices: Vec::new(),
            offered: Vec::new(),
            default: 0,
            streams: Vec::new(),
            step: crate::video::VideoStep::Stream(0),
            refreshes: Vec::new(),
            site: String::new(),
            origin: crate::video::PlanOrigin::Page,
        };

        assert_eq!(plan_site(&plan), "example.com");
        let named = VideoPlan {
            site: "YouTube".to_string(),
            ..plan
        };
        assert_eq!(plan_site(&named), "YouTube");
    }
}
