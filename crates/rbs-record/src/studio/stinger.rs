//! The Stinger transition on a running studio (ADR-0140 decisions 4 to 8).
//!
//! A Stinger change starts like a Fade: the binder binds the incoming compositor and,
//! through the outgoing entry, the stinger's source (running, on air, linked to the
//! program compositor, heard). Before the apply that makes the source Active the
//! studio arms it ([`MediaControl::arm`](crate::MediaControl::arm)): paused at the
//! start of its play range, not looping. The transition waits until the incoming
//! picture and the stinger's first picture are ready, or [`STINGER_WAIT`]; then it
//! plays the stinger (`play_now`, anchor `A`) and sets every layer and ramp from `A`
//! once: the stinger shows from `A`, the incoming scene replaces the outgoing one at
//! `A + cut` under it, and the transition ends at `A` plus the play range's length.
//!
//! The host gives the studio what it knows of the file with [`Studio::set_stinger`].
//! Nothing of this runs while no Stinger change runs (P13).

use rbs_nodes::MediaPlayState as PlayState;
use rbs_scene::TransitionKind;

use super::*;

/// The longest a Stinger change waits for the incoming picture and the stinger's
/// first picture before it runs as a Cut (ADR-0140 decision 4), on the master clock.
pub const STINGER_WAIT: Duration = Duration::from_secs(1);

/// What the host knows of the stinger's file (ADR-0140 decisions 3 and 4): a runtime
/// cache, never in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StingerFacts {
    /// The stinger's source.
    pub source: SourceId,
    /// The length of its play range, once its duration is known.
    pub length: Option<Duration>,
    /// The cut point found from its alpha, from the start of the play range.
    pub found_cut: Option<Duration>,
    /// The size of its first picture, which it is fitted to the canvas by.
    pub size: Option<(u32, u32)>,
    /// The file has transparency no adapter here keeps: it runs as a Cut.
    pub alpha_unsupported: bool,
}

/// The stinger of the main canvas's transition.
#[derive(Debug, Clone, Copy)]
pub(super) struct StingerRun {
    pub(super) source: SourceId,
    /// The armed record's generation, once armed; its first picture opens the gate.
    pub(super) generation: Option<u32>,
    /// Arming failed: the change runs as a Cut at once.
    pub(super) failed: bool,
    pub(super) size: Option<(u32, u32)>,
}

/// What the gate of a waiting Stinger change says now.
pub(super) enum Gate {
    /// Still waiting.
    Wait,
    /// Play: the anchor `A`.
    Play(Timestamp),
    /// Run as a Cut now.
    Cut,
}

/// What a Stinger change plays: the stinger's source, its cut point, the range's
/// length and its picture size. `None` runs it as a Cut.
pub(super) struct StingerPlan {
    pub(super) source: SourceId,
    pub(super) cut: Duration,
    pub(super) length: Duration,
    pub(super) size: Option<(u32, u32)>,
}

/// Arms the stinger a new change asked for, once its media control exists: called
/// before the on-air membership is written and again after the instances are made,
/// both before the apply (ADR-0140 decision 4). Takes the studio's fields so the
/// caller may hold the collection.
pub(super) fn arm(
    pending: &mut Option<SourceId>,
    armed_now: &mut Option<SourceId>,
    controls: &BTreeMap<SourceId, crate::MediaControl>,
    transitions: &mut BTreeMap<CanvasId, transition::CanvasTransition>,
    main: CanvasId,
) {
    let Some(source) = *pending else {
        return;
    };
    let Some(control) = controls.get(&source) else {
        return;
    };
    *pending = None;
    *armed_now = Some(source);
    let armed = control.arm();
    if let Some(run) = transitions
        .get_mut(&main)
        .and_then(|t| t.stinger.as_mut())
        .filter(|r| r.source == source)
    {
        match armed {
            Ok(generation) => run.generation = Some(generation),
            Err(_) => run.failed = true,
        }
    }
}

impl Studio {
    /// What the host knows of the stinger's file, or `None` without a stinger
    /// (ADR-0140 decision 3). Read by the next Stinger change.
    pub fn set_stinger(&mut self, facts: Option<StingerFacts>) {
        self.stinger_facts = facts;
    }

    /// True while `source` is armed as the stinger: play, pause and seek on it are
    /// refused (ADR-0140 decision 5).
    pub fn stinger_armed(&self, source: SourceId) -> bool {
        self.stinger_armed == Some(source)
    }

    /// Stinger changes that ran as a Cut because the stinger was not ready in time.
    pub fn stinger_late_cuts(&self) -> u64 {
        self.stinger_late_cuts
    }

    /// What a Stinger change on the main canvas plays now, or `None` to run it as a
    /// Cut: no source, a source placed in a scene, a file whose alpha is not kept
    /// here or whose length is unknown, or a source in back-off.
    pub(super) fn stinger_plan(
        &self,
        collection: &SceneCollection,
        effective: &transition::Effective,
    ) -> Option<StingerPlan> {
        if effective.kind() != TransitionKind::Stinger {
            return None;
        }
        let settings = effective.transition.stinger;
        let source = settings.source?;
        collection.source(source)?;
        if collection.placed_stinger().is_some() {
            return None;
        }
        let facts = self.stinger_facts.filter(|f| f.source == source)?;
        if facts.alpha_unsupported {
            return None;
        }
        let length = facts.length.filter(|l| !l.is_zero())?;
        if self
            .media_controls
            .get(&source)
            .is_some_and(|c| c.state() == PlayState::Unavailable)
        {
            return None;
        }
        let cut = settings
            .cut_ms
            .map(|ms| Duration::from_millis(u64::from(ms)))
            .or(facts.found_cut)
            .unwrap_or(length / 2)
            .min(length);
        Some(StingerPlan {
            source,
            cut,
            length,
            size: facts.size,
        })
    }

    /// Gives the armed source its own keys back once no Stinger change plays it.
    pub(super) fn disarm_stale_stinger(&mut self) {
        let Some(source) = self.stinger_armed else {
            return;
        };
        if self.stinger_arm == Some(source) || self.transitions.values().any(|t| t.plays(source)) {
            return;
        }
        self.stinger_armed = None;
        if let Some(control) = self.media_controls.get(&source) {
            control.disarm();
        }
    }

    /// The gate of the main canvas's waiting Stinger change (ADR-0140 decisions 4, 5
    /// and 7): every waiting Stinger canvas's incoming picture and the stinger's
    /// first picture, within [`STINGER_WAIT`]. `None` when none waits.
    pub(super) fn stinger_gate(&mut self, now: Timestamp) -> Option<Gate> {
        let main = self.transitions.get(&self.settings.canvas)?;
        let (since, run) = main.stinger_waiting()?;
        let control = self.media_controls.get(&run.source).cloned();
        let unavailable = control
            .as_ref()
            .is_some_and(|c| c.state() == PlayState::Unavailable);
        if run.failed || unavailable {
            return Some(Gate::Cut);
        }
        let pictures = self
            .transitions
            .values()
            .filter(|t| t.stinger_waiting_any())
            .all(|t| t.incoming_ready());
        let stinger = match (run.generation, control.as_ref()) {
            (Some(g), Some(c)) => c.picture_ready(g),
            _ => false,
        };
        if pictures && stinger {
            return Some(match control.map(|c| c.play_now()) {
                Some(Ok(at)) => Gate::Play(at),
                _ => Gate::Cut,
            });
        }
        if now.saturating_since(since) >= STINGER_WAIT {
            self.stinger_late_cuts += 1;
            return Some(Gate::Cut);
        }
        Some(Gate::Wait)
    }

    /// True when the stinger of a running Stinger change stopped: a decode fault or
    /// its end hold (ADR-0140 decisions 4 and 5).
    pub(super) fn stinger_stopped(&self, source: SourceId) -> bool {
        self.media_controls
            .get(&source)
            .is_some_and(|c| matches!(c.state(), PlayState::Unavailable | PlayState::Ended))
    }

    /// A transition whose stinger's source left the collection ends now, before the
    /// binder retires the node (ADR-0140 decision 5).
    pub(super) fn drop_gone_stinger(&mut self, collection: &SceneCollection) {
        let now = self.engine.clock().now();
        for t in self.transitions.values_mut() {
            if let Some(source) = t.stinger_source()
                && collection.source(source).is_none()
            {
                t.end_stinger(now);
            }
        }
    }

    /// The stinger's layer, fitted to the canvas (contain, centred) and shown from
    /// `at`: a step `Fade` from 0, so nothing of it shows before `A`.
    pub(super) fn stinger_layer(
        &self,
        source: SourceId,
        size: Option<(u32, u32)>,
        canvas: (u32, u32),
        at: Timestamp,
    ) -> Option<rbs_gpu::node::NodeLayer> {
        let (node, _) = self.binder.chain_tail(source)?;
        let (cw, ch) = (canvas.0 as f32, canvas.1 as f32);
        let (x, y, w, h) = match size {
            Some((pw, ph)) if pw > 0 && ph > 0 => {
                let (pw, ph) = (pw as f32, ph as f32);
                let scale = (cw / pw).min(ch / ph);
                let (w, h) = (pw * scale, ph * scale);
                ((cw - w) / 2.0, (ch - h) / 2.0, w, h)
            }
            _ => (0.0, 0.0, cw, ch),
        };
        let mut layer = rbs_gpu::node::NodeLayer::new(node, 0, x, y, w, h);
        layer.fade = Some(rbs_gpu::node::Fade {
            from: 0.0,
            start: at,
            duration: Duration::ZERO,
            easing: rbs_gpu::node::Easing::Linear,
        });
        Some(layer)
    }
}
