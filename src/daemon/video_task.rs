//! The daemon's side of a video page: when the link check finds a web page,
//! look it up with yt-dlp and put the download in `Choosing`, or fail it with
//! the reason (spec sections 4, 5.1 and 8).

use super::core::DaemonCore;
use super::task::TaskRecord;
use crate::engine::{Outcome, Progress};
use crate::error::Error;
use crate::ipc::protocol::DownloadStatus;
use crate::request_context::ContextCookie;
use crate::tools::display::getting_text;
use crate::tools::{FetchCtx, Freshness, Installed, Tool, ToolError};
use crate::video::choices::{build_choices, pick};
use crate::video::errors::{
    failure_message, is_extractor_error, page_message, LookupFailure, NO_VIDEO,
};
use crate::video::info::PageInfo;
use crate::video::ytdlp::{look_up, LookupRequest};
use crate::video::{site_name, ChosenQuality, Container, VideoChoices, VideoPlan, VideoStep};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use url::Url;

/// What a row says while yt-dlp reads the page.
const LOOKING_STEP: &str = "Looking for a video on this page…";
/// yt-dlp is asked for a newer version after an extractor error at most this often.
pub const EXTRACTOR_UPDATE_EVERY: Duration = Duration::from_secs(60 * 60);
/// What a download that has its quality fails with until video downloading arrives.
const VIDEO_PLACEHOLDER: &str = "Video downloading arrives in the next update";
/// What `look_up` reports when it was cancelled.
const CANCELLED: &str = "cancelled";
/// The name given to a video whose title leaves nothing usable.
const FALLBACK_NAME: &str = "video";
/// Cookies files of a lookup: `lookup-<random>.txt` in the data folder.
const LOOKUP_FILE_PREFIX: &str = "lookup-";
const LOOKUP_FILE_SUFFIX: &str = ".txt";

/// How a lookup ended.
#[derive(Debug)]
pub(super) enum LookupOutcome {
    /// The page has qualities to choose from.
    Choices(Box<PageInfo>, VideoChoices),
    /// The page has nothing to download, or yt-dlp failed; `text` is for the user.
    Message { text: String, transient: bool },
    /// yt-dlp (or deno) could not be had; the tools manager's own words.
    NoTools(ToolError),
    /// The download was stopped meanwhile.
    Cancelled,
}

/// Lets an extractor error ask for a newer yt-dlp at most once an hour, for
/// the whole daemon.
#[derive(Default)]
pub(super) struct ExtractorUpdateGate {
    last: Mutex<Option<Instant>>,
}

impl ExtractorUpdateGate {
    /// True, and counts as the check made, unless one was allowed within the hour.
    pub(super) fn may_update(&self, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let allowed =
            last.is_none_or(|at| now.saturating_duration_since(at) >= EXTRACTOR_UPDATE_EVERY);
        if allowed {
            *last = Some(now);
        }
        allowed
    }
}

/// What a lookup needs of its download, copied out so no lock is held while yt-dlp runs.
struct Target {
    page_url: Url,
    has_cookies: bool,
    user_agent: Option<String>,
    referer: Option<String>,
    cookies: Vec<ContextCookie>,
}

impl DaemonCore {
    /// The engine's link check ended with `outcome`. A web page on a fresh
    /// download is looked up as a video; any other outcome is returned to be
    /// settled as it always was. `None` means the lookup settled the download.
    pub(super) async fn web_page_to_video(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        outcome: Result<Outcome, Error>,
        cancel: watch::Receiver<bool>,
        fresh: bool,
    ) -> Option<Result<Outcome, Error>> {
        if !matches!(outcome, Err(Error::WebPage)) || !fresh || self.tools.is_none() {
            return Some(outcome);
        }
        let id = {
            let rec = record.lock().unwrap();
            let wanted = rec.status == DownloadStatus::Active && rec.video.is_none();
            if !wanted {
                return Some(outcome);
            }
            rec.id.clone()
        };
        let cookies = self.cookies_for(&id);
        let found = self.look_up_video(record, cookies, cancel).await;
        self.finish_lookup(record, found);
        None
    }

    /// A download that starts with a plan already (back from a cancel, or a
    /// restart) does not need the engine: it waits for its choice again, or its
    /// page is looked up once more when no choices were kept. Returns true when
    /// that settled the download.
    pub(super) async fn continue_video_plan(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        cancel: watch::Receiver<bool>,
    ) -> bool {
        enum Next {
            Fresh,
            Stopped,
            Choose,
            Placeholder,
            LookUp(String),
        }
        let next = {
            let rec = record.lock().unwrap();
            match &rec.video {
                None if !rec.lookup_next => Next::Fresh,
                _ if rec.status != DownloadStatus::Active => Next::Stopped,
                Some(plan) if plan.choice.is_some() => Next::Placeholder,
                Some(plan) if !plan.choices.is_empty() => Next::Choose,
                // The last run failed in the lookup, or no choices were kept.
                _ => Next::LookUp(rec.id.clone()),
            }
        };
        match next {
            Next::Fresh => return false,
            Next::Stopped => self.finish_task(record, Err(Error::Cancelled)),
            Next::Choose | Next::Placeholder => {
                let kept = {
                    let mut rec = record.lock().unwrap();
                    // Stopped since the check above: settle like any stop.
                    if rec.status != DownloadStatus::Active {
                        false
                    } else {
                        settle_run(&mut rec);
                        rec.error_message = None;
                        if matches!(next, Next::Placeholder) {
                            fail_with_placeholder(&mut rec);
                        } else {
                            rec.status = DownloadStatus::Choosing;
                        }
                        true
                    }
                };
                if !kept {
                    self.finish_task(record, Err(Error::Cancelled));
                    return true;
                }
                self.persist();
                self.schedule();
            }
            Next::LookUp(id) => {
                let cookies = self.cookies_for(&id);
                let found = self.look_up_video(record, cookies, cancel).await;
                self.finish_lookup(record, found);
            }
        }
        true
    }

    /// Look the download's page up with yt-dlp, getting yt-dlp and deno first
    /// if need be. The row's `step` says so while it runs.
    pub(super) async fn look_up_video(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        client_cookies: Vec<ContextCookie>,
        cancel: watch::Receiver<bool>,
    ) -> LookupOutcome {
        let Some(tools) = self.tools.clone() else {
            let missing = ToolError::Unusable("This daemon has no tools folder.".to_string());
            return LookupOutcome::NoTools(missing);
        };
        let target = match Target::of(record, client_cookies, self.data_dir()) {
            Ok(target) => target,
            Err(text) => {
                return LookupOutcome::Message {
                    text,
                    transient: false,
                }
            }
        };
        record.lock().unwrap().step = Some(LOOKING_STEP.to_string());

        let fetch = |tool, fresh| self.fetch_tool(record, &tools, tool, fresh, &cancel);
        let mut ytdlp = match fetch(Tool::YtDlp, Freshness::CheckIfStale).await {
            Ok(installed) => installed,
            Err(e) => return tool_outcome(e),
        };
        let deno = match fetch(Tool::Deno, Freshness::AsInstalled).await {
            Ok(installed) => installed,
            Err(e) => return tool_outcome(e),
        };

        let dir = self.data_dir().unwrap_or(Path::new(""));
        let run = |ytdlp: Installed| run_lookup(ytdlp, &deno, &target, dir, cancel.clone());
        let mut result = run(ytdlp.clone()).await;
        if let Err(LookupFailure::Exited { stderr, .. }) = &result {
            let wants_update = is_extractor_error(stderr)
                && !*cancel.borrow()
                && self.extractor_gate.may_update(Instant::now());
            if wants_update {
                match fetch(Tool::YtDlp, Freshness::CheckNow).await {
                    Ok(newer) if newer.version != ytdlp.version => {
                        ytdlp = newer;
                        result = run(ytdlp.clone()).await;
                    }
                    Err(ToolError::Cancelled) => return LookupOutcome::Cancelled,
                    // No newer yt-dlp, or none could be had: the first failure stands.
                    _ => {}
                }
            }
        }
        if *cancel.borrow() || matches!(&result, Err(LookupFailure::Spawn(m)) if m == CANCELLED) {
            return LookupOutcome::Cancelled;
        }
        match result {
            Ok(info) => outcome_of_page(info, &target.page_url),
            Err(failure) => {
                let site = site_name(&PageInfo::default(), &target.page_url);
                let (text, transient) =
                    failure_message(&failure, &site, target.has_cookies, &ytdlp.version);
                LookupOutcome::Message { text, transient }
            }
        }
    }

    /// Make sure `tool` is installed, showing its progress on the row while
    /// it is being fetched.
    async fn fetch_tool(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        tools: &crate::tools::ToolStore,
        tool: Tool,
        fresh: Freshness,
        cancel: &watch::Receiver<bool>,
    ) -> Result<Installed, ToolError> {
        let progress = {
            let mut rec = record.lock().unwrap();
            let progress = Arc::new(Progress::with_rate_limiters(
                Arc::clone(&rec.share_limiter),
                Arc::clone(&rec.speed_limiter),
            ));
            rec.lookup_progress = Some((tool, Arc::clone(&progress)));
            progress
        };
        let ctx = FetchCtx {
            progress,
            cancel: Some(cancel.clone()),
        };
        let result = tools.ensure(tool, fresh, ctx).await;
        {
            let mut rec = record.lock().unwrap();
            rec.lookup_progress = None;
            rec.step = Some(LOOKING_STEP.to_string());
        }
        result
    }

    /// The download's browser cookies, from the secret store. Empty when it has
    /// none, or when this daemon keeps none (no data folder): the lookup then
    /// runs without them.
    pub(super) fn cookies_for(&self, id: &str) -> Vec<ContextCookie> {
        let has_cookies = {
            let state = self.state.lock().unwrap();
            let task = state.downloads.get(id);
            task.is_some_and(|t| t.lock().unwrap().has_cookies)
        };
        if !has_cookies {
            return Vec::new();
        }
        let loaded = match &self.secrets {
            Some(secrets) => secrets.load(id).map_err(|e| e.kind()),
            None => Err("no secret store"),
        };
        match loaded {
            Ok(cookies) => cookies,
            Err(kind) => {
                tracing::info!("{id}: looking the page up without its sign-in ({kind})");
                Vec::new()
            }
        }
    }

    /// Settle the download after a lookup and save it.
    pub(super) fn finish_lookup(&self, record: &Arc<Mutex<TaskRecord>>, outcome: LookupOutcome) {
        let end_lookup = |failed: bool| {
            let mut rec = record.lock().unwrap();
            rec.step = None;
            rec.lookup_progress = None;
            rec.lookup_next |= failed;
            rec.has_cookies
        };
        match outcome {
            LookupOutcome::Choices(info, choices) => self.store_choices(record, &info, &choices),
            LookupOutcome::Message { text, transient } => {
                let has_cookies = end_lookup(true);
                // A page with no video where cookies were sent: the sign-in
                // wording, and the cookies go, as for any web page.
                let error = if has_cookies && text == NO_VIDEO {
                    Error::WebPage
                } else {
                    Error::Video {
                        message: text,
                        transient,
                    }
                };
                self.finish_task(record, Err(error));
            }
            LookupOutcome::NoTools(e) => {
                end_lookup(true);
                let error = Error::Tool {
                    message: e.to_string(),
                    transient: e.is_transient(),
                };
                self.finish_task(record, Err(error));
            }
            LookupOutcome::Cancelled => {
                end_lookup(false);
                self.finish_task(record, Err(Error::Cancelled));
            }
        }
    }

    /// Keep the choices: the download waits for the user, or, when a quality
    /// was asked for at add time, chooses at once.
    fn store_choices(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        info: &PageInfo,
        choices: &VideoChoices,
    ) {
        let kept = {
            let mut rec = record.lock().unwrap();
            rec.step = None;
            rec.lookup_progress = None;
            // Stopped meanwhile: settle like any stop, whatever was found.
            if rec.status != DownloadStatus::Active {
                false
            } else {
                settle_run(&mut rec);
                rec.error_message = None;
                tracing::info!(
                    "{}: {} qualities found on {}",
                    rec.id,
                    choices.choices.len(),
                    info.extractor_key.as_deref().unwrap_or("a web page")
                );
                rec.video = Some(plan_of(&rec.url, choices));
                rec.lookup_next = false;
                match rec.quality {
                    Some(quality) => apply_choice(&mut rec, pick(choices, quality)),
                    None => {
                        rec.status = DownloadStatus::Choosing;
                        let default = choices.choices.get(choices.default);
                        let container = default.map_or(Container::Mp4, |c| c.container);
                        rec.filename = video_file_name(
                            rec.requested_name.as_deref(),
                            &choices.title,
                            container,
                        );
                    }
                }
                true
            }
        };
        if !kept {
            self.finish_task(record, Err(Error::Cancelled));
            return;
        }
        self.persist();
        self.schedule();
    }
}

impl Target {
    fn of(
        record: &Arc<Mutex<TaskRecord>>,
        client_cookies: Vec<ContextCookie>,
        data_dir: Option<&Path>,
    ) -> Result<Target, String> {
        let rec = record.lock().unwrap();
        let page_url = Url::parse(&rec.url).map_err(|e| format!("The link is not usable: {e}"))?;
        // Cookies reach yt-dlp through a file in the data folder; without one
        // they are left out.
        let cookies = if data_dir.is_some() {
            client_cookies
        } else {
            tracing::info!(
                "{}: {} cookies not passed to yt-dlp (no data folder)",
                rec.id,
                client_cookies.len()
            );
            Vec::new()
        };
        Ok(Target {
            page_url,
            has_cookies: rec.has_cookies,
            user_agent: rec.user_agent.clone(),
            referer: rec.referer.clone(),
            cookies,
        })
    }
}

/// A tool that could not be had ends the lookup: stopped, or the tools manager's words.
fn tool_outcome(error: ToolError) -> LookupOutcome {
    match error {
        ToolError::Cancelled => LookupOutcome::Cancelled,
        other => LookupOutcome::NoTools(other),
    }
}

async fn run_lookup(
    ytdlp: Installed,
    deno: &Installed,
    target: &Target,
    cookies_dir: &Path,
    cancel: watch::Receiver<bool>,
) -> Result<PageInfo, LookupFailure> {
    look_up(LookupRequest {
        ytdlp: &ytdlp,
        deno,
        page_url: &target.page_url,
        cookies: &target.cookies,
        user_agent: target.user_agent.as_deref(),
        referer: target.referer.as_deref(),
        cookies_dir,
        cancel: Some(cancel),
    })
    .await
}

/// The user's answer, or the failure, for a page yt-dlp read.
fn outcome_of_page(info: PageInfo, page_url: &Url) -> LookupOutcome {
    let built = build_choices(&info);
    let site = site_name(&info, page_url);
    match (
        page_message(&info, built.as_ref().err().copied(), &site),
        built,
    ) {
        (Some(text), _) => LookupOutcome::Message {
            text,
            transient: false,
        },
        (None, Ok(choices)) => LookupOutcome::Choices(Box::new(info), choices),
        (None, Err(_)) => LookupOutcome::Message {
            text: NO_VIDEO.to_string(),
            transient: false,
        },
    }
}

fn plan_of(page_url: &str, choices: &VideoChoices) -> VideoPlan {
    VideoPlan {
        page_url: page_url.to_string(),
        title: choices.title.clone(),
        choice: None,
        choices: choices.choices.clone(),
        default: choices.default,
        streams: Vec::new(),
        step: VideoStep::Choosing,
        refreshes: Vec::new(),
    }
}

/// The run that was started has ended, though no engine report will say so.
fn settle_run(rec: &mut TaskRecord) {
    rec.speed_bps = 0;
    rec.eta_secs = None;
    rec.cancel_tx = None;
    rec.running = false;
}

/// The file a video is saved as: the name the user asked for, else its title,
/// made safe, and the quality's ending. A requested name that already ends
/// with that ending (in any letter case) is not given it twice.
fn video_file_name(requested: Option<&str>, title: &str, container: Container) -> String {
    let usable =
        |text: &str| Some(crate::template::sanitize_component(text)).filter(|n| !n.is_empty());
    let ending = format!(".{}", container.ext());
    if let Some(name) = requested.and_then(usable) {
        let has_ending = name.len() > ending.len()
            && name
                .get(name.len() - ending.len()..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(&ending));
        return if has_ending { name } else { name + &ending };
    }
    let name = usable(title).unwrap_or_else(|| FALLBACK_NAME.to_string());
    name + &ending
}

fn fail_with_placeholder(rec: &mut TaskRecord) {
    rec.status = DownloadStatus::Failed;
    rec.retry_at = None;
    rec.error_message = Some(VIDEO_PLACEHOLDER.to_string());
}

/// Record that row `index` of the stored choices was chosen. Until video
/// downloading arrives the download then fails with a plain message.
pub(super) fn apply_choice(rec: &mut TaskRecord, index: usize) {
    let Some(plan) = rec.video.as_mut() else {
        return;
    };
    let Some(row) = plan.choices.get(index).cloned() else {
        // A row that is not there leaves nothing to download.
        rec.status = DownloadStatus::Failed;
        rec.retry_at = None;
        rec.error_message = Some(NO_VIDEO.to_string());
        return;
    };
    plan.choice = Some(ChosenQuality {
        label: row.label,
        container: row.container,
        choice_id: row.id,
    });
    plan.step = VideoStep::Stream(0);
    plan.choices = Vec::new();
    let title = plan.title.clone();
    rec.filename = video_file_name(rec.requested_name.as_deref(), &title, row.container);
    fail_with_placeholder(rec);
}

/// Show the tool being fetched for the lookup as the row's step. While nothing
/// has arrived yet (checking for a newer one) the step stays as it is.
pub(super) fn refresh_step(task: &mut TaskRecord) {
    let Some((tool, progress)) = &task.lookup_progress else {
        return;
    };
    let downloaded = progress
        .downloaded
        .load(std::sync::atomic::Ordering::Relaxed);
    let total = progress.total.load(std::sync::atomic::Ordering::Relaxed);
    if downloaded == 0 && total == 0 {
        return;
    }
    let text = getting_text(tool.name(), downloaded, (total > 0).then_some(total));
    task.step = Some(text);
}

/// Whether the download is waiting for yt-dlp (or for a tool it needs), so
/// that silence from the engine is not a stall.
pub(super) fn is_looking_up(task: &TaskRecord) -> bool {
    task.lookup_progress.is_some() || task.step.as_deref() == Some(LOOKING_STEP)
}

/// Remove cookies files that an earlier run left in `dir`. Best effort.
pub(super) fn sweep_lookup_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let removed = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(LOOKUP_FILE_PREFIX)
                && name.ends_with(LOOKUP_FILE_SUFFIX)
                && entry.file_type().is_ok_and(|t| t.is_file())
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count();
    if removed > 0 {
        tracing::info!("removed {removed} cookies file(s) left by an earlier lookup");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{VideoChoice, VideoStep};
    use std::sync::atomic::Ordering;

    fn row(id: &str, label: &str, container: Container) -> VideoChoice {
        VideoChoice {
            id: id.to_string(),
            label: label.to_string(),
            container,
            height: None,
            size_bytes: None,
            size_is_estimate: false,
            streams: Vec::new(),
        }
    }

    fn record_with_plan() -> TaskRecord {
        let mut rec = TaskRecord::new(
            "dl_1".to_string(),
            "https://video.test/watch?v=1".to_string(),
            std::path::PathBuf::from("/downloads"),
            None,
            4,
        );
        let choices = VideoChoices {
            title: "A: title/with*marks".to_string(),
            duration_secs: None,
            choices: vec![
                row("1080-mp4", "1080p", Container::Mp4),
                row("audio-m4a", "Audio only", Container::M4a),
            ],
            default: 0,
        };
        rec.video = Some(plan_of(&rec.url, &choices));
        rec
    }

    #[test]
    fn the_gate_lets_one_update_through_an_hour() {
        let gate = ExtractorUpdateGate::default();
        let start = Instant::now();

        assert!(gate.may_update(start));
        assert!(!gate.may_update(start + Duration::from_secs(60)));
        assert!(!gate.may_update(start + EXTRACTOR_UPDATE_EVERY - Duration::from_secs(1)));
        assert!(gate.may_update(start + EXTRACTOR_UPDATE_EVERY));
        assert!(!gate.may_update(start + EXTRACTOR_UPDATE_EVERY + Duration::from_secs(1)));
    }

    #[test]
    fn choosing_a_row_keeps_it_empties_the_list_and_names_the_file() {
        let mut rec = record_with_plan();

        apply_choice(&mut rec, 1);

        let plan = rec.video.as_ref().unwrap();
        let chosen = plan.choice.as_ref().unwrap();
        assert_eq!(chosen.label, "Audio only");
        assert_eq!(chosen.container, Container::M4a);
        assert_eq!(chosen.choice_id, "audio-m4a");
        assert!(plan.choices.is_empty());
        assert_eq!(plan.step, VideoStep::Stream(0));
        assert_eq!(rec.filename, "A_ title_with_marks.m4a");
        assert_eq!(rec.status, DownloadStatus::Failed);
        assert_eq!(rec.error_message.as_deref(), Some(VIDEO_PLACEHOLDER));
    }

    #[test]
    fn a_row_that_is_not_there_fails_the_download_with_the_no_video_message() {
        let mut rec = record_with_plan();

        apply_choice(&mut rec, 9);

        assert_eq!(rec.video.as_ref().unwrap().choices.len(), 2);
        assert_eq!(rec.status, DownloadStatus::Failed);
        assert_eq!(rec.error_message.as_deref(), Some(NO_VIDEO));
    }

    #[test]
    fn a_title_with_nothing_usable_still_gives_a_name() {
        assert_eq!(video_file_name(None, "...", Container::Webm), "video.webm");
        assert_eq!(video_file_name(None, "Rust", Container::Mp4), "Rust.mp4");
    }

    #[test]
    fn a_name_the_user_asked_for_beats_the_title() {
        assert_eq!(
            video_file_name(Some("My: clip"), "Rust", Container::Mp4),
            "My_ clip.mp4"
        );
        assert_eq!(
            video_file_name(Some("..."), "Rust", Container::M4a),
            "Rust.m4a"
        );
    }

    #[test]
    fn a_requested_name_keeps_its_own_ending_only_when_it_is_the_right_one() {
        let name = |requested: &str, container| video_file_name(Some(requested), "T", container);

        assert_eq!(name("clip.mp4", Container::Mp4), "clip.mp4");
        assert_eq!(name("Clip.MP4", Container::Mp4), "Clip.MP4");
        assert_eq!(name("clip.mkv", Container::Mp4), "clip.mkv.mp4");
        assert_eq!(name("clip.mp4", Container::Webm), "clip.mp4.webm");
        assert_eq!(name("mp4", Container::Mp4), "mp4.mp4");
        // Leading dots are trimmed when the name is made safe.
        assert_eq!(name(".mp4", Container::Mp4), "mp4.mp4");
    }

    #[test]
    fn choosing_a_row_names_the_file_after_the_requested_name() {
        let mut rec = record_with_plan();
        rec.requested_name = Some("My clip".to_string());

        apply_choice(&mut rec, 0);

        assert_eq!(rec.filename, "My clip.mp4");
    }

    #[test]
    fn a_tool_being_fetched_is_shown_on_the_row() {
        let mut rec = record_with_plan();
        rec.step = Some(LOOKING_STEP.to_string());
        let progress = Arc::new(Progress::new());
        rec.lookup_progress = Some((Tool::YtDlp, Arc::clone(&progress)));

        refresh_step(&mut rec);
        assert_eq!(
            rec.step.as_deref(),
            Some(LOOKING_STEP),
            "nothing arrived yet"
        );

        progress.downloaded.store(12_000_000, Ordering::Relaxed);
        refresh_step(&mut rec);
        assert_eq!(rec.step.as_deref(), Some("Getting yt-dlp: 12 MB"));

        progress.total.store(24_000_000, Ordering::Relaxed);
        refresh_step(&mut rec);
        assert_eq!(rec.step.as_deref(), Some("Getting yt-dlp: 12/24 MB"));
        assert!(is_looking_up(&rec));
    }

    #[test]
    fn only_lookup_cookies_files_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "lookup-1.txt",
            "lookup-2.txt",
            "lookup-3.json",
            "queue.json",
        ] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        std::fs::create_dir(dir.path().join("lookup-dir.txt")).unwrap();

        sweep_lookup_files(dir.path());

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["lookup-3.json", "lookup-dir.txt", "queue.json"]);
        sweep_lookup_files(&dir.path().join("missing"));
    }
}
