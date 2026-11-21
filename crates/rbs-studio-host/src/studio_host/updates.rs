//! What the control thread tells the window: Update and the status types it carries (ADR-0159 decision 8).

use super::*;

/// What happened to the recording.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingUpdate {
    /// It runs.
    Started {
        /// The file.
        path: PathBuf,
        /// The video encoder's name.
        video_encoder: String,
        /// The audio encoder's name, when audio is recorded.
        audio_encoder: Option<String>,
        /// The names of the sources written as files of their own (ADR-0086), in
        /// track order; empty for a recording of one file.
        separate: Vec<String>,
    },
    /// It could not start, in words; nothing records.
    NotStarted(String),
    /// It stopped, asked to or because it failed; the file is closed.
    Stopped {
        /// The file.
        path: PathBuf,
        /// Why it stopped on its own, if it did.
        failure: Option<String>,
        /// Video frames encoded.
        frames: u64,
        /// Video frames dropped at the export or the encoder.
        dropped: u64,
        /// Packets the video encoder still held after its final flush (normally 0).
        flush_lost: u64,
    },
}

/// What happened to the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamUpdate {
    /// It runs and connects in the background ([`StreamUpdate::Link`] follows).
    Started {
        /// The server URL.
        url: String,
        /// The video encoder's name.
        video_encoder: String,
        /// The audio encoder's name, when audio is sent.
        audio_encoder: Option<String>,
    },
    /// It could not start, in words; nothing streams.
    NotStarted(String),
    /// The server needs a key and the key store has none; nothing streams.
    NoKey,
    /// The connection changed: connected, lost, another attempt.
    Link(Link),
    /// It ended, asked to or because it failed for good.
    Ended {
        /// Why it ended on its own, if it did.
        failure: Option<String>,
        /// Video frames encoded.
        frames: u64,
        /// Video frames dropped at the export or the encoder, and items the transport
        /// dropped while the connection was slow or down.
        dropped: u64,
        /// Times the stream connected (0: it never reached the server).
        connects: u64,
    },
}

/// What the studio tells the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// The answer to a [`Command::Api`] sent with [`ApiReply::Ticket`] (ADR-0163
    /// decision 2).
    ApiAnswer {
        /// The window's ticket.
        ticket: u64,
        /// What the call answered, or why it was refused.
        result: Result<rbs_api::Response, ApiError>,
    },
    /// An add-on's status changed (ADR-0145): an install's progress or end, a clean
    /// or a removal. Settings › Add-ons asks for the whole list when it opens.
    AddOnChanged(api::AddOnChanged),
    /// Every add-on, answering [`Command::ListAddOns`].
    AddOns(api::AddOnList),
    /// Show this add-on in Settings › Add-ons: Add Source › Browser was chosen while
    /// the browser engine is missing, and no source was added (ADR-0145 decision 8).
    OpenAddOn(String),
    /// The play state of every media source the studio runs (ADR-0108 decision 10).
    Media(Vec<MediaTick>),
    /// The milliseconds left on each running Show cue (ADR-0105), by item, in the
    /// scenes the rows show; empty when none runs. Sent when it changes, apart from
    /// the rows, so a running cue does not rebuild the source list (F-10).
    Cues(Vec<(ItemId, u32)>),
    /// New audio for the sources whose volume, mute or monitor alone changed, to
    /// write into the rows of the last [`Update::Sources`] and [`Update::Heard`]
    /// (F-10: a fader step does not resend every row).
    SourceAudio(Vec<AudioPatch>),
    /// The studio canvas's master, when only it changed: the `master` of the last
    /// [`Update::Scenes`] (F-10).
    Master(MasterAudio),
    /// The monitor output (ADR-0097): device, volume, whether it is wanted and plays,
    /// its latency, solo and problem. Sent once at the start and when it changes.
    Monitor(api::MonitorStatus),
    /// A held T-bar ended other than by going to 0 (ADR-0135 decision 14): the rail's
    /// T-bar goes back to 0.
    TBarReset,
    /// The second canvas runs (`Some`) or does not (`None`), sent when it changes
    /// (ADR-0093) and once at the start.
    Vertical(Option<VerticalState>),
    /// The scenes, the one on air and the collection's sources, after any change of
    /// them (ADR-0059).
    Scenes(SceneList),
    /// The studio mixer's master meter, when the studio has one and each time it is
    /// made again (ADR-0081); `None` while there is no mixer.
    MasterMeter(Option<Arc<rbs_audio::LevelMeter>>),
    /// The loudness meter for the Mixer (ADR-0092 decision 7): whether it is shown,
    /// the running meter's values (`None` while off or without audio), the target and
    /// the true-peak limit. Sent once at the start and when any of them changes.
    Loudness(LoudnessView),
    /// The answer to [`Command::SetSourceBox`] number `seq`: `ok` when the box is the
    /// source's now (changed or already so), else it was refused (an
    /// [`Update::Error`] says why).
    BoxPlaced {
        /// The request's number.
        seq: u64,
        /// Taken.
        ok: bool,
    },
    /// The answer to [`Command::SetSourceCrop`] number `seq`, as [`Update::BoxPlaced`].
    CropPlaced {
        /// The request's number.
        seq: u64,
        /// Taken.
        ok: bool,
    },
    /// The studio runs. `preview`: where the UI connects to the canvas preview (`None`
    /// where the platform has none). `kinds`: the kinds the adapters can add.
    Started {
        /// The preview endpoint.
        preview: Option<PreviewEndpoint>,
        /// Kinds the user can add.
        kinds: Vec<SourceKind>,
        /// Adapters that could not be registered or the GPU's name, for the log.
        notes: Vec<String>,
        /// Video encoders the user can pick, best first.
        encoders: Vec<EncoderInfo>,
        /// Why this system cannot record, or `None` when it can.
        cannot_record: Option<String>,
        /// The canvas's size.
        size: Size,
        /// The canvas's rate.
        fps: FrameRate,
        /// The main display's framebuffer in physical pixels, when known, and its OS
        /// scale for the Video settings label (ADR-0102). The canvas uses `size` and
        /// is not multiplied by the scale.
        display: Option<MainDisplay>,
    },
    /// Video encoders after the one probe, and why recording is unavailable.
    Encoders {
        /// Best first. Empty when none probed successfully.
        encoders: Vec<EncoderInfo>,
        /// `None` when this computer can record video.
        cannot_record: Option<String>,
    },
    /// The canvas the studio runs now, after [`Command::SetCanvas`]: changed, or
    /// unchanged when the change was refused (an [`Update::Error`] says why). Connect
    /// to `preview` again.
    Canvas {
        /// The preview endpoint.
        preview: Option<PreviewEndpoint>,
        /// The canvas's size.
        size: Size,
        /// The canvas's rate.
        fps: FrameRate,
    },
    /// The studio cannot run (no GPU, or a studio node failed for good).
    Failed(String),
    /// The program scene's sources changed or one changed state.
    Sources(Vec<SourceRow>),
    /// The program scene's sources, top first: what is heard and recorded, for the
    /// Mixer. The same as [`Update::Sources`] outside studio mode (ADR-0061).
    Heard(Vec<SourceRow>),
    /// Where the UI connects to the preview scene's picture in studio mode (ADR-0061),
    /// each time it appears (`Some`) or goes (`None`).
    ScenePreview(Option<PreviewEndpoint>),
    /// Where the UI connects to the vertical canvas's picture (ADR-0093 decision 7),
    /// each time it appears (`Some`) or goes (`None`).
    VerticalPreview(Option<PreviewEndpoint>),
    /// The vertical canvas's picture did not start; the UI asks again only after its
    /// panel was hidden and shown.
    VerticalPictureFailed,
    /// A projector's picture (ADR-0125): where the UI connects, and whether the
    /// projector is open, suspended or closed by the studio. Sent when either changes.
    Projector {
        /// The UI's id.
        id: u32,
        /// Where to connect, once the picture runs.
        picture: Option<PreviewEndpoint>,
        /// Its state.
        state: ProjectorState,
    },
    /// An Interact window's picture (ADR-0132 decision 6): where the UI connects, or
    /// that the studio closed it. Sent when either changes.
    Interact {
        /// The browser source.
        source: SourceId,
        /// Where to connect, once the picture runs.
        picture: Option<PreviewEndpoint>,
        /// Its state.
        state: InteractState,
    },
    /// What a kind can capture, in OS order.
    Targets(SourceKind, Vec<Target>),
    /// One target's picture ([`Command::GrabThumbnails`]); `None` when the adapter has
    /// none for it (the row keeps its icon).
    Thumbnail {
        /// The adapter that listed the target.
        adapter: String,
        /// The target.
        target: String,
        /// The picture, if one could be made.
        picture: Option<rbs_capture::Thumbnail>,
    },
    /// What the Source settings window of a source shows ([`Command::OpenSourceSettings`]).
    SourceSettings(SourceSettingsView),
    /// The installed font families, sorted, once a `text.fonts` call found them still
    /// being read (the window's font picker waits for them); empty when no text
    /// renderer is installed.
    TextFonts(Vec<String>),
    /// What the Measure sync offset window shows; `None` closes it (ADR-0126).
    SyncMeasure(Option<SyncMeasureView>),
    /// What the attribute clipboard holds (ADR-0133); `None` while it is empty.
    AttributesClipboard(Option<rbs_api::types::AttributesClipboard>),
    /// What the Reset source or Paste attributes dialog shows (ADR-0133).
    Attributes(AttributesView),
    /// Region-section preview grab (ADR-0103); `None` when the adapter cannot grab.
    RegionPreview {
        /// The source.
        id: SourceId,
        /// Generation so a closed window drops a late answer.
        generation: u64,
        /// The grab, when one was made.
        picture: Option<rbs_capture::RegionGrab>,
    },
    /// A source was deleted, or (`from_scene`) removed from one scene;
    /// [`Command::UndoDelete`] brings it back.
    Deleted {
        /// The source's name.
        name: String,
        /// Only its item in one scene went (Remove from scene, ADR-0065).
        from_scene: bool,
    },
    /// What `edit.undo` and `edit.redo` would apply now (`None`: nothing).
    /// Sent when it changes.
    History {
        /// The next undo.
        undo: Option<StepInfo>,
        /// The next redo.
        redo: Option<StepInfo>,
    },
    /// A command did not work, in words for a toast.
    Error(String),
    /// The recording started, did not start or stopped.
    Recording(RecordingUpdate),
    /// The stream started, did not start, changed its connection or ended.
    Stream(StreamUpdate),
    /// The vertical canvas's recording started, did not start or stopped (ADR-0093
    /// decision 6).
    VerticalRecording(RecordingUpdate),
    /// The vertical canvas's stream started, did not start, changed its connection
    /// or ended.
    VerticalStream(StreamUpdate),
    /// [`Command::SaveStreamKey`] done: `Ok(true)` a key is saved, `Ok(false)` none is,
    /// `Err` why the store refused.
    StreamKey(Result<bool, String>),
    /// [`Command::SaveVerticalStreamKey`] done, as [`Update::StreamKey`].
    VerticalStreamKey(Result<bool, String>),
    /// [`Command::SaveDestinationKey`] done for destination `.0`, as
    /// [`Update::StreamKey`].
    DestinationKey(u32, Result<bool, String>),
    /// A control API client changed the recording settings; the UI shows and saves
    /// them.
    Profile(RecordProfile),
    /// The control API changed whether Record and Go live also act on the vertical
    /// outputs (`output.set_with_vertical`, ADR-0111); the UI saves them in the profile.
    VerticalFlags(VerticalFlags),
    /// A control API client changed the stream settings (not the server or the key);
    /// the UI shows and saves them.
    StreamProfile(StreamProfile),
    /// A control API client changed Swap after transition (`settings.set`, ADR-0164
    /// decision 5); the UI shows and saves it.
    SwapAfterTransition(bool),
    /// A control API client changed Separate files; the UI saves the section and
    /// keeps a hardware-limit value this build does not know when the policy is the
    /// same.
    SeparateFiles(crate::record_profile::SeparateFiles),
    /// A control API client changed the replay buffer's settings; the UI saves them.
    ReplaySetup(crate::replay_settings::ReplaySetup),
    /// A control API client changed the vertical recording's own settings (`None`:
    /// removed); the UI saves or removes the section.
    VerticalProfile(Option<RecordProfile>),
    /// A control API client changed the vertical stream's settings (not the server
    /// or the key); the UI shows and saves them.
    VerticalStreamProfile(StreamProfile),
    /// A control API client turned a stream destination on or off
    /// (`stream.destination.set_enabled`, protocol 1.77); the UI saves the section.
    StreamDestinations(Destinations),
    /// The destinations of a canvas's running stream changed state, counters or error
    /// (ADR-0127 decision 17): the live chip's list; empty once the stream ends.
    LiveDestinations(OutputCanvas, Vec<api::LiveDestination>),
    /// A control API client (`rbs-cli token` over local IPC) made or revoked an access
    /// token: it is saved and in effect; the UI reads the token list again.
    TokensChanged,
    /// The running outputs' counters for the Stats panel, once a second while any
    /// output runs.
    Stats(Stats),
    /// The virtual camera started or stopped sending (`None`), and its counters once a
    /// second while it sends.
    VirtualCamera(Option<CameraStatus>),
    /// The replay buffer's state: armed or not, what it holds, and whether a clip is
    /// being saved. Sent when it changes and once a second while armed.
    Replay(replay::ReplayState),
    /// A saved clip's file, or why it could not be saved.
    ReplaySaved(Result<std::path::PathBuf, String>),
    /// The running recordings' manual marker counts (main, vertical; `None` while
    /// that canvas does not record), after a marker was added or removed through the
    /// window or the control API. `placed` is the marker just added (ADR-0089
    /// decision 10: the window's notice).
    Markers {
        /// Manual markers of each canvas's recording.
        counts: [Option<u32>; 2],
        /// The marker just added, if this update is for one.
        placed: Option<MarkerNote>,
    },
    /// The running recordings' pause states (main, vertical; `None` while that canvas
    /// does not record), after a pause or resume through the window or the control
    /// API (ADR-0124).
    RecordPause([Option<RecordPause>; 2]),
    /// The live streams' hold states (main, vertical; `None` while that canvas does
    /// not stream), after a hold or resume through the window or the control API
    /// (ADR-0124).
    StreamHold([Option<StreamHold>; 2]),
    /// [`Command::SwitchCollection`] with this `seq` is done: `Ok` the new collection
    /// runs, `Err` why the one before still runs.
    CollectionSwitched {
        /// The request's number.
        seq: u64,
        /// How it went.
        result: Result<(), String>,
    },
    /// `import.obs.run` wrote a new scene collection and output profile (control API
    /// 1.82): the UI reloads its libraries, and its shortcuts when `shortcuts`.
    ObsImported {
        /// Hotkeys were added to `shortcuts.toml`.
        shortcuts: bool,
    },
    /// A control API client asked to switch output profiles (`collection` false) or
    /// scene collections to the entry `id` (`profile.switch`, `collection.switch`);
    /// the UI switches as its File menu does, then answers with
    /// [`Command::SetLibrary`].
    SwitchRequested {
        /// The request's number, echoed in [`Command::SetLibrary`]'s `answered`.
        seq: u64,
        /// Scene collections, else output profiles.
        collection: bool,
        /// The entry's id.
        id: String,
    },
    /// After a caught panic: how many outputs the host did not track and stopped
    /// (tests only, ADR-0055).
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    Recovered {
        /// Untracked outputs stopped.
        stopped: usize,
    },
    /// The OS permission statuses (ADR-0141), answering [`Command::ReadPermissions`]
    /// and sent after every permission request. `waiting` is the kind whose OS prompt
    /// is showing now, if any.
    Permissions {
        /// Every kind with an installed adapter, in page order.
        rows: Vec<(PermissionKind, PermissionInfo)>,
        /// The kind being asked for now.
        waiting: Option<PermissionKind>,
    },
}

/// Counters of one running output, for the Stats panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutputStats {
    /// Frames the video encoder took.
    pub frames: u64,
    /// Frames lost before or at the encoder: dropped at export, replaced in the queue
    /// into the encoder, or dropped by the encoder.
    pub dropped: u64,
    /// Packets the output took (all streams).
    pub packets: u64,
    /// The transport reaches its destination now (a stream is live).
    pub connected: bool,
    /// Connection attempts after a failure.
    pub reconnect_attempts: u64,
    /// Items the transport dropped while the connection was slow or down.
    pub transport_drops: u64,
}

impl OutputStats {
    pub(super) fn of(p: &Progress) -> Self {
        Self {
            frames: p.frames_encoded,
            dropped: p.export_drops + p.encoder_skipped + p.encoder_drops,
            packets: p.packets_taken,
            connected: p.connected,
            reconnect_attempts: p.reconnect_attempts,
            transport_drops: p.transport_drops,
        }
    }
}

/// What the Stats panel shows: the recording's and the stream's counters (when they
/// run) and the engine's late ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// The running recording's counters.
    pub recording: Option<OutputStats>,
    /// The running stream's counters.
    pub stream: Option<OutputStats>,
    /// Render ticks skipped because the render worker was late.
    pub late_render_ticks: u64,
    /// Audio ticks skipped because the audio worker was late.
    pub late_audio_ticks: u64,
}

impl Stats {
    pub(super) fn of(recording: Option<&Progress>, stream: Option<&Progress>) -> Self {
        let any = recording.or(stream);
        Self {
            recording: recording.map(OutputStats::of),
            stream: stream.map(OutputStats::of),
            late_render_ticks: any.map_or(0, |p| p.late_render_ticks),
            late_audio_ticks: any.map_or(0, |p| p.late_audio_ticks),
        }
    }
}

/// The virtual camera while RBS sends to it ([`Update::VirtualCamera`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraStatus {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second, rounded.
    pub rate: u32,
    /// What its session did with the frames, when the adapter shares its counters.
    pub counters: Option<VcamStats>,
    /// Why its node failed, until it runs again.
    pub failure: Option<String>,
}

/// The virtual camera as the app knows it ([`Command::SetVirtualCamera`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CameraSetup {
    /// Whether it is added; `None` while the app has not read it yet.
    pub state: Option<InstallState>,
    /// The saved width in pixels.
    pub width: u32,
    /// The saved height in pixels.
    pub height: u32,
    /// The saved frame rate.
    pub fps: FrameRate,
    /// Settings is adding, removing or reformatting the camera now: no start.
    pub busy: bool,
}

/// The platform's virtual camera ([`HostConfig::virtual_camera`]).
#[derive(Clone)]
pub struct CameraFactory(pub Arc<dyn VirtualCameraFactory>);

impl std::fmt::Debug for CameraFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CameraFactory").field(&self.0.id()).finish()
    }
}
