//! The stinger's file, looked at once (ADR-0140 decisions 2, 3 and 9): its length,
//! whether its transparency is kept here, and its cut point. The look runs on a thread
//! of its own at the default priority, below every live thread (they run raised,
//! ADR-0030), one at a time; a newer look cancels the older one.
//! The result is a runtime cache, never in the document or the undo stack.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use rbs_scene::model::SourceId;
use rbs_sources::stinger::{LookError, StingerAlpha, StingerLook, look};

use super::{Host, Msg};

/// What a look is for: the file as it is on disk, its play range and the canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LookKey {
    source: SourceId,
    path: PathBuf,
    bytes: u64,
    modified: Option<SystemTime>,
    range: (Duration, Option<Duration>),
    canvas: (u32, u32),
}

/// The host's stinger file state.
#[derive(Default)]
pub(super) struct StingerFile {
    /// The look asked for last; `None` without a stinger source.
    key: Option<LookKey>,
    /// Its result, once it finished.
    found: Option<StingerLook>,
    /// Cancels the running look.
    cancel: Option<Arc<AtomicBool>>,
}

impl StingerFile {
    /// The found cut point in ms, from the start of the play range; `None` until found
    /// or when no picture covers the canvas.
    pub(super) fn cut_found_ms(&self) -> Option<u32> {
        let cut = self.found?.cut?;
        Some(u32::try_from(cut.as_millis()).unwrap_or(u32::MAX))
    }

    /// The file's length as stated, once looked at.
    pub(super) fn duration(&self) -> Option<Duration> {
        self.found?.duration
    }

    /// True when the file has transparency no adapter here keeps.
    pub(super) fn alpha_unsupported(&self) -> bool {
        self.found
            .is_some_and(|f| f.alpha == StingerAlpha::Unsupported)
    }

    /// True while a look runs for the stinger's file.
    pub(super) fn looking(&self) -> bool {
        self.key.is_some() && self.found.is_none() && self.cancel.is_some()
    }

    /// What was found, once found.
    pub(super) fn found(&self) -> Option<StingerLook> {
        self.found
    }

    fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Release);
        }
    }
}

impl Drop for StingerFile {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Host {
    /// Starts a look when the stinger's file, its play range or the canvas changed
    /// since the last one; drops the result when there is no stinger. Called after
    /// every collection change; costs one `metadata` call while a stinger is set.
    pub(super) fn look_at_stinger(&mut self) {
        let key = self.stinger_key();
        if key == self.stinger.key {
            return;
        }
        self.stinger.stop();
        self.stinger.found = None;
        self.stinger.key = key.clone();
        self.push_stinger_facts();
        let Some(key) = key else {
            return;
        };
        let Some(source) = self.collection.source(key.source) else {
            return;
        };
        let Ok((path, range, factories)) = self.adapters.sources.stinger_file(source) else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.stinger.cancel = Some(Arc::clone(&cancel));
        let inbox = self.inbox.clone();
        let canvas = key.canvas;
        let spawned = std::thread::Builder::new()
            .name("rbs-stinger-look".into())
            .spawn(move || {
                let result = look(&factories, &path, range, canvas, &cancel);
                if matches!(result, Err(LookError::Cancelled)) {
                    return;
                }
                if let Some(tx) = inbox.upgrade() {
                    let _ = tx.send(Msg::StingerLook { key, result });
                }
            });
        if spawned.is_err() {
            self.stinger.cancel = None;
        }
    }

    /// A look finished: kept when it is still the one asked for.
    pub(super) fn stinger_looked(
        &mut self,
        key: LookKey,
        result: Result<StingerLook, LookError>,
    ) -> bool {
        if self.stinger.key.as_ref() != Some(&key) {
            return false;
        }
        self.stinger.cancel = None;
        match result {
            Ok(found) => {
                self.stinger.found = Some(found);
                self.push_stinger_facts();
                true
            }
            Err(_) => false,
        }
    }

    /// True when the stinger's file has transparency no adapter here keeps (ADR-0140
    /// decision 9): from the finished look, or else from opening the file once.
    pub(super) fn stinger_alpha_unsupported(&self) -> bool {
        if self.stinger.found().is_some() {
            return self.stinger.alpha_unsupported();
        }
        let Some(id) = self.collection.transition().stinger.source else {
            return false;
        };
        let Some(source) = self.collection.source(id) else {
            return false;
        };
        let Ok((path, _, factories)) = self.adapters.sources.stinger_file(source) else {
            return false;
        };
        rbs_sources::stinger::alpha_here(&factories, &path) == Some(StingerAlpha::Unsupported)
    }

    /// Gives the studio what is known of the stinger's file (ADR-0140 decision 3):
    /// the play range's length, the found cut, the picture size and whether its alpha
    /// is kept here. Unknown until the look finished; a change then runs as a Cut.
    fn push_stinger_facts(&mut self) {
        let length = self.stinger_length();
        let facts = self.stinger.key.as_ref().map(|key| {
            let found = self.stinger.found;
            rbs_record::StingerFacts {
                source: key.source,
                length,
                found_cut: found.and_then(|f| f.cut),
                size: found.and_then(|f| f.size),
                alpha_unsupported: found.is_some_and(|f| f.alpha == StingerAlpha::Unsupported),
            }
        });
        // Only once a look finished: before it, nothing is known.
        let facts = facts.filter(|_| self.stinger.found.is_some());
        self.studio.set_stinger(facts);
    }

    /// The length of the stinger's play range, once its file was looked at: the
    /// range's end (at most the stated duration) less its start.
    pub(super) fn stinger_length(&self) -> Option<Duration> {
        let key = self.stinger.key.as_ref()?;
        let duration = self.stinger.found?.duration;
        let (start, end) = key.range;
        let end = match (end, duration) {
            (Some(end), Some(d)) => Some(end.min(d)),
            (end, d) => end.or(d),
        };
        end.map(|end| end.saturating_sub(start))
    }

    /// The key of the look the collection's stinger needs now.
    fn stinger_key(&self) -> Option<LookKey> {
        let id = self.collection.transition().stinger.source?;
        let source = self.collection.source(id)?;
        let (path, range, _) = self.adapters.sources.stinger_file(source).ok()?;
        let meta = std::fs::metadata(&path).ok()?;
        let size = self.collection.canvas(self.canvas)?.size;
        Some(LookKey {
            source: id,
            path,
            bytes: meta.len(),
            modified: meta.modified().ok(),
            range,
            canvas: (size.width, size.height),
        })
    }
}
