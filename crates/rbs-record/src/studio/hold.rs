//! Holding a stream on a running studio (ADR-0124 decisions 12 to 14, spec
//! `docs/specs/recording.md`, "Pause and hold").
//!
//! A stream started with [`OutputSettings::holdable`] runs on chains that follow a
//! hold switch: its export and its audio encoder node. [`Studio::hold_output`] turns
//! the switches on and wakes the export, which from its next tick reads nothing
//! back and sends a pinned picture (the last frame, or a black one made once)
//! stamped with each tick, while the audio encoder node sends silence with the
//! blocks' own timestamps. The connection stays up and timestamps keep running.
//! [`Studio::release_hold`] turns them off: the export goes back to the program on
//! its next tick and raises its run-path value again.
//!
//! The hold is on the chain, so every stream sharing it is held with it. A chain
//! shared with an output that cannot be held (a recording) is held only while that
//! output is paused, so the hold picture never reaches its file (decision 14); the
//! host pauses the recording first, and [`Studio::resume_output`] refuses while a
//! hold of its chains is on ([`PauseRefused::Held`](super::PauseRefused::Held)).

use std::fmt;

use rbs_core::pause::HoldPicture;
use rbs_scene::binder::HoldScene;

/// How long a hold scene may take to send its first picture before the hold falls
/// back to the frozen frame (ADR-0124 phase 2b), on the master clock.
pub const HOLD_SCENE_TIMEOUT: Duration = Duration::from_secs(2);

use super::*;

/// Why [`Studio::hold_output`] or [`Studio::release_hold`] did nothing.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldRefused {
    /// No output with this id runs.
    NotRunning,
    /// The output was not started [`OutputSettings::holdable`] (a recording, the
    /// replay buffer), or a chain it uses was not built to hold.
    NotHoldable,
    /// The black picture cannot be made for the export's pixel format.
    PictureUnsupported,
    /// Its encoder is shared with an output that cannot be held and is not paused
    /// (ADR-0124 decision 14): pause that recording first.
    SharedWithRecording,
    /// The output was ended (ADR-0045).
    Ending,
    /// The stream is already on hold with this picture.
    AlreadyHeld,
    /// The stream is not on hold.
    NotHeld,
    /// The hold scene is not in the collection (phase 2b).
    UnknownScene,
    /// The hold scene cannot be shown on the stream's canvas (phase 2b).
    SceneNotShowable,
    /// Another stream of the canvas holds with another scene or sound: one hold scene
    /// per canvas (phase 2b).
    OtherSceneHeld,
    /// [`HoldPicture::Scene`] was asked for without a scene: use
    /// [`Studio::hold_output_scene`].
    NoScene,
    /// The hold scene's parts could not be bound or applied.
    NotBound,
}

impl fmt::Display for HoldRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotRunning => "the output does not run",
            Self::NotHoldable => "the output cannot be held",
            Self::PictureUnsupported => "the hold picture cannot be made for this stream",
            Self::SharedWithRecording => {
                "the stream shares its encoder with a recording that is not paused"
            }
            Self::Ending => "the output is ending",
            Self::AlreadyHeld => "the stream is already on hold",
            Self::NotHeld => "the stream is not on hold",
            Self::UnknownScene => "the hold scene is not in the collection",
            Self::SceneNotShowable => "the hold scene cannot be shown on this canvas",
            Self::OtherSceneHeld => "another stream of this canvas holds with another scene",
            Self::NoScene => "no hold scene was given",
            Self::NotBound => "the hold scene could not be started",
        })
    }
}

impl std::error::Error for HoldRefused {}

/// What a hold scene sends for sound (ADR-0124 phase 2b).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HoldSound {
    /// Silence, as a freeze or black hold.
    #[default]
    Silence,
    /// The scene's own sources, mixed at their levels.
    Scene,
}

/// Why a scene hold fell back to the frozen frame and silence (phase 2b).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldFallback {
    /// No scene picture arrived within [`HOLD_SCENE_TIMEOUT`].
    NoPicture,
    /// The hold scene's compositor failed.
    Failed,
    /// The hold scene left the collection, or can no longer be shown on the canvas.
    SceneGone,
}

impl fmt::Display for HoldFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoPicture => "the hold scene sent no picture in time",
            Self::Failed => "the hold scene failed",
            Self::SceneGone => "the hold scene is gone",
        })
    }
}

/// Where a holdable output's holds stand, on the master clock.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoldState {
    /// The picture sent while on hold; `None` while not held. A scene hold that fell
    /// back says [`HoldPicture::Freeze`].
    pub picture: Option<HoldPicture>,
    /// The hold scene while held with one (phase 2b).
    pub scene: Option<SceneId>,
    /// Why a scene hold fell back to the frozen frame, while it does.
    pub fallback: Option<HoldFallback>,
    /// The hold scene's sound as asked for, while held with a scene.
    pub sound: Option<HoldSound>,
    /// Why the hold scene's sound fell back to silence while its picture runs on.
    pub sound_fallback: Option<HoldFallback>,
    /// When the open hold began.
    pub held_at: Option<Timestamp>,
    /// Time on hold so far, an open hold counted up to now.
    pub held_ns: u64,
}

/// An output's holds, kept on the control thread.
#[derive(Debug, Default)]
pub(super) struct Holds {
    /// Started [`OutputSettings::holdable`].
    holdable: bool,
    /// The open hold: its picture and when it began.
    on: Option<(HoldPicture, Timestamp)>,
    /// Time held by earlier holds.
    before_ns: u64,
}

impl Holds {
    pub(super) fn new(holdable: bool) -> Self {
        Self {
            holdable,
            ..Self::default()
        }
    }

    /// A holdable output that joins chains already on hold with `held` is held from
    /// `now` (ADR-0127 decision 13: a destination started during a hold joins held).
    pub(super) fn joined(holdable: bool, held: Option<HoldPicture>, now: Timestamp) -> Self {
        let mut holds = Self::new(holdable);
        if let (true, Some(picture)) = (holdable, held) {
            holds.start(picture, now);
        }
        holds
    }

    fn state(&self, now: Timestamp) -> HoldState {
        let open = self
            .on
            .map_or(0, |(_, at)| now.as_nanos().saturating_sub(at.as_nanos()));
        HoldState {
            picture: self.on.map(|(p, _)| p),
            scene: None,
            fallback: None,
            sound: None,
            sound_fallback: None,
            held_at: self.on.map(|(_, at)| at),
            held_ns: self.before_ns.saturating_add(open),
        }
    }

    fn start(&mut self, picture: HoldPicture, now: Timestamp) {
        match &mut self.on {
            Some((p, _)) => *p = picture,
            None => self.on = Some((picture, now)),
        }
    }

    fn end(&mut self, now: Timestamp) {
        if let Some((_, at)) = self.on.take() {
            self.before_ns = self
                .before_ns
                .saturating_add(now.as_nanos().saturating_sub(at.as_nanos()));
        }
    }
}

/// A canvas's hold scene while a stream of it holds with one, kept on the control
/// thread (phase 2b).
pub(super) struct HoldRun {
    scene: SceneId,
    sound: HoldSound,
    /// When it was asked for, and the exports' scene frames then.
    since: Timestamp,
    frames_at: u64,
    /// Its compositor once bound and adopted.
    view: Option<HoldView>,
    /// Its hold mixer once bound and adopted ([`HoldSound::Scene`] with audio on).
    mixer: Option<HoldMix>,
    /// Set when it fell back: the binder no longer binds it.
    fallback: Option<HoldFallback>,
    /// Set when its sound alone fell back to silence (the hold mixer failed); the
    /// picture runs on.
    sound_fallback: Option<HoldFallback>,
    /// Released: its parts leave at the next sync.
    released: bool,
}

impl HoldRun {
    pub(super) fn view_mut(&mut self) -> Option<&mut HoldView> {
        self.view.as_mut()
    }
}

/// A hold scene's mixer: its strips are the scene's sources at the scene's levels.
/// No monitor output, no meters, no solo (ADR-0124 phase 2b).
pub(super) struct HoldMix {
    node: NodeId,
    partition: PartitionId,
    params: ParamSender<MixerParams>,
    stats: Arc<MixerStats>,
    strips: Vec<MixStrip>,
}

/// A hold scene's compositor and what draws its layers.
pub(super) struct HoldView {
    pub(super) compositor: NodeId,
    pub(super) layers: LayerSender,
    pub(super) bound: BoundCompositor,
}

impl Studio {
    /// The hold scenes the binder binds now: one per canvas, those not fallen back
    /// or released.
    pub(super) fn hold_requests(&self) -> BTreeMap<CanvasId, HoldScene> {
        self.hold_scenes
            .iter()
            .filter(|(_, r)| r.fallback.is_none() && !r.released)
            .map(|(c, r)| {
                let sound = self.mixer.is_some()
                    && r.sound == HoldSound::Scene
                    && r.sound_fallback.is_none();
                (*c, HoldScene::new(r.scene, sound))
            })
            .collect()
    }

    /// Adopts a newly bound hold compositor and links it to the "hold" input of every
    /// holdable export of its canvas; a retired one goes to `leaving`.
    pub(super) fn sync_hold_views(
        &mut self,
        report: &BindReport,
        leaving: &mut Vec<Added>,
    ) -> Result<(), RecordError> {
        let gpu = |node| Added {
            node,
            partition: RENDER,
            gpu: true,
        };
        let canvases: Vec<CanvasId> = self.hold_scenes.keys().copied().collect();
        for canvas in canvases {
            let bound = report.compositors.get(&(canvas, Role::Hold)).cloned();
            let size = self.canvas_size(canvas).ok();
            let Some(run) = self.hold_scenes.get_mut(&canvas) else {
                continue;
            };
            match (run.view.as_mut(), bound, size) {
                (Some(view), Some(bound), _) => view.bound = bound,
                (None, Some(bound), Some((width, height))) => {
                    let (compositor, layers) = CompositorNode::new(
                        CanvasDesc { width, height },
                        MAX_LAYERS as u32,
                        Vec::new(),
                    );
                    self.engine
                        .adopt::<GpuRuntime>(RENDER, bound.node, Box::new(compositor))?;
                    for chain in self.chains.video.iter().flatten() {
                        if chain.canvas == canvas && chain.hold.is_some() {
                            self.graph
                                .connect(bound.node, "canvas", chain.export.node, "hold")?;
                        }
                    }
                    run.view = Some(HoldView {
                        compositor: bound.node,
                        layers,
                        bound,
                    });
                }
                (Some(_), None, _) => {
                    if let Some(view) = run.view.take() {
                        leaving.push(gpu(view.compositor));
                    }
                }
                _ => {}
            }
            self.sync_hold_mixer(canvas, report.hold_mixers.get(&canvas), leaving)?;
        }
        // Released and fallen-back runs whose parts are gone are forgotten (a fallen
        // back one stays, without parts, for its state while the hold lasts).
        self.hold_scenes
            .retain(|_, r| !(r.released && r.view.is_none() && r.mixer.is_none()));
        Ok(())
    }

    /// Adopts a newly bound hold mixer of `canvas` and links its output to the "hold"
    /// input of every audio encoder of the canvas's streams that has one; a retired
    /// one goes to `leaving`. Its strips are sent around the apply
    /// ([`Studio::send_hold_strips`]).
    fn sync_hold_mixer(
        &mut self,
        canvas: CanvasId,
        bound: Option<&BoundMixer>,
        leaving: &mut Vec<Added>,
    ) -> Result<(), RecordError> {
        let slots = self.audio_chains_of(canvas);
        let Some(run) = self.hold_scenes.get_mut(&canvas) else {
            return Ok(());
        };
        match (run.mixer.as_ref(), bound) {
            (None, Some(bound)) => {
                let partition = self
                    .graph
                    .node(bound.node)
                    .map(|n| n.partition)
                    .ok_or(RecordError::NotBound("hold mixer"))?;
                let (node, params, stats) = MixerNode::new(
                    self.settings.audio_graph,
                    MAX_LAYERS,
                    MixerParams::new(Vec::new()).with_master(self.master.gain()),
                );
                self.engine
                    .adopt::<()>(partition, bound.node, Box::new(node))?;
                for slot in slots {
                    // Every chain built to hold has the "hold" input: a stream's own
                    // (`HoldGate`) and one a recording shares (`PauseGate::with_hold`).
                    if let Some(chain) = self.chains.audio[slot].as_ref()
                        && chain.hold.is_some()
                    {
                        self.graph
                            .connect(bound.node, "main", chain.encoder.node, "hold")?;
                    }
                }
                run.mixer = Some(HoldMix {
                    node: bound.node,
                    partition,
                    params,
                    stats,
                    strips: Vec::new(),
                });
            }
            (Some(_), None) => {
                if let Some(mix) = run.mixer.take() {
                    leaving.push(Added {
                        node: mix.node,
                        partition: mix.partition,
                        gpu: false,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Sends each hold mixer its strips: the scene's sources at the scene's levels
    /// (ADR-0091) under the canvas master. Before the apply (`report` without
    /// readers) a picture-and-sound source keeps its strip only while its old reader
    /// is known, and the strips leaving stay until after it; after the apply the
    /// strips are exactly the bound inputs.
    pub(super) fn send_hold_strips(&mut self, report: &BindReport, applied: bool) {
        let collection = &self.collection;
        let master = self.master;
        for (canvas, run) in &mut self.hold_scenes {
            let (Some(mix), Some(bound)) = (run.mixer.as_mut(), report.hold_mixers.get(canvas))
            else {
                continue;
            };
            let levels = scene_mix_of(collection, run.scene);
            let mut staged = Vec::new();
            for input in &bound.inputs {
                let late = !applied
                    && report.picture_sound.iter().any(|p| {
                        p.canvas == *canvas && p.role == Role::Hold && p.source == input.source
                    });
                let producer = if late {
                    let Some(kept) = mix.strips.iter().find(|s| s.source == input.source) else {
                        continue;
                    };
                    kept.producer
                } else {
                    input.producer
                };
                staged.push(MixStrip {
                    producer,
                    source: input.source,
                    meter: None,
                    audio: levels.get(&input.source).copied().unwrap_or_default(),
                });
            }
            let mut sent = staged.clone();
            if !applied {
                sent.extend(
                    mix.strips
                        .iter()
                        .filter(|old| !staged.iter().any(|n| n.producer == old.producer))
                        .cloned(),
                );
            }
            let _ = mix.params.send(hold_mixer_params(&sent, master));
            mix.strips = staged;
        }
    }

    /// Hold mixer inputs that had no strip or an invalid block.
    pub(super) fn hold_mixer_anomalies(&self) -> u64 {
        self.hold_scenes
            .values()
            .filter_map(|r| r.mixer.as_ref())
            .map(|m| m.stats.invalid_inputs() + m.stats.unknown_inputs())
            .sum()
    }

    /// Sends every hold mixer its sources' new volume and mute and the master's
    /// (from [`Studio::set_mix`]).
    pub(super) fn set_hold_mix(&mut self, collection: &SceneCollection) {
        let master = self.master;
        for run in self.hold_scenes.values_mut() {
            let Some(mix) = run.mixer.as_mut() else {
                continue;
            };
            let levels = scene_mix_of(collection, run.scene);
            for strip in &mut mix.strips {
                if let Some(audio) = levels.get(&strip.source) {
                    strip.audio = *audio;
                }
            }
            let _ = mix.params.send(hold_mixer_params(&mix.strips, master));
        }
    }

    /// The scene frames the holdable exports of `canvas` read back so far.
    fn scene_frames(&self, canvas: CanvasId) -> u64 {
        self.chains
            .video
            .iter()
            .flatten()
            .filter(|c| c.canvas == canvas)
            .filter_map(|c| c.hold.as_ref())
            .map(|(h, _)| h.scene_frames())
            .sum()
    }

    /// Falls a canvas's scene hold back to the frozen frame and silence: every chain
    /// of the canvas held with the scene changes to [`HoldPicture::Freeze`]; the
    /// scene's parts leave at the next sync.
    pub(super) fn fall_back(&mut self, canvas: CanvasId, why: HoldFallback) {
        let Some(run) = self.hold_scenes.get_mut(&canvas) else {
            return;
        };
        if run.fallback.is_some() || run.released {
            return;
        }
        run.fallback = Some(why);
        for chain in self.chains.video.iter().flatten() {
            if chain.canvas == canvas
                && let Some((hold, _)) = &chain.hold
                && hold.switch().held() == Some(HoldPicture::Scene)
            {
                hold.switch().hold(HoldPicture::Freeze);
            }
        }
        for a in self.audio_chains_of(canvas) {
            if let Some(switch) = self.chains.audio[a].as_ref().and_then(|c| c.hold.as_ref())
                && switch.held() == Some(HoldPicture::Scene)
            {
                switch.hold(HoldPicture::Freeze);
            }
        }
    }

    /// The audio chain slots of the outputs whose video chain exports `canvas`.
    fn audio_chains_of(&self, canvas: CanvasId) -> Vec<usize> {
        let mut slots: Vec<usize> = self
            .outputs
            .iter()
            .filter(|o| {
                o.video
                    .and_then(|v| self.chains.video[v].as_ref())
                    .is_some_and(|c| c.canvas == canvas)
            })
            .filter_map(|o| o.audio)
            .collect();
        slots.sort_unstable();
        slots.dedup();
        slots
    }

    /// Checks the hold scenes on the control thread (from [`Studio::service`]): one
    /// that sent no picture within [`HOLD_SCENE_TIMEOUT`] or whose compositor failed
    /// falls back. True when one fell back (an update then releases its parts).
    pub(super) fn check_hold_scenes(&mut self, failed: &BTreeSet<NodeId>) -> bool {
        if self.hold_scenes.is_empty() {
            return false;
        }
        let now = self.engine.clock().now();
        let mut fell = Vec::new();
        let mut silenced = Vec::new();
        for (canvas, run) in &self.hold_scenes {
            if run.fallback.is_some() || run.released {
                continue;
            }
            if run
                .view
                .as_ref()
                .is_some_and(|v| failed.contains(&v.compositor))
            {
                fell.push((*canvas, HoldFallback::Failed));
                continue;
            }
            if run.sound_fallback.is_none()
                && run.mixer.as_ref().is_some_and(|m| failed.contains(&m.node))
            {
                silenced.push(*canvas);
            }
            let waited = now.as_nanos().saturating_sub(run.since.as_nanos());
            if waited >= HOLD_SCENE_TIMEOUT.as_nanos() as u64
                && self.scene_frames(*canvas) == run.frames_at
            {
                fell.push((*canvas, HoldFallback::NoPicture));
            }
        }
        let any = !fell.is_empty() || !silenced.is_empty();
        for canvas in silenced {
            // The encoder's gate fills silence where no scene block comes.
            if let Some(run) = self.hold_scenes.get_mut(&canvas) {
                run.sound_fallback = Some(HoldFallback::Failed);
            }
        }
        for (canvas, why) in fell {
            self.fall_back(canvas, why);
        }
        any
    }

    /// True once after [`Studio::service`] fell a hold scene, or its sound, back
    /// (ADR-0124 phase 2b): the host then syncs (an update), so the scene's parts
    /// leave, and reads [`Studio::output_hold`] again.
    pub fn take_hold_fallback(&mut self) -> bool {
        std::mem::take(&mut self.hold_fell_back)
    }

    /// Falls back every scene hold whose scene `collection` no longer has or can no
    /// longer show on its canvas (an update, a collection switched).
    pub(super) fn check_hold_scenes_in(&mut self, collection: &SceneCollection) {
        let gone: Vec<CanvasId> = self
            .hold_scenes
            .iter()
            .filter(|(c, r)| {
                r.fallback.is_none()
                    && !collection
                        .showable_scenes(**c)
                        .iter()
                        .any(|s| s.id == r.scene)
            })
            .map(|(c, _)| *c)
            .collect();
        for canvas in gone {
            self.fall_back(canvas, HoldFallback::SceneGone);
        }
    }

    /// Puts a running stream on hold with a scene of the collection (ADR-0124 phase
    /// 2b): the scene is bound on the stream's canvas (its compositor, its media and
    /// browser sources playing) and applied, then the stream's switches go to
    /// [`HoldPicture::Scene`]; the export sends the frozen frame until the scene's
    /// first picture. The sound is silence for now ([`HoldSound::Scene`] needs the hold
    /// mixer's wiring). A second stream of the canvas may hold with the same scene and
    /// sound (they share it), not another ([`HoldRefused::OtherSceneHeld`]). When no
    /// scene picture arrives within [`HOLD_SCENE_TIMEOUT`], or the scene leaves the
    /// collection, the hold falls back to [`HoldPicture::Freeze`] and
    /// [`Studio::output_hold`] says why.
    pub fn hold_output_scene(
        &mut self,
        id: OutputId,
        scene: SceneId,
        sound: HoldSound,
        sources: &SourceTypes,
    ) -> Result<Timestamp, HoldRefused> {
        let r = self.attached(id).ok_or(HoldRefused::NotRunning)?;
        let canvas = r
            .video
            .and_then(|v| self.chains.video[v].as_ref())
            .map(|c| c.canvas)
            .ok_or(HoldRefused::NotHoldable)?;
        if self.collection.scene(scene).is_none() {
            return Err(HoldRefused::UnknownScene);
        }
        if !self
            .collection
            .showable_scenes(canvas)
            .iter()
            .any(|s| s.id == scene)
        {
            return Err(HoldRefused::SceneNotShowable);
        }
        let fresh = match self.hold_scenes.get(&canvas) {
            Some(run) if run.fallback.is_none() && !run.released => {
                if run.scene != scene || run.sound != sound {
                    return Err(HoldRefused::OtherSceneHeld);
                }
                false
            }
            _ => true,
        };
        // The same checks as any hold, before anything is bound.
        self.check_holdable(id, HoldPicture::Scene)?;
        if fresh {
            let now = self.engine.clock().now();
            let frames_at = self.scene_frames(canvas);
            self.hold_scenes.insert(
                canvas,
                HoldRun {
                    scene,
                    sound,
                    since: now,
                    frames_at,
                    view: None,
                    mixer: None,
                    fallback: None,
                    sound_fallback: None,
                    released: false,
                },
            );
            let collection = self.collection.clone();
            let bound = self.update(&collection, sources).is_ok()
                && self
                    .hold_scenes
                    .get(&canvas)
                    .is_some_and(|r| r.view.is_some());
            if !bound {
                if let Some(run) = self.hold_scenes.get_mut(&canvas) {
                    run.released = true;
                }
                let _ = self.update(&collection, sources);
                return Err(HoldRefused::NotBound);
            }
        }
        self.hold_output(id, HoldPicture::Scene)
    }

    /// [`Studio::release_hold`], then the next sync at once: the hold scene's
    /// compositor and its sources leave when no stream of the canvas holds with it.
    pub fn release_hold_scene(
        &mut self,
        id: OutputId,
        sources: &SourceTypes,
    ) -> Result<Timestamp, HoldRefused> {
        let at = self.release_hold(id)?;
        self.settle_hold_scenes(sources);
        Ok(at)
    }

    /// Syncs at once when a hold scene's parts are to leave: one released (its last
    /// stream let go of it, or changed to another picture) or fallen back, or a sound
    /// fallen back to silence. Nothing happens otherwise. The host calls it after
    /// [`Studio::hold_output`] and when [`Studio::take_hold_fallback`] says so.
    pub fn settle_hold_scenes(&mut self, sources: &SourceTypes) {
        let leaving = self.hold_scenes.values().any(|r| {
            (r.released || r.fallback.is_some()) && (r.view.is_some() || r.mixer.is_some())
                || r.sound_fallback.is_some() && r.mixer.is_some()
        });
        if leaving {
            let collection = self.collection.clone();
            let _ = self.update(&collection, sources);
        }
    }

    /// Puts a running stream on hold (ADR-0124 decisions 12 and 13): from the next
    /// render tick its export sends `picture` stamped with each tick and its audio
    /// encoder sends silence; the connection, the encoder sessions and the
    /// timestamps run on. Every stream sharing its chains is held with it. Calling
    /// it again with another picture changes the picture. Returns when the hold
    /// began, on the master clock.
    pub fn hold_output(
        &mut self,
        id: OutputId,
        picture: HoldPicture,
    ) -> Result<Timestamp, HoldRefused> {
        self.check_holdable(id, picture)?;
        let r = self.attached(id).ok_or(HoldRefused::NotRunning)?;
        let (video, audio) = (r.video, r.audio);
        let video_chain = video.and_then(|v| self.chains.video[v].as_ref());
        let audio_chain = audio.and_then(|a| self.chains.audio[a].as_ref());
        let video_hold = video_chain.and_then(|c| c.hold.as_ref());
        let audio_hold = audio_chain.and_then(|c| c.hold.as_ref());
        let canvas = video_chain.map(|c| c.canvas);
        if picture == HoldPicture::Scene
            && !canvas
                .and_then(|c| self.hold_scenes.get(&c))
                .is_some_and(|r| r.fallback.is_none() && !r.released && r.view.is_some())
        {
            // A scene is bound by `hold_output_scene`, never by a picture alone.
            return Err(HoldRefused::NoScene);
        }
        let now = self.engine.clock().now();
        if let Some((hold, format)) = video_hold
            && picture == HoldPicture::Black
            && !hold.has_black()
        {
            // Allocates once, here on the control thread, never on a tick.
            let black = rbs_nodes::black_frame(*format).ok_or(HoldRefused::PictureUnsupported)?;
            hold.set_black(black);
        }
        if let Some((hold, _)) = video_hold {
            hold.switch().hold(picture);
        }
        if let Some(switch) = audio_hold {
            switch.hold(picture);
        }
        if let Some(c) = video_chain {
            // The export reads the switch on its slow path only.
            c.end.wake_at(Timestamp::ZERO);
        }
        for o in self.outputs.iter_mut().filter(|o| o.hold.holdable) {
            if o.id == id || shares(o, video, audio) {
                o.hold.start(picture, now);
            }
        }
        self.release_unused_scene(canvas);
        Ok(now)
    }

    /// The checks every hold makes before it changes anything.
    fn check_holdable(&self, id: OutputId, picture: HoldPicture) -> Result<(), HoldRefused> {
        let r = self.attached(id).ok_or(HoldRefused::NotRunning)?;
        if r.ended.is_some() {
            return Err(HoldRefused::Ending);
        }
        if !r.hold.holdable {
            return Err(HoldRefused::NotHoldable);
        }
        if r.hold.on.is_some_and(|(p, _)| p == picture) {
            return Err(HoldRefused::AlreadyHeld);
        }
        let (video, audio) = (r.video, r.audio);
        match (video, video.map(|v| self.chains.video[v].as_ref())) {
            (Some(_), Some(Some(c))) if c.hold.is_none() => return Err(HoldRefused::NotHoldable),
            (Some(_), Some(None)) => return Err(HoldRefused::NotRunning),
            _ => {}
        }
        match (audio, audio.map(|a| self.chains.audio[a].as_ref())) {
            (Some(_), Some(Some(c))) if c.hold.is_none() => return Err(HoldRefused::NotHoldable),
            (Some(_), Some(None)) => return Err(HoldRefused::NotRunning),
            _ => {}
        }
        // Every other user of these chains is a stream held with it, or a paused
        // recording whose output cuts what the chains send (decision 14).
        let now = self.engine.clock().now();
        for o in self
            .outputs
            .iter()
            .filter(|o| o.id != id && shares(o, video, audio))
        {
            if o.hold.holdable {
                continue;
            }
            let paused = o
                .pause
                .as_ref()
                .and_then(|p| p.read())
                .is_some_and(|w| w.is_open() && w.pause_at <= now.as_nanos());
            if !paused {
                return Err(HoldRefused::SharedWithRecording);
            }
        }
        Ok(())
    }

    /// Marks the scene of `canvas` released when no chain of the canvas holds with it
    /// any more; its parts leave at the next sync.
    fn release_unused_scene(&mut self, canvas: Option<CanvasId>) {
        let Some(canvas) = canvas else {
            return;
        };
        let used = self.chains.video.iter().flatten().any(|c| {
            c.canvas == canvas
                && c.hold
                    .as_ref()
                    .is_some_and(|(h, _)| h.switch().held() == Some(HoldPicture::Scene))
        });
        let fell_back_and_free = self.hold_scenes.get(&canvas).is_some_and(|r| {
            r.fallback.is_some()
                && !self.chains.video.iter().flatten().any(|c| {
                    c.canvas == canvas && c.hold.as_ref().is_some_and(|(h, _)| h.switch().is_held())
                })
        });
        if let Some(run) = self.hold_scenes.get_mut(&canvas)
            && (!used && run.fallback.is_none() || fell_back_and_free)
        {
            run.released = true;
        }
    }

    /// Takes a held stream back to the program from the next render tick, with every
    /// stream sharing its chains. Returns when the hold ended, on the master clock.
    pub fn release_hold(&mut self, id: OutputId) -> Result<Timestamp, HoldRefused> {
        let r = self.attached(id).ok_or(HoldRefused::NotRunning)?;
        if !r.hold.holdable {
            return Err(HoldRefused::NotHoldable);
        }
        if r.hold.on.is_none() {
            return Err(HoldRefused::NotHeld);
        }
        let (video, audio) = (r.video, r.audio);
        let canvas = video
            .and_then(|v| self.chains.video[v].as_ref())
            .map(|c| c.canvas);
        if let Some((hold, _)) = video
            .and_then(|v| self.chains.video[v].as_ref())
            .and_then(|c| c.hold.as_ref())
        {
            hold.switch().release();
        }
        if let Some(switch) = audio
            .and_then(|a| self.chains.audio[a].as_ref())
            .and_then(|c| c.hold.as_ref())
        {
            switch.release();
        }
        let now = self.engine.clock().now();
        for o in self.outputs.iter_mut() {
            if o.id == id || shares(o, video, audio) {
                o.hold.end(now);
            }
        }
        self.release_unused_scene(canvas);
        Ok(now)
    }

    /// Where a running output's holds stand. `None` when `id` does not run or is not
    /// holdable.
    pub fn output_hold(&self, id: OutputId) -> Option<HoldState> {
        let r = self.attached(id)?;
        if !r.hold.holdable {
            return None;
        }
        let mut state = r.hold.state(self.engine.clock().now());
        if state.picture == Some(HoldPicture::Scene)
            && let Some(run) = r
                .video
                .and_then(|v| self.chains.video[v].as_ref())
                .and_then(|c| self.hold_scenes.get(&c.canvas))
        {
            state.scene = Some(run.scene);
            state.sound = Some(run.sound);
            state.fallback = run.fallback;
            state.sound_fallback = run.sound_fallback;
            if run.fallback.is_some() {
                state.picture = Some(HoldPicture::Freeze);
            }
        }
        Some(state)
    }

    /// True when a hold is on for a chain `r` uses (a recording sharing a held
    /// stream's encoder must not resume, ADR-0124 decision 14).
    pub(super) fn chains_held(&self, r: &Attached) -> bool {
        let video = r
            .video
            .and_then(|v| self.chains.video[v].as_ref())
            .and_then(|c| c.hold.as_ref())
            .is_some_and(|(h, _)| h.switch().is_held());
        let audio = r
            .audio
            .and_then(|a| self.chains.audio[a].as_ref())
            .and_then(|c| c.hold.as_ref())
            .is_some_and(|h| h.is_held());
        video || audio
    }

    /// `p` with `r`'s hold fields filled in.
    pub(super) fn with_hold_progress(&self, r: &Attached, mut p: Progress) -> Progress {
        let state = r.hold.state(self.engine.clock().now());
        p.held = state.picture.is_some();
        p.held_ns = state.held_ns;
        p
    }
}

/// True when `o` takes packets from the video chain in slot `video` or the audio
/// chain in slot `audio`.
fn shares(o: &Attached, video: Option<usize>, audio: Option<usize>) -> bool {
    (video.is_some() && o.video == video) || (audio.is_some() && o.audio == audio)
}

/// The volume and mute the hold mixer gives each source of `collection`: its own,
/// with `scene`'s entries over them while that scene is in Own levels (ADR-0091).
fn scene_mix_of(collection: &SceneCollection, scene: SceneId) -> BTreeMap<SourceId, SourceAudio> {
    let levels = collection.scene(scene).and_then(|s| s.audio.as_ref());
    collection
        .sources()
        .map(|s| {
            let audio = match levels {
                Some(l) => l.heard(s.id, s.audio),
                None => s.audio,
            };
            (s.id, audio)
        })
        .collect()
}

/// The hold mixer's parameters: `strips` at their volume and mute, no monitor, meter
/// or solo, under the canvas master.
fn hold_mixer_params(strips: &[MixStrip], master: MasterAudio) -> MixerParams {
    let strips = strips
        .iter()
        .map(|s| Strip::new(s.producer, s.audio.volume).with_muted(s.audio.muted))
        .collect();
    MixerParams::new(strips).with_master(master.gain())
}
