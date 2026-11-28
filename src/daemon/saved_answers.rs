//! Videos looked up for the browser before a click (spec 1f, section 4): kept
//! in memory for ten minutes, so the click's download needs no second lookup.

use crate::video::info::PageInfo;
use crate::video::{PlanOrigin, VideoChoices};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a looked-up video is kept.
pub(super) const SAVED_FOR: Duration = Duration::from_secs(10 * 60);
/// How many looked-up videos are kept at most; the oldest goes first.
pub(super) const MAX_SAVED: usize = 20;

/// What a saved answer is found by: the link as the browser sent it, and
/// whether cookies came with it (a signed-in answer is not a signed-out one).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct AnswerKey {
    pub url: String,
    pub has_cookies: bool,
}

/// A lookup's result, as a download row would have kept it.
#[derive(Clone, Debug)]
pub(super) struct SavedAnswer {
    pub info: PageInfo,
    pub choices: VideoChoices,
    pub origin: PlanOrigin,
}

/// The looked-up videos, oldest first.
#[derive(Default)]
pub(super) struct SavedAnswers {
    entries: Mutex<VecDeque<(AnswerKey, SavedAnswer, Instant)>>,
}

impl SavedAnswers {
    /// Keep an answer; one for the same key is replaced and counts from `now`.
    pub(super) fn put(
        &self,
        key: AnswerKey,
        info: PageInfo,
        choices: VideoChoices,
        origin: PlanOrigin,
        now: Instant,
    ) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(k, _, _)| *k != key);
        entries.push_back((
            key,
            SavedAnswer {
                info,
                choices,
                origin,
            },
            now,
        ));
        while entries.len() > MAX_SAVED {
            entries.pop_front();
        }
    }

    /// The answer for `key` while it is younger than `SAVED_FOR`; older ones go.
    pub(super) fn get(&self, key: &AnswerKey, now: Instant) -> Option<SavedAnswer> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(_, _, at)| now.saturating_duration_since(*at) < SAVED_FOR);
        entries
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, answer, _)| answer.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn key(url: &str, has_cookies: bool) -> AnswerKey {
        AnswerKey {
            url: url.to_string(),
            has_cookies,
        }
    }

    fn choices(title: &str) -> VideoChoices {
        VideoChoices {
            title: title.to_string(),
            duration_secs: None,
            choices: Vec::new(),
            default: 0,
        }
    }

    fn put(saved: &SavedAnswers, k: AnswerKey, title: &str, at: Instant) {
        saved.put(k, PageInfo::default(), choices(title), PlanOrigin::Page, at);
    }

    #[test]
    fn an_answer_is_kept_for_ten_minutes() {
        let saved = SavedAnswers::default();
        let t0 = Instant::now();
        put(&saved, key("https://a/1", false), "one", t0);
        assert!(saved
            .get(&key("https://a/1", false), t0 + Duration::from_secs(599))
            .is_some());
        assert!(saved
            .get(&key("https://a/1", false), t0 + Duration::from_secs(600))
            .is_none());
    }

    #[test]
    fn the_oldest_goes_when_twenty_are_kept() {
        let saved = SavedAnswers::default();
        let t0 = Instant::now();
        for i in 0..=MAX_SAVED {
            put(&saved, key(&format!("https://a/{i}"), false), "x", t0);
        }
        assert!(saved.get(&key("https://a/0", false), t0).is_none());
        assert!(saved
            .get(&key(&format!("https://a/{MAX_SAVED}"), false), t0)
            .is_some());
        assert!(saved.get(&key("https://a/1", false), t0).is_some());
    }

    #[test]
    fn cookies_are_part_of_the_key() {
        let saved = SavedAnswers::default();
        let t0 = Instant::now();
        put(&saved, key("https://a/1", false), "one", t0);
        assert!(saved.get(&key("https://a/1", true), t0).is_none());
    }

    #[test]
    fn a_second_put_replaces_and_renews() {
        let saved = SavedAnswers::default();
        let t0 = Instant::now();
        put(&saved, key("https://a/1", false), "first", t0);
        put(
            &saved,
            key("https://a/1", false),
            "second",
            t0 + Duration::from_secs(500),
        );
        let got = saved.get(&key("https://a/1", false), t0 + Duration::from_secs(900));
        assert_eq!(got.map(|a| a.choices.title), Some("second".to_string()));
    }
}
