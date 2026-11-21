//! Why each scheduled source is not running: the studio's source states, and the
//! borrowed view a caller that only looks for a change reads (F-10).

use std::borrow::Cow;

use crate::recording::{SourceHealth, SourceState};

use super::{Entry, Studio, Watch, browser_engine_message};

impl Studio {
    /// The bound sources the running plan schedules, in binder order, each with its
    /// adapter, target and why it is not running (if it is not).
    pub fn sources(&self) -> Vec<SourceState> {
        self.entries
            .iter()
            .filter(|e| e.scheduled)
            .map(|e| {
                let health = self.health_of(e);
                SourceState {
                    failed: health.failed.map(Cow::into_owned),
                    fault: health.fault,
                    reason: health.reason,
                    ..e.state.clone()
                }
            })
            .collect()
    }

    /// Why each scheduled source is not running, in binder order: the part of
    /// [`Studio::sources`] that changes while the studio runs, borrowed, so a caller
    /// that only looks for a change allocates nothing (F-10).
    pub fn source_health(&self) -> impl Iterator<Item = SourceHealth<'_>> {
        self.entries
            .iter()
            .filter(|e| e.scheduled)
            .map(|e| self.health_of(e))
    }

    fn health_of<'a>(&'a self, e: &'a Entry) -> SourceHealth<'a> {
        let mut state = SourceHealth {
            source: e.source,
            failed: e.state.failed.as_deref().map(Cow::Borrowed),
            fault: e.state.fault,
            reason: e.state.reason,
        };
        // The node's own counters say why a running instance failed.
        state.fault = match (&state.failed, e.instance.as_ref().map(|i| &i.watch)) {
            (None, _) => None,
            (Some(_), Some(Watch::Video(stats))) => stats.fault().or(state.fault),
            (Some(_), _) => state.fault,
        };
        // A browser source without its engine says so, also after a runner
        // that could not load it exited (add-ons spec "Browser engine
        // consumer").
        if let Some(reason) = self
            .browser_controls
            .get(&e.source)
            .filter(|c| !c.helper_running())
            .and_then(|c| {
                // The tree's own answer first: a failed node's instance is
                // made again without being prepared, so its control is new.
                (state.failed.is_some())
                    .then(|| self.browser_tree.as_ref()?.unavailable())
                    .flatten()
                    .or_else(|| c.engine_reason())
            })
        {
            state.failed = Some(Cow::Borrowed(browser_engine_message(reason)));
            state.fault = None;
            state.reason = Some(reason);
        } else if self
            .browser_controls
            .get(&e.source)
            .is_some_and(|c| c.capped())
        {
            // A shown page took the background page's slot (ADR-0149
            // decision 5).
            state.reason = Some(rbs_sources::BROWSER_CAP);
        } else if self.browser_controls.contains_key(&e.source) {
            // Both page slots are taken (ADR-0109 decision 7), or the page
            // has no address yet.
            let failed = state.failed.as_deref().unwrap_or_default();
            state.reason = [rbs_sources::BROWSER_CAP, rbs_sources::BROWSER_NO_ADDRESS]
                .into_iter()
                .find(|r| failed.contains(r));
        }
        // A text source shows its worker's problem (ADR-0129 decision 7).
        if state.failed.is_none()
            && let Some(p) = self.text_problem(e.source).filter(|p| p.fault().is_some())
        {
            state.failed = Some(Cow::Owned(p.message()));
            state.fault = p.fault();
        }
        if state.failed.is_none()
            && let Some(Watch::Text(stats)) = e.instance.as_ref().map(|i| &i.watch)
            && let Some(fault) = stats.fault()
        {
            state.failed = Some(Cow::Borrowed(
                "The text picture could not be kept on the GPU",
            ));
            state.fault = Some(fault);
        }
        state
    }
}
