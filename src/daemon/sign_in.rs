//! The daemon's side of a download's request context: checking what a client
//! sent, and bringing a download's cookies back after a restart.

use crate::daemon::core::{DaemonCore, DaemonState};
use crate::daemon::secrets::{SecretError, SecretStore};
use crate::daemon::task::TaskRecord;
use crate::engine::Timeouts;
use crate::error::{saved_sign_in_unreadable_message, sign_in_lapsed_message};
use crate::ipc::protocol::{DownloadStatus, DownloadSummary, RpcError};
use crate::request_context::{client_for, now_unix, ContextCookie, RequestContext};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

impl DaemonCore {
    /// Keep `cookies` as download `id`'s saved sign-in, or remove the saved one
    /// when there are none. A failure is logged; the cookies then live in memory only.
    pub(super) fn keep_secret(&self, id: &str, cookies: &[ContextCookie]) {
        let Some(secrets) = &self.secrets else {
            return;
        };
        if cookies.is_empty() {
            secrets.delete(id);
        } else if let Err(e) = secrets.save(id, cookies) {
            tracing::warn!(
                "could not save the sign-in for {id} ({}); a restart will need it again",
                e.kind()
            );
        }
    }

    pub(super) fn delete_secret(&self, id: &str) {
        if let Some(secrets) = &self.secrets {
            secrets.delete(id);
        }
    }

    /// Save the new sign-in of a stopped download that the browser started again,
    /// and queue it. The record already holds the context.
    pub(super) fn continue_with_context(
        &self,
        task: &Arc<Mutex<TaskRecord>>,
        context: &RequestContext,
    ) -> DownloadSummary {
        let id = task.lock().unwrap().id.clone();
        self.keep_secret(&id, &context.cookies);
        log_context(&id, context, "continued with");
        self.persist();
        self.schedule();
        let summary = task.lock().unwrap().to_summary();
        summary
    }
}

/// What a client asked for when it added a link.
pub struct AddRequest<'a> {
    pub url: &'a str,
    pub folder: Option<&'a std::path::Path>,
    pub file_name: Option<&'a str>,
}

/// The paused or failed download that an add of the same link with a fresh
/// sign-in should carry on: nothing is running for it, and the add names no
/// other folder or file name than the download already has.
pub fn stopped_download_of(
    state: &DaemonState,
    ask: &AddRequest<'_>,
) -> Option<Arc<Mutex<TaskRecord>>> {
    state
        .in_order()
        .find(|task_lock| {
            let task = task_lock.lock().unwrap();
            task.url == ask.url
                && !task.running
                && matches!(task.status, DownloadStatus::Paused | DownloadStatus::Failed)
                && ask.folder.is_none_or(|f| f == task.directory)
                && ask
                    .file_name
                    .is_none_or(|n| task.requested_name.as_deref() == Some(n) || task.filename == n)
        })
        .cloned()
}

/// Log that a download took a request context: counts and the Referer's origin, never a value.
pub fn log_context(id: &str, context: &RequestContext, what: &str) {
    tracing::info!(
        "{id} {what} {} cookies, referer {}",
        context.cookies.len(),
        context.referer_origin().as_deref().unwrap_or("none")
    );
}

/// A context that passed validation, with the client that carries it.
pub struct CheckedContext {
    pub context: RequestContext,
    /// None when nothing is left to send, so the shared client will do.
    pub client: Option<reqwest::Client>,
}

/// Validate what a client sent and build the client for it. The error names
/// the field that is wrong, never its value.
pub fn check(sent: &RequestContext) -> Result<CheckedContext, RpcError> {
    let context = sent
        .validated(now_unix())
        .map_err(RpcError::invalid_params)?;
    let client = client_of(&context)
        .map_err(|e| RpcError::internal_error(format!("could not use the request context: {e}")))?;
    Ok(CheckedContext { context, client })
}

fn client_of(context: &RequestContext) -> Result<Option<reqwest::Client>, crate::error::Error> {
    if context.is_empty() {
        return Ok(None);
    }
    client_for(context, Timeouts::default()).map(Some)
}

/// Bring back a restored download's client, with its cookies from `secrets`.
/// A download whose cookies cannot be had is paused with a message instead of
/// starting without them. Returns true when the record was changed.
pub fn restore(secrets: Option<&SecretStore>, task: &mut TaskRecord) -> bool {
    if matches!(
        task.status,
        DownloadStatus::Completed | DownloadStatus::Cancelled
    ) {
        // Nothing runs for these, so nothing needs the cookies.
        return std::mem::take(&mut task.has_cookies);
    }
    let cookies = if task.has_cookies {
        let loaded = match secrets {
            Some(store) => store.load(&task.id),
            None => Err(SecretError::Missing),
        };
        match loaded {
            Ok(cookies) => cookies,
            Err(e) => {
                tracing::warn!(
                    "saved sign-in for {} could not be restored ({})",
                    task.id,
                    e.kind()
                );
                let message = saved_sign_in_unreadable_message();
                return match e {
                    // Trouble reading may pass: keep the file and the claim on it.
                    SecretError::Unreadable(_) => pause(task, message),
                    _ => give_up(task, message),
                };
            }
        }
    } else {
        Vec::new()
    };
    let sent = RequestContext {
        cookies,
        referer: task.referer.clone(),
        user_agent: task.user_agent.clone(),
    };
    // Cookies that ran out while the daemon was off are dropped here.
    let Ok(CheckedContext { context, client }) = check(&sent) else {
        return give_up(task, saved_sign_in_unreadable_message());
    };
    if !sent.cookies.is_empty() && context.cookies.is_empty() {
        // Starting without the sign-in would only fetch a login page.
        return give_up(task, sign_in_lapsed_message());
    }
    let changed = context.cookies.len() != sent.cookies.len();
    task.set_context(&context, client);
    changed
}

/// Pause `task` with `message`, keeping what it knows of its sign-in.
fn pause(task: &mut TaskRecord, message: String) -> bool {
    task.status = DownloadStatus::Paused;
    task.resume_after_restart = false;
    task.error_message = Some(message);
    true
}

/// Pause `task` with `message` because its saved sign-in is gone for good.
fn give_up(task: &mut TaskRecord, message: String) -> bool {
    task.drop_cookies();
    pause(task, message)
}

/// Delete secret files that no restored download needs.
pub fn sweep(secrets: &SecretStore, tasks: &[&TaskRecord]) {
    let keep: HashSet<String> = tasks
        .iter()
        .filter(|t| t.has_cookies)
        .map(|t| t.id.clone())
        .collect();
    secrets.sweep(&keep);
}
