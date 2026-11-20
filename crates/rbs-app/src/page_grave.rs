//! Keeps a stopped browser tree's studio textures until no importer can still match
//! their handle values (ADR-0157 decision 13, capture-port spec "Native surface
//! rules"): every frame has let go, and then [`HANDLE_HOLD`] more, because importers
//! cache opened surfaces by handle value and forget an idle one within 8 s. Only then
//! are the handles closed and their values free for the OS to reuse.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a page's handles stay open after its last frame let go.
pub const HANDLE_HOLD: Duration = Duration::from_secs(10);

/// Stopped pages, each with the time it was first seen unreferenced.
#[derive(Debug)]
pub struct Grave<T> {
    pages: Vec<(Arc<T>, Option<Instant>)>,
}

impl<T> Default for Grave<T> {
    fn default() -> Self {
        Self { pages: Vec::new() }
    }
}

impl<T> Grave<T> {
    /// Keeps `page` until [`Grave::sweep`] finds it unreferenced for [`HANDLE_HOLD`].
    pub fn bury(&mut self, page: Arc<T>, now: Instant) {
        self.pages.push((page, None));
        self.sweep(now);
    }

    /// Drops the pages no frame has referenced for [`HANDLE_HOLD`]. A page referenced
    /// again starts its wait over. Control thread; the last release is here.
    pub fn sweep(&mut self, now: Instant) {
        self.pages.retain_mut(|(page, idle)| {
            if Arc::strong_count(page) > 1 {
                *idle = None;
                return true;
            }
            let since = *idle.get_or_insert(now);
            now.saturating_duration_since(since) < HANDLE_HOLD
        });
    }

    /// Pages still kept.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.pages.len()
    }

    /// True when no page is kept.
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_stays_until_its_frames_are_gone_and_the_hold_has_passed() {
        let t0 = Instant::now();
        let mut grave = Grave::default();
        let page = Arc::new(7u32);
        let frame = Arc::clone(&page);
        grave.bury(page, t0);
        grave.sweep(t0 + HANDLE_HOLD * 2);
        assert_eq!(grave.len(), 1, "a frame still holds it");
        drop(frame);
        let released = t0 + HANDLE_HOLD * 3;
        grave.sweep(released);
        grave.sweep(released + HANDLE_HOLD - Duration::from_millis(1));
        assert_eq!(grave.len(), 1, "an importer may still cache its handles");
        grave.sweep(released + HANDLE_HOLD);
        assert!(grave.is_empty());
    }

    #[test]
    fn a_page_referenced_again_starts_its_wait_over() {
        let t0 = Instant::now();
        let mut grave = Grave::default();
        let page = Arc::new(1u32);
        let keep = Arc::clone(&page);
        grave.bury(page, t0);
        drop(keep);
        grave.sweep(t0);
        // Seen idle, then held again (a republish), then idle once more.
        let again = grave.pages[0].0.clone();
        grave.sweep(t0 + Duration::from_secs(5));
        drop(again);
        grave.sweep(t0 + Duration::from_secs(6));
        grave.sweep(t0 + HANDLE_HOLD + Duration::from_secs(1));
        assert_eq!(grave.len(), 1, "the wait started at 6 s");
        grave.sweep(t0 + Duration::from_secs(16));
        assert!(grave.is_empty());
    }
}
