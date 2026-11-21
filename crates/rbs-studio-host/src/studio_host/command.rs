//! What the window asks the control thread: Command and the requests it carries (ADR-0159 decision 8).

use super::*;

/// What the UI asks the studio to do.
#[derive(Debug, Clone)]
pub enum Command {
    /// List what a kind can capture ([`Update::Targets`]).
    ListTargets(SourceKind),
    /// Add a display or window and its "Capture audio too" companion sound as one
    /// undo step (ADR-0096). A refused companion adds nothing.
    AddSourceWithAudio {
        /// Its kind (a display or a window).
        kind: SourceKind,
        /// What it captures.
        target: Target,
    },
    /// Add a source of `kind` capturing `target` to the program scene, on top.
    AddSource {
        /// Its kind.
        kind: SourceKind,
        /// What it captures.
        target: Target,
    },
    /// Add a new source of `kind` capturing `target` as a global source (one undo
    /// step).
    AddGlobalSource {
        /// Its kind.
        kind: SourceKind,
        /// What it captures.
        target: Target,
    },
    /// The toast's Undo after a delete: undoes the newest edit if it is a delete, else
    /// nothing (another edit came after it).
    UndoDelete,
    /// Move a scene to where scene `to` is in the scene list (a row dragged onto
    /// another); one undo step.
    MoveSceneTo {
        /// The scene.
        id: SceneId,
        /// The scene whose place it takes.
        to: SceneId,
    },
    /// Move a source to where source `to` is in the program scene's layer order (a
    /// row dragged onto another row of the Sources list); one undo step.
    MoveSourceTo {
        /// The source.
        id: SourceId,
        /// The source whose place it takes.
        to: SourceId,
    },
    /// A source's volume, a linear factor from 0 to 1 (ADR-0059). A live control: not
    /// on the undo stack, and it reaches the mixer without rebinding the scene. The
    /// window's fader: while the program scene keeps its own levels and places the
    /// source, that scene's level changes instead (ADR-0091 decision 5).
    SetSourceVolume {
        /// The source.
        id: SourceId,
        /// From 0 to 1.
        volume: f32,
    },
    /// Mute or unmute a source's sound (ADR-0059). A live control: not on the undo
    /// stack. In Own levels, as [`Command::SetSourceVolume`], the scene's mute.
    SetSourceMuted {
        /// The source.
        id: SourceId,
        /// Muted.
        muted: bool,
    },
    /// The Mixer panel is shown (`true`) or hidden. Meters stay on while a control
    /// client subscribes to `audio.levels` (ADR-0143 R2).
    SetMetersUi(bool),
    /// Probe video and audio encoders once and publish the list. Sent before output
    /// settings are shown.
    EnsureEncoders,
    /// Turns a scene's Own levels on (`own` true, copying what is heard) or puts it
    /// back in Shared levels (ADR-0091, `scene.audio.own` / `scene.audio.shared`). One
    /// undo step; a refusal comes back as [`Update::Error`].
    SetSceneOwnLevels {
        /// The scene.
        scene: SceneId,
        /// Own levels on.
        own: bool,
    },
    /// Removes a scene's own level for one source (`scene.audio.reset_source`).
    ResetSceneLevel {
        /// The scene.
        scene: SceneId,
        /// The source.
        source: SourceId,
    },
    /// Arranges the scene the vertical canvas shows (`canvas.auto_layout`, protocol
    /// 1.44). One undo step; a refusal comes back as [`Update::Error`].
    AutoLayoutVertical(rbs_api::request::VerticalLayout),
    /// The vertical canvas's size (`canvas.set_vertical_size`, protocol 1.44): its
    /// compositor restarts, [`Update::Vertical`] answers.
    SetVerticalSize(Size),
    /// The canvas's master volume, a linear factor from 0 to 1 (ADR-0081). A live
    /// control like a source's volume: not on the undo stack, no rebind.
    SetMasterVolume {
        /// From 0 to 1.
        volume: f32,
    },
    /// Mute or unmute the canvas's master (ADR-0081). A live control.
    SetMasterMuted {
        /// Muted.
        muted: bool,
    },
    /// Show loudness on or off (`loudness.set_enabled`, ADR-0092), saved.
    SetLoudnessShown(bool),
    /// The loudness target (LUFS) and true-peak limit (dBTP), saved (`settings.set`
    /// `loudness`, protocol 1.59).
    SetLoudnessSettings {
        /// LUFS.
        target: f32,
        /// dBTP.
        true_peak_limit: f32,
    },
    /// Hide a source from the Mixer or show it again (ADR-0080). A live view choice like
    /// the mute: not on the undo stack; the sound is untouched.
    SetSourceMixerHidden {
        /// The source.
        id: SourceId,
        /// Hidden from the Mixer.
        hidden: bool,
    },
    /// Add the voice chain to a sound source (ADR-0130 decision 10; one undo step).
    AddVoiceChain(SourceId),
    /// Apply a built-in look to a source's item and filters (ADR-0075; one undo step).
    ApplyLook {
        /// The source.
        source: SourceId,
        /// The look's id (`builtin:...`).
        look: String,
    },
    /// Add a built-in sample picture to the scene being edited: a background below
    /// everything, a frame above it (ADR-0074, ADR-0075; one undo step).
    AddSample {
        /// The sample's id (`rbs_sources::still::SAMPLES`).
        sample: String,
    },
    /// Add a colour shape on the scene being edited, centred, one undo step.
    /// 0 circle, 1 square, 2 rectangle, 3 rounded rectangle, 4 ellipse, 5 triangle,
    /// 6 arrow.
    AddStamp {
        /// Which shape.
        kind: u8,
    },
    /// Add an image source from a PNG file, fitted into the canvas, one undo step.
    AddImage {
        /// The file.
        path: std::path::PathBuf,
    },
    /// Add a media file source (ADR-0108), centred at half the canvas, one undo step.
    AddMedia {
        /// A local regular file. The demo decoder ignores contents.
        path: std::path::PathBuf,
    },
    /// Make a media file the stinger (ADR-0140): a new media source that no scene
    /// places, and the transition set to Stinger with it, one undo step.
    SetStingerFile(std::path::PathBuf),
    /// Add a browser page (ADR-0109), fitted to the canvas, one undo step.
    AddBrowser {
        /// `http`, `https`, or `file` URL.
        url: String,
    },
    /// Fit a text source's box to its text (ADR-0129 decision 13, Fit box to text):
    /// the item keeps its centre and its scale. One undo step.
    FitTextBox(SourceId),
    /// Whether a colour source draws its fill. Off leaves the inside empty and, when
    /// the shape has no border, turns a border on so the shape stays visible. One
    /// undo step.
    SetSourceFilled {
        /// The source.
        id: SourceId,
        /// Draw the fill.
        on: bool,
    },
    /// Write a filter's full values (a released slider, a colour, Reset to defaults;
    /// one undo step).
    SetFilterValues {
        /// The filter.
        filter: FilterId,
        /// Every field's value.
        values: rbs_filters::Values,
    },
    /// Show values on the running filter without changing the collection (a slider
    /// drag, ADR-0062 decision 3).
    PreviewFilter {
        /// The filter.
        filter: FilterId,
        /// Every field's value.
        values: rbs_filters::Values,
    },
    /// Put the collection's values back on the running filter (a cancelled drag).
    RestoreFilter(FilterId),
    /// Open the Measure sync offset window for a source (ADR-0126); answered with
    /// [`Update::SyncMeasure`].
    OpenSyncMeasure(SourceId),
    /// Start measuring in that window against a picture source.
    StartSyncMeasure(SourceId),
    /// The Measure window closed: a measurement it started is cancelled.
    CloseSyncMeasure,
    /// Point a source at another target of its kind, keeping its name, layout, filters
    /// and place in every scene (one undo step, ADR-0077).
    RetargetSource {
        /// The source.
        id: SourceId,
        /// What it should capture now.
        target: Target,
    },
    /// Grab small pictures of the listed targets of `kind`, two at a time off the control
    /// thread, one [`Update::Thumbnail`] each (ADR-0077). A newer request or
    /// [`Command::StopThumbnails`] makes older work stop at its next item.
    GrabThumbnails {
        /// The source kind whose adapters are asked.
        kind: SourceKind,
        /// (adapter, target) of each row, in the order wanted.
        rows: Vec<(String, String)>,
        /// The largest picture wanted, in pixels.
        max: (u32, u32),
    },
    /// The target list closed: stop grabbing and forget the pictures.
    StopThumbnails,
    /// Ask for what the Source settings window shows ([`Update::SourceSettings`]).
    OpenSourceSettings(SourceId),
    /// The Source settings window closed: drop a late region preview.
    CloseSourceSettings,
    /// Change a source's stored settings by a patch (a `null` removes a key; one undo
    /// step, ADR-0077): what Apply and OK of the Source settings window send.
    SetSourceSettings {
        /// The source.
        id: SourceId,
        /// The keys to change.
        patch: BTreeMap<String, serde_json::Value>,
        /// Patches that share a non-zero gesture (one drag of a field) are one undo
        /// step; 0 never joins.
        gesture: u64,
    },
    /// A message to a browser source's running page: reload it, or run a script
    /// (ADR-0132 decision 5, ADR-0149 decision 3). A reload of a page that does not
    /// run is the `source.reload` call.
    BrowserControl {
        /// The browser source.
        id: SourceId,
        /// What to send.
        message: rbs_browser_ipc::control::Control,
    },
    /// Forget sign-in on the Page tab (ADR-0132 decision 10): the page restarts with a
    /// fresh profile and the kept one is deleted.
    ForgetBrowserSignIn(SourceId),
    /// Ask for what the Reset source (`paste` false) or Paste attributes dialog shows
    /// ([`Update::Attributes`]).
    OpenAttributes {
        /// The source right-clicked.
        source: SourceId,
        /// Paste attributes.
        paste: bool,
    },
    /// Put a scene on air (a cut); in studio mode, make it the preview scene instead
    /// (ADR-0061). A live action: not on the undo stack (ADR-0021).
    SwitchScene(SceneId),
    /// Enter (`true`) or leave studio mode (ADR-0061): the preview scene starts as the
    /// program scene. A live action.
    SetStudioMode(bool),
    /// Rehearse on Preview (ADR-0135 decision 11): the transition on the Preview
    /// picture only. Nothing happens outside studio mode, while a transition runs or
    /// holds, during a rehearsal, and for Cut or a duration of 0.
    Rehearse,
    /// Whether a transition also puts the old program scene in preview (ADR-0098; the
    /// app setting `swap_after_transition`, off by default).
    SetSwapAfterTransition(bool),
    /// Place a video source's picture: the box's top-left corner and size in canvas
    /// pixels. Rotation, crop and opacity stay. A size of zero or a value that is not
    /// finite is refused with [`Update::Error`]. Always answered with
    /// [`Update::BoxPlaced`] carrying `seq`, after any new rows, also when nothing
    /// changed, so the UI can send the next box of a drag (ADR-0048).
    SetSourceBox {
        /// The caller's number for this request.
        seq: u64,
        /// Boxes with the same number other than 0 that follow each other are one undo
        /// step (one drag on the picture panel); 0 is a step of its own.
        gesture: u64,
        /// The source.
        id: SourceId,
        /// Top-left corner.
        position: [f32; 2],
        /// Width and height.
        size: [f32; 2],
        /// The canvas whose layout changes: the main one, or the vertical one, where
        /// the box becomes the item's own layout there (ADR-0093 decision 4, U15).
        canvas: OutputCanvas,
    },
    /// Cut source pixels from the edges of a video source's picture (ADR-0058), on the
    /// layout the canvas shows. With `keep_scale`, the box changes so a source pixel
    /// keeps its size and what stays keeps its place. Always answered with
    /// [`Update::CropPlaced`] carrying `seq`, also when nothing changed; a refusal is
    /// also an [`Update::Error`].
    SetSourceCrop {
        /// The caller's number for this request.
        seq: u64,
        /// Crops with the same number other than 0 that follow each other are one undo
        /// step (one drag on the picture panel); 0 is a step of its own.
        gesture: u64,
        /// The source.
        id: SourceId,
        /// Pixels cut from the left, top, right and bottom.
        crop: [u32; 4],
        /// Keep the scale (else the cropped picture fills the box).
        keep_scale: bool,
        /// The canvas whose layout changes: the vertical canvas's own layout only while
        /// it shows the scene being edited (ADR-0093 decision 4).
        canvas: OutputCanvas,
    },
    /// Flip, turn, fit or centre a video source's picture on the layout the canvas
    /// shows (ADR-0084). One undo step; a refusal is an [`Update::Error`].
    TransformSource {
        /// The source.
        id: SourceId,
        /// What to do.
        op: TransformOp,
        /// Moves that share a non-zero gesture are one undo step. 0 never joins.
        gesture: u64,
        /// The canvas whose layout changes: the vertical canvas's own layout only while
        /// it shows the scene being edited (ADR-0093 decision 4).
        canvas: OutputCanvas,
    },
    /// Show a part of a video source's picture filling as much of the canvas as fits
    /// (ADR-0058, Properties' Show buttons). One undo step.
    SetSourceRegion {
        /// The source.
        id: SourceId,
        /// Which part.
        region: CropRegion,
    },
    /// Shape and border of a video source's picture (ADR-0053), replacing the whole
    /// style of the layout the canvas shows. A style that is not valid is refused with
    /// [`Update::Error`].
    SetSourceStyle {
        /// Styles with the same number other than 0 that follow each other are one undo
        /// step (one slider drag); 0 is a step of its own.
        gesture: u64,
        /// The source.
        id: SourceId,
        /// The new style.
        style: ItemStyle,
        /// The canvas whose layout changes: the vertical canvas's own layout only while
        /// it shows the scene being edited (ADR-0093 decision 4).
        canvas: OutputCanvas,
    },
    /// Sets or clears the scroll of a video source's picture (ADR-0150) on the layout
    /// the canvas shows; refused with [`Update::Error`] when out of range.
    SetSourceScroll {
        /// Edits with the same number other than 0 that follow each other are one undo
        /// step (one field drag); 0 is a step of its own.
        gesture: u64,
        /// The source.
        id: SourceId,
        /// The scroll, or `None` to stop scrolling.
        scroll: Option<Scroll>,
        /// The canvas whose layout changes, as for [`Command::SetSourceStyle`].
        canvas: OutputCanvas,
    },
    /// Start recording the program ([`Update::Recording`] says how it went).
    StartRecording(RecordRequest),
    /// Start recording the vertical canvas ([`Update::VerticalRecording`] says how it
    /// went; ADR-0093 decision 6).
    StartVerticalRecording(RecordRequest),
    /// Go live with the vertical canvas ([`Update::VerticalStream`] says how it went);
    /// its key is read from the vertical account ([`stream_key::vertical_account`]).
    StartVerticalStream(StreamRequest),
    /// The vertical canvas's recording settings as saved (`None`: it uses the main
    /// ones). Sent at start and after each change.
    SetVerticalProfile(Option<RecordProfile>),
    /// The vertical canvas's stream settings as saved (`None`: none saved). Sent at
    /// start and after each change.
    SetVerticalStreamProfile(Option<StreamProfile>),
    /// The output profile's stream destinations (ADR-0127 decision 1): the next Go
    /// live of each canvas sends to every enabled, configured one. `true`: the section
    /// did not load, so streaming counts as available (ADR-0148 decision 1).
    SetStreamDestinations(Destinations, bool),
    /// The output profile's Separate files settings: whether Record and
    /// `record.start` without `iso` write the separate files (ADR-0086 decision 4),
    /// and the hardware limit policy (ADR-0087 decision 5). Sent at start and after
    /// each change.
    SetSeparateFiles(SeparateFiles),
    /// The vertical canvas's panel shows (`true`) or not (ADR-0093 decision 7): its
    /// picture runs only while it shows, so a closed panel costs nothing. Kept while
    /// there is no vertical canvas, for the next one.
    SetVerticalPicture(bool),
    /// Open projector `id` (chosen by the UI) of a canvas (ADR-0125 decision 2): a
    /// picture of that canvas at its full size, only while it is open. Answered with
    /// [`Update::Projector`]. An `id` already open is moved to `canvas`.
    OpenProjector {
        /// The UI's id for it.
        id: u32,
        /// Whose program it shows.
        canvas: OutputCanvas,
    },
    /// Close projector `id`: its picture goes. Unknown ids change nothing.
    CloseProjector(u32),
    /// Open the Interact window of a browser source (ADR-0132 decision 6): its page is
    /// held at Shown and the UI gets an [`Update::Interact`].
    OpenInteract(SourceId),
    /// Close the Interact window of a browser source.
    CloseInteract(SourceId),
    /// Input from a focused Interact window to its page (never logged).
    InteractInput {
        /// The browser source.
        source: SourceId,
        /// The event, in page pixels.
        event: rbs_browser_ipc::input::InputEvent,
    },
    /// Change the canvas's size and rate (Settings, Video). The studio restarts with
    /// it (ADR-0041: a running studio's canvas does not change); the program scene's
    /// items are placed again for the new size. Refused while recording. Always
    /// answered with [`Update::Canvas`]; the UI drops its preview connection first,
    /// since the old studio's preview ends.
    SetCanvas {
        /// The new size.
        size: Size,
        /// The new rate.
        fps: FrameRate,
    },
    /// The recording settings as saved: what `record.start` uses for control API
    /// clients and `settings.get` shows. Sent at start and after each change.
    SetProfile(RecordProfile),
    /// Go live: stream the program ([`Update::Stream`] says how it went). The stream key
    /// is read from the host's key store now, never earlier.
    StartStream(StreamRequest),
    /// Start sending the program to the virtual camera (ADR-0066 decision 12) at this
    /// size and rate; answered with [`Update::VirtualCamera`], and an
    /// [`Update::Error`] when it did not start.
    StartVirtualCamera {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Frames per second.
        fps: FrameRate,
    },
    /// Stop sending to the virtual camera; answered with [`Update::VirtualCamera`].
    StopVirtualCamera,
    /// The virtual camera as Settings › Virtual camera knows it: what `vcam.start` and
    /// `vcam.status` use for control API clients. Sent at start and after each change.
    SetVirtualCamera(CameraSetup),
    /// The replay buffer's settings as saved (ADR-0088): used when it arms. Sent at
    /// start and after each change; a buffer that runs keeps the settings it started
    /// with until it is armed again.
    SetReplay(crate::replay_settings::ReplaySetup),
    /// Save the last `seconds` (all of the buffer for `None`) as a clip file; answered
    /// with [`Update::ReplaySaved`] once it is written.
    SaveReplay {
        /// How much of the past, or `None` for all of it.
        seconds: Option<u32>,
    },
    /// A manual marker in every running recording (`marker.add`, ADR-0089 decision
    /// 1); answered with [`Update::Markers`], or [`Update::Error`] when refused.
    AddMarker,
    /// Remove these markers, each a recording's canvas and the marker's index (the
    /// marker notice's Undo); answered with [`Update::Markers`].
    RemoveMarkers(Vec<(OutputCanvas, u32)>),
    /// Pause every running recording, or resume them all when none runs unpaused
    /// (`record.pause_toggle`, ADR-0124); answered with [`Update::RecordPause`], or
    /// [`Update::Error`] when refused.
    PauseRecording,
    /// Hold every live stream, or resume them all when every one is held
    /// (`stream.hold_toggle`, ADR-0124); answered with [`Update::StreamHold`], or
    /// [`Update::Error`] when refused.
    HoldStream,
    /// Break (`output.break_toggle`, ADR-0124 decision 15): hold every live stream and
    /// pause every recording, or resume them all.
    Break,
    /// The stream settings as saved: what `stream.start` uses for control API clients.
    /// Sent at start and after each change.
    SetStreamProfile(StreamProfile),
    /// Save the stream key in the key store (`None` removes it); answered with
    /// [`Update::StreamKey`].
    SaveStreamKey(Option<Secret>),
    /// The key store account of the output profile in use ([`stream_key::account`]):
    /// where Go live, `stream.start` and [`Command::SaveStreamKey`] read and write the
    /// key from now on. Sent when the user switches output profiles.
    SetKeyAccount(String),
    /// Save the vertical stream's key (`None` removes it) under the vertical account
    /// ([`stream_key::vertical_account`]); answered with [`Update::VerticalStreamKey`].
    SaveVerticalStreamKey(Option<Secret>),
    /// Save the key of stream destination `id` (`None` removes it) under its account
    /// ([`crate::stream_destinations::key_account`], ADR-0127 decision 1); answered
    /// with [`Update::DestinationKey`].
    SaveDestinationKey(u32, Option<Secret>),
    /// Start destination `.0` again while its canvas streams, from the live chip's
    /// list (ADR-0127 decision 17): `stream.destination.start`.
    StartDestination(u32),
    /// Stop destination `.0` while the others go on: `stream.destination.stop`.
    StopDestination(u32),
    /// Remove the key saved for a deleted output profile's account (ADR-0051 decision
    /// 5). A failure is reported with [`Update::Error`].
    RemoveStreamKey(String),
    /// Save an imported stream key under a new output profile's account (ADR-0131
    /// decision 5). A failure is reported with [`Update::Error`].
    SaveImportedKey(String, Secret),
    /// Run another scene collection (Scene collection menu): the studio restarts with
    /// it, as for a canvas change, and its first canvas's program scene is shown.
    /// Refused while recording or streaming. Changes are saved through `autosave` from
    /// now on. Always answered with [`Update::Canvas`] and then
    /// [`Update::CollectionSwitched`] carrying `seq`; on a refusal or failure the
    /// collection that ran keeps running.
    SwitchCollection {
        /// The caller's number for this request.
        seq: u64,
        /// The collection.
        collection: Box<SceneCollection>,
        /// Saves it after every change; `None`: changes are not saved.
        autosave: Option<CollectionSaver>,
    },
    /// The output profiles and scene collections with the ones in use, for
    /// `profile.list` and `collection.list`. Sent at start and after every change of
    /// the libraries or of the ones in use; after an [`Update::SwitchRequested`] once
    /// the switch is done or refused (for an output profile after its settings and
    /// key account were sent, for a scene collection after the studio answered).
    SetLibrary {
        /// The output profiles.
        profiles: api::Library,
        /// The scene collections.
        collections: api::Library,
        /// The `seq` of the [`Update::SwitchRequested`] this answers, if any.
        answered: Option<u64>,
    },
    /// Read the OS permission statuses ([`Update::Permissions`], ADR-0141). Fast, never
    /// prompts.
    ReadPermissions,
    /// Show the OS prompt for a permission kind on a worker thread (ADR-0141 decision
    /// 4); [`Update::Permissions`] follows when it is answered. Ignored while a prompt
    /// for the kind is showing.
    RequestPermission(PermissionKind),
    /// Read every add-on's status for Settings › Add-ons ([`Update::AddOns`],
    /// ADR-0145). Reads each add-on's folder; starts nothing.
    ListAddOns,
    /// Download, cancel, update or remove an add-on of the catalog (ADR-0145 decision
    /// 9); progress and the end arrive as [`Update::AddOnChanged`], a refusal as
    /// [`Update::Error`].
    AddOn {
        /// The add-on's id.
        id: String,
        /// What to do.
        action: AddOnAction,
    },
    /// A control API command from the window (ADR-0163 decision 1): run as a remote
    /// call is, with the same checks, handler, events and logging, but without the
    /// caller checks, because the window is the person at this computer (decision 3).
    Api {
        /// The command.
        call: Box<rbs_api::Command>,
        /// How the window wants the answer.
        reply: ApiReply,
    },
}

/// How [`Command::Api`] answers the window (ADR-0163 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiReply {
    /// A refusal shows as [`Update::Error`]; state comes back through the usual
    /// updates.
    Shown,
    /// A refusal is logged and not shown: high-rate live input, and refusals the
    /// window ignores (Undo with nothing to undo).
    Quiet,
    /// As [`ApiReply::Shown`], except a refusal of this kind, which is logged and
    /// not shown: Undo with nothing to undo is `conflict`, a refused step is shown.
    QuietIf(rbs_api::ErrorKind),
    /// As [`ApiReply::Shown`], and [`Update::ApiAnswer`] with this ticket carries the
    /// result, a refusal or a caught panic included.
    Ticket(u64),
    /// As [`ApiReply::Ticket`], but a refusal is not shown: the window handles the
    /// answer itself (the font list while it is being read).
    QuietTicket(u64),
}

/// What [`Command::AddOn`] does: the commands of an add-on's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOnAction {
    /// Download (also Retry): `addon.install`.
    Install,
    /// Update: install the catalog's version, then clean older ones.
    Update,
    /// Cancel a running install.
    Cancel,
    /// Remove every version.
    Remove,
}

/// A video encoder the user can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// Its adapter id.
    pub id: String,
    /// Its name in the UI.
    pub name: String,
    /// What its quality settings can be, from its capabilities.
    pub options: EncoderOptions,
}

/// The quality settings a video encoder takes, from its probed capabilities
/// (`docs/specs/output-ports.md`), so the settings offer only what it can do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncoderOptions {
    /// `codec.video.rate_control` values: `cbr`, `cbr_padded`, `vbr`,
    /// `constant_quality`.
    pub rate_controls: Vec<String>,
    /// H.264 profile names (`high`, `main`, `baseline`) from `codec.video.profiles`.
    pub h264_profiles: Vec<String>,
    /// `codec.video.b_frames.max`.
    pub b_frames_max: u32,
    /// `cbr_padded` is listed: CBR can be padded with filler data.
    pub cbr_padding: bool,
    /// `codec.video.encode` values (`h264`, `hevc`). Empty when the probe said nothing.
    pub codecs: Vec<String>,
}

impl EncoderOptions {
    /// Reads them from a capability set.
    pub fn from_caps(caps: &CapabilitySet) -> Self {
        let set = |key| match caps.get(key) {
            Some(CapabilityValue::Set(v)) => v.to_vec(),
            _ => Vec::new(),
        };
        Self {
            rate_controls: set(&codec_caps::VIDEO_RATE_CONTROL),
            h264_profiles: set(&codec_caps::VIDEO_PROFILES)
                .iter()
                .filter_map(|p| p.strip_prefix("h264.").map(str::to_owned))
                .collect(),
            b_frames_max: match caps.get(&codec_caps::VIDEO_B_FRAMES_MAX) {
                Some(CapabilityValue::Int(n)) => u32::try_from(*n).unwrap_or(0),
                _ => 0,
            },
            cbr_padding: set(&codec_caps::VIDEO_RATE_CONTROL)
                .iter()
                .any(|r| r == "cbr_padded"),
            codecs: set(&codec_caps::VIDEO_ENCODE),
        }
    }

    /// What any of `all` can do: the choice when the encoder is picked automatically.
    pub fn union<'a>(all: impl IntoIterator<Item = &'a EncoderOptions>) -> Self {
        let mut u = EncoderOptions::default();
        for o in all {
            for r in &o.rate_controls {
                if !u.rate_controls.contains(r) {
                    u.rate_controls.push(r.clone());
                }
            }
            for p in &o.h264_profiles {
                if !u.h264_profiles.contains(p) {
                    u.h264_profiles.push(p.clone());
                }
            }
            u.b_frames_max = u.b_frames_max.max(o.b_frames_max);
            u.cbr_padding |= o.cbr_padding;
            for c in &o.codecs {
                if !u.codecs.contains(c) {
                    u.codecs.push(c.clone());
                }
            }
        }
        u
    }
}

/// Where and how to record.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordRequest {
    /// The file, absolute. Its folder is created; an existing file is not replaced.
    pub path: PathBuf,
    /// The container.
    pub container: ContainerId,
    /// The video encoder's adapter id: only this one is used. `None`: the best one.
    pub encoder: Option<String>,
    /// Video encoding: rate control, keyframes, profile, preset, B-frames, padding.
    /// `None`: audio only, no picture encoder.
    pub video: Option<VideoSettings>,
    /// Audio bitrate in bit/s. Zero for PCM and FLAC.
    pub audio_bitrate_bps: u64,
    /// Sample width when `video` is `None`: PCM 16, 24 or 32 (float), FLAC 16 or 24.
    /// `None` is 16-bit.
    pub bit_depth: Option<u16>,
    /// Encoded picture size (ADR-0102). `None`: this output's canvas.
    pub size: Option<(u32, u32)>,
    /// Write the separate files of the sources marked to record separately
    /// (ADR-0086). Only the main canvas's recording has them.
    pub iso: bool,
    /// Write an editor project when it stops (ADR-0089 decision 10). Also what makes
    /// the master keep its timing for the manifest; `None` adds no work (P13).
    pub editor_project: Option<crate::record_profile::EditorProject>,
    /// The reaction-time offset and the automatic markers (ADR-0089 decision 3).
    pub markers: crate::record_profile::MarkerSettings,
}

/// Marks every non-zero undo gesture the window sends (ADR-0164 decision 2); the
/// host clears it from every other caller's gestures, so the two never join a step.
pub const WINDOW_GESTURE: u64 = 1 << 63;

/// A change to a source's separate-file marks (`source.set_record_separately`): an
/// absent field keeps the source's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordPatch {
    /// The main feed: the picture of a picture source, the sound otherwise.
    pub enabled: Option<bool>,
    /// The picture.
    pub video: Option<bool>,
    /// The sound.
    pub audio: Option<bool>,
    /// The picture's form; `Image` only for a still source.
    pub video_as: Option<VideoAs>,
}

impl RecordPatch {
    /// The main feed on or off (the Properties switch, protocol 1.46).
    pub fn enabled(on: bool) -> Self {
        Self {
            enabled: Some(on),
            ..Self::default()
        }
    }

    /// The `source.set_record_separately` call for `source` (the window sends it
    /// through `Command::Api`, ADR-0163).
    pub fn call(self, source: SourceId) -> rbs_api::Command {
        rbs_api::Command::SourceSetRecordSeparately(rbs_api::request::SetRecordSeparately {
            source: source.get(),
            enabled: self.enabled,
            video: self.video,
            audio: self.audio,
            video_as: self.video_as.map(|form| match form {
                VideoAs::Video => rbs_api::request::VideoAs::Video,
                VideoAs::Image => rbs_api::request::VideoAs::Image,
                _ => rbs_api::request::VideoAs::Auto,
            }),
        })
    }

    pub(super) fn from_api(p: &rbs_api::request::SetRecordSeparately) -> Self {
        Self {
            enabled: p.enabled,
            video: p.video,
            audio: p.audio,
            video_as: p.video_as.map(|form| match form {
                rbs_api::request::VideoAs::Auto => VideoAs::Auto,
                rbs_api::request::VideoAs::Video => VideoAs::Video,
                rbs_api::request::VideoAs::Image => VideoAs::Image,
            }),
        }
    }
}
