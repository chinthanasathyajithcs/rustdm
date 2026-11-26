//! Running one stream of a video in pieces mode: its pieces are read (the HLS
//! playlist again on every run, or the DASH list kept in the plan) and fetched
//! into the stream file (spec 1c, sections 5, 7).

use super::core::DaemonCore;
use super::task::TaskRecord;
use super::video_join::NameClaim;
use super::video_run::{no_video, stream_job, StreamJob};
use crate::cancel::or_cancel;
use crate::engine::{Outcome, Timeouts};
use crate::error::{is_link_status, Error};
use crate::ipc::protocol::{PieceLane, PieceMap};
use crate::pieces::{
    download, kept_bytes, leftovers, Piece, PieceCounter, PiecesRequest, UNREADABLE_PLAYLIST,
};
use crate::request_context::{stream_client, RequestContext};
use crate::video::errors::{DRM, LIVE};
use crate::video::hls::{parse_playlist, HlsError, Playlist, MAX_PLAYLIST_BYTES};
use crate::video::StreamSource;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use url::Url;

impl DaemonCore {
    /// Fetch stream `index`, whose source is HLS or DASH, into its stream file.
    pub(super) async fn run_pieces_stream(
        &self,
        record: &Arc<Mutex<TaskRecord>>,
        index: usize,
        cancel: watch::Receiver<bool>,
    ) -> Result<Outcome, Error> {
        let job = stream_job(record, index)?;
        let client = self.pieces_client(&job)?;
        // The stream file is named once and kept in the plan, so a restart
        // resumes the same file; the claim keeps a parallel download off it.
        let (file, _claim) = match job.stream.file.clone() {
            Some(file) => (file, None),
            None => {
                let claim = self.claim_stream_file(&job.directory.join(&job.file_name));
                let file = claim.path.clone();
                set_stream_file(record, index, &file);
                self.persist();
                (file, Some(claim))
            }
        };
        if is_finished(&file) {
            return Ok(Outcome::Completed {
                path: file,
                steals: 0,
            });
        }
        // The row keeps what earlier runs kept while the playlist is read
        // again, so neither the size nor the speed jumps.
        job.progress
            .downloaded
            .store(kept_bytes(&file), Ordering::Relaxed);
        let pieces = pieces_of(&client, &job.stream.source, &cancel).await?;
        let counter = Arc::new(PieceCounter::default());
        record.lock().unwrap().pieces_counter = Some(Arc::clone(&counter));
        let request = PiecesRequest {
            client,
            pieces,
            file,
            connections: job.connections,
            progress: job.progress,
            counter,
            cancel,
        };
        let result = download(request).await;
        record.lock().unwrap().pieces_counter = None;
        result.map(|path| Outcome::Completed { path, steals: 0 })
    }

    /// The client for the stream's playlist, keys and pieces: the download's
    /// sign-in, Referer and User-Agent, and the stream's allowed headers.
    fn pieces_client(&self, job: &StreamJob) -> Result<reqwest::Client, Error> {
        let ctx = RequestContext {
            cookies: self.cookies_for(&job.id),
            referer: job.referer.clone(),
            user_agent: job.user_agent.clone(),
        };
        let ctx = Some(&ctx).filter(|ctx| !ctx.is_empty());
        stream_client(ctx, &job.stream.headers, Timeouts::default())
    }

    /// Claim the first stream file name from `wanted` on whose files are all
    /// absent and which no other running download holds.
    fn claim_stream_file(&self, wanted: &Path) -> NameClaim {
        let mut claimed = self.claimed_names.lock().unwrap();
        let free = |path: &PathBuf| {
            !claimed.contains(path) && !path.exists() && leftovers(path).iter().all(|p| !p.exists())
        };
        let path = (1..)
            .map(|n| match n {
                1 => wanted.to_path_buf(),
                n => numbered(wanted, n - 1),
            })
            .find(free)
            .expect("some numbered name is free");
        claimed.insert(path.clone());
        // An empty `.part` holds the name on disk too, beyond this run: a
        // pause or a restart before the first piece must not free it.
        let _ = std::fs::write(crate::state::part_path(&path), b"");
        NameClaim::new(path, Arc::clone(&self.claimed_names))
    }
}

/// `A.f1.mp4` → `A.f1 (n).mp4`.
fn numbered(path: &Path, n: u32) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(crate::template::collision_name(&name, n))
}

/// A stream file whose pieces were all written before the plan could say so.
fn is_finished(file: &Path) -> bool {
    file.is_file() && leftovers(file).iter().all(|p| !p.exists())
}

fn set_stream_file(record: &Arc<Mutex<TaskRecord>>, index: usize, file: &Path) {
    let mut rec = record.lock().unwrap();
    let stream = rec
        .video
        .as_mut()
        .and_then(|plan| plan.streams.get_mut(index));
    if let Some(stream) = stream {
        stream.file = Some(file.to_path_buf());
    }
}

/// The stream's pieces: its HLS media playlist read now, or its DASH links.
async fn pieces_of(
    client: &reqwest::Client,
    source: &StreamSource,
    cancel: &watch::Receiver<bool>,
) -> Result<Vec<Piece>, Error> {
    match source {
        StreamSource::Hls { playlist } => {
            let url = Url::parse(playlist).map_err(|_| unreadable())?;
            let (text, at) = fetch_playlist(client, &url, cancel).await?;
            match parse_playlist(&text, &at) {
                Ok(Playlist::Media(media)) if media.ended => Ok(media.pieces),
                Ok(Playlist::Media(_)) => Err(video_error(LIVE)),
                Err(HlsError::Protected) => Err(video_error(DRM)),
                Ok(Playlist::Master(_)) | Err(_) => Err(unreadable()),
            }
        }
        StreamSource::Dash { pieces } => pieces
            .iter()
            .map(|piece| {
                let url = Url::parse(&piece.link).map_err(|_| unreadable())?;
                Ok(Piece {
                    url,
                    range: None,
                    duration_ms: piece.duration_ms,
                    key: None,
                })
            })
            .collect(),
        StreamSource::Direct => Err(no_video()),
    }
}

/// The text of the playlist at `url`, at most `MAX_PLAYLIST_BYTES`, and the
/// address it came from after redirects.
pub(super) async fn fetch_playlist(
    client: &reqwest::Client,
    url: &Url,
    cancel: &watch::Receiver<bool>,
) -> Result<(String, Url), Error> {
    let mut response = or_cancel(client.get(url.clone()).send(), Some(cancel)).await??;
    let status = response.status().as_u16();
    if is_link_status(status) {
        return Err(Error::LinkExpired { status });
    }
    if !response.status().is_success() {
        return Err(Error::Protocol(format!(
            "the server answered the video's playlist with status {status}"
        )));
    }
    let at = response.url().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = or_cancel(response.chunk(), Some(cancel)).await?? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_PLAYLIST_BYTES {
            return Err(unreadable());
        }
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), at))
}

fn video_error(message: &str) -> Error {
    Error::Video {
        message: message.to_string(),
        transient: false,
    }
}

pub(super) fn unreadable() -> Error {
    video_error(UNREADABLE_PLAYLIST)
}

/// The details pane's view of a pieces stream.
pub(super) fn piece_map(counter: &PieceCounter) -> PieceMap {
    let activity = counter.activity();
    PieceMap {
        total: counter.total.load(Ordering::Relaxed),
        written: activity.written as u64,
        waiting: activity.waiting.iter().map(|&i| i as u64).collect(),
        running: activity
            .running
            .iter()
            .map(|(&index, piece)| PieceLane {
                index: index as u64,
                downloaded_bytes: piece.got,
                total_bytes: piece.len,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pieces::{PieceActivity, RunningPiece};

    #[test]
    fn the_piece_map_copies_where_the_pieces_are() {
        let counter = PieceCounter::default();
        counter.total.store(10, Ordering::Relaxed);
        counter.set_activity_for_tests(PieceActivity {
            written: 2,
            waiting: [4].into(),
            running: [(
                2,
                RunningPiece {
                    got: 5,
                    len: Some(9),
                },
            )]
            .into(),
        });

        let map = piece_map(&counter);

        assert_eq!(
            map,
            PieceMap {
                total: 10,
                written: 2,
                waiting: vec![4],
                running: vec![PieceLane {
                    index: 2,
                    downloaded_bytes: 5,
                    total_bytes: Some(9),
                }],
            }
        );
    }
}
