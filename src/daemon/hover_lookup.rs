//! `video.look_up`: the browser asks for a video before the click (spec 1f,
//! sections 3 and 4). Nothing is added to the list; the answer is kept so the
//! click's download starts without a second lookup.

use super::core::DaemonCore;
use super::saved_answers::AnswerKey;
use super::sign_in;
use super::video_playlist::{is_m3u8_link, read_playlist_page};
use super::video_task::{LookupOutcome, Target, ToolSource, TOOLS_NOT_READY};
use crate::ipc::protocol::{LookUpParams, LookUpResult, RpcError};
use crate::video::{PlanOrigin, VideoChoices};
use std::time::Instant;
use tokio::sync::watch;
use url::Url;

/// What a lookup stopped by a newer one answers.
pub const LOOKUP_CANCELLED: &str = "cancelled";

impl DaemonCore {
    /// Look `params.url` up, or answer from a saved lookup. One runs at a
    /// time: a newer call stops the one still running.
    pub async fn look_up_for_hover(&self, params: LookUpParams) -> Result<LookUpResult, RpcError> {
        let url = Url::parse(&params.url)
            .map_err(|e| RpcError::invalid_params(format!("Invalid URL: {e}")))?;
        let context = params.context.as_ref().map(sign_in::validate).transpose()?;
        let key = AnswerKey {
            url: params.url.clone(),
            has_cookies: context.as_ref().is_some_and(|c| !c.cookies.is_empty()),
        };
        if let Some(saved) = self.saved_answers.get(&key, Instant::now()) {
            return Ok(result_of(&saved.choices));
        }
        let cancel = self.take_hover_turn();
        let (outcome, origin) = if is_m3u8_link(&params.url) {
            let read = read_playlist_page(&params.url, context.as_ref(), &cancel).await;
            (read, PlanOrigin::Playlist)
        } else {
            let Some(tools) = self.tools.clone() else {
                return Err(RpcError::lookup_failed(TOOLS_NOT_READY));
            };
            let target = Target::from_request(&url, context.as_ref(), self.data_dir());
            let source = ToolSource::InstalledOnly(&tools);
            (
                self.look_up_page(&target, source, cancel).await,
                PlanOrigin::Page,
            )
        };
        match outcome {
            LookupOutcome::Choices(info, choices) => {
                let result = result_of(&choices);
                self.saved_answers
                    .put(key, *info, choices, origin, Instant::now());
                Ok(result)
            }
            LookupOutcome::Message { text, .. } => Err(RpcError::lookup_failed(text)),
            LookupOutcome::NoTools(e) => Err(RpcError::lookup_failed(e.to_string())),
            LookupOutcome::Cancelled => Err(RpcError::lookup_failed(LOOKUP_CANCELLED)),
        }
    }

    /// A stop signal for this lookup; the one still running is stopped.
    fn take_hover_turn(&self) -> watch::Receiver<bool> {
        let (stop, stopped) = watch::channel(false);
        let mut turn = self.hover_cancel.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(older) = turn.replace(stop) {
            let _ = older.send(true);
        }
        stopped
    }
}

fn result_of(choices: &VideoChoices) -> LookUpResult {
    LookUpResult {
        title: choices.title.clone(),
        duration_secs: choices.duration_secs,
        choices: choices.choices.clone(),
        default: choices.default,
    }
}
