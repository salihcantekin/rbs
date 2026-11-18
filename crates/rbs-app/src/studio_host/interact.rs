//! Interact windows (ADR-0132 decision 6): one browser page in a window of its own,
//! drawn from the studio's picture of that source, with input sent to the page while
//! the window has focus.
//!
//! While a window is open its page is held at Shown (ADR-0152 decision 2): it runs
//! and takes a preview slot even when no shown scene draws it. Nothing here runs
//! while no window is open (P13).

use rbs_gpu::share::{PreviewDesc, PreviewEndpoint};
use rbs_record::PictureId;
use rbs_scene::model::SourceId;
use rbs_sources::types::{self, keys};

use super::{Host, Update, preview_surfaces};

/// At most this many Interact windows are open at a time: each holds a page slot,
/// and there are two (ADR-0109 decision 7).
pub const MAX_INTERACT: usize = 2;

/// What became of an Interact window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractState {
    /// Open: its picture runs or is being prepared; while the page cannot run the
    /// window shows why (the source's own reason).
    Open,
    /// Closed by the studio, with the reason to tell the user (the source is gone,
    /// or too many are open); empty when there is nothing to tell (another scene
    /// collection). The UI closes the window.
    Closed(String),
}

/// One Interact window as the host keeps it.
pub(super) struct InteractRun {
    source: SourceId,
    /// Its picture in the running studio; `None` until added (or after a restart).
    picture: Option<PictureId>,
    /// The page size the picture was made at.
    size: (u32, u32),
    /// What was last sent to the UI.
    sent: Option<Option<PreviewEndpoint>>,
    /// The source as it was when a picture could not be added (a page in standby):
    /// tried again only once it changed.
    refused: Option<rbs_scene::model::Source>,
}

/// The page size of a browser source's settings.
fn page_size(settings: &rbs_scene::model::Settings) -> (u32, u32) {
    rbs_sources::browser_settings(settings)
        .map_or(rbs_sources::BROWSER_DEFAULT_SIZE, |b| (b.width, b.height))
}

impl Host {
    /// Opens the Interact window of browser source `source`. Refused, as
    /// [`InteractState::Closed`], for a source that is not a browser page and beyond
    /// [`MAX_INTERACT`]. Its picture is added by [`Self::sync_interacts`].
    pub(super) fn open_interact(&mut self, source: SourceId, notify: &dyn Fn(Update)) {
        if self.interacts.iter().any(|r| r.source == source) {
            return;
        }
        let browser = self
            .collection
            .source(source)
            .is_some_and(|s| s.type_id.as_str() == types::BROWSER_SOURCE);
        let why = if !browser {
            Some("That page is gone.")
        } else if self.interacts.len() >= MAX_INTERACT {
            Some("Two pages can be interacted with at a time. Close one first.")
        } else {
            None
        };
        if let Some(why) = why {
            notify(Update::Interact {
                source,
                picture: None,
                state: InteractState::Closed(why.to_owned()),
            });
            return;
        }
        self.interacts.push(InteractRun {
            source,
            picture: None,
            size: (0, 0),
            sent: None,
            refused: None,
        });
        self.apply(notify);
    }

    /// Closes the Interact window of `source`: its picture leaves the studio, and its
    /// page goes back to the scene rules. Unknown sources change nothing.
    pub(super) fn close_interact(&mut self, source: SourceId) {
        let Some(at) = self.interacts.iter().position(|r| r.source == source) else {
            return;
        };
        let run = self.interacts.remove(at);
        if let Some(picture) = run.picture {
            let col = self.run_collection();
            let running = col.as_ref().unwrap_or(&self.collection);
            let _ = self
                .studio
                .remove_picture(running, &self.adapters.sources, picture);
            self.waiting = self.studio.waiting();
        }
    }

    /// Closes every Interact window (another scene collection): the UI closes them
    /// without a message.
    pub(super) fn close_all_interacts(&mut self, notify: &dyn Fn(Update)) {
        let open: Vec<SourceId> = self.interacts.iter().map(|r| r.source).collect();
        for source in open {
            self.close_interact(source);
            notify(Update::Interact {
                source,
                picture: None,
                state: InteractState::Closed(String::new()),
            });
        }
    }

    /// Brings the Interact windows' pictures in line with the studio, like
    /// projectors: a window whose source is gone is closed; one without a picture (new,
    /// after a restart, or after its page size changed) gets one; a picture that
    /// failed is made again at the next change. Tells the UI what changed.
    pub(super) fn sync_interacts(&mut self, notify: &dyn Fn(Update)) {
        if self.interacts.is_empty() {
            return;
        }
        let present = self.studio.pictures();
        let mut closed = Vec::new();
        for at in 0..self.interacts.len() {
            let source = self.interacts[at].source;
            let Some(now) = self
                .collection
                .source(source)
                .filter(|s| s.type_id.as_str() == types::BROWSER_SOURCE)
                .cloned()
            else {
                closed.push(source);
                continue;
            };
            let settings = now.settings.clone();
            let size = page_size(&settings);
            let run = &mut self.interacts[at];
            // Gone with a restart, failed, or made at another page size: made again.
            let stale = run.picture.is_some_and(|pid| {
                !present.contains(&pid)
                    || self.studio.picture_failure(pid).is_some()
                    || run.size != size
            });
            if stale && let Some(pid) = run.picture.take() {
                let col = self.run_collection();
                let running = col.as_ref().unwrap_or(&self.collection);
                let _ = self
                    .studio
                    .remove_picture(running, &self.adapters.sources, pid);
            }
            if self.interacts[at].picture.is_some()
                || self.interacts[at].refused.as_ref() == Some(&now)
            {
                continue;
            }
            let fps = settings
                .get(keys::FPS)
                .and_then(|v| match v {
                    rbs_scene::model::SettingValue::Int(n) => u32::try_from(*n).ok(),
                    _ => None,
                })
                .unwrap_or(rbs_sources::BROWSER_DEFAULT_FPS);
            let desc = PreviewDesc {
                width: size.0,
                height: size.1,
                surfaces: preview_surfaces(
                    rbs_core::timing::FrameRate::new(fps.max(1), 1)
                        .unwrap_or(rbs_core::timing::FrameRate::FPS_30),
                ),
            };
            let col = self.run_collection();
            let running = col.as_ref().unwrap_or(&self.collection);
            // A page that cannot run (no engine, no slot, standby) still gets its
            // window: it shows the reason.
            if let Ok((pid, _)) =
                self.studio
                    .add_source_picture(running, &self.adapters.sources, source, desc)
            {
                self.interacts[at].picture = Some(pid);
                self.interacts[at].size = size;
                self.interacts[at].refused = None;
            } else {
                self.interacts[at].refused = Some(now);
            }
            self.waiting = self.studio.waiting();
        }
        for source in closed {
            self.close_interact(source);
            notify(Update::Interact {
                source,
                picture: None,
                state: InteractState::Closed("That page is gone.".to_owned()),
            });
        }
        for run in &mut self.interacts {
            let now = run.picture.and_then(|pid| self.studio.picture(pid));
            if run.sent.as_ref() != Some(&now) {
                run.sent = Some(now.clone());
                notify(Update::Interact {
                    source: run.source,
                    picture: now,
                    state: InteractState::Open,
                });
            }
        }
    }

    /// Input from an open Interact window to its page (ADR-0132 decision 6): sent
    /// only while the window has focus, which the UI checks. Dropped when the page
    /// does not run. Text is never logged.
    pub(super) fn interact_input(
        &mut self,
        source: SourceId,
        event: rbs_browser_ipc::input::InputEvent,
    ) {
        if self.interacts.iter().any(|r| r.source == source) {
            let _ = self
                .studio
                .browser_control(source, &rbs_sources::BrowserMessage::Input(event));
        }
    }
}
