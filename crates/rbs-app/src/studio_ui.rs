//! The window's side of the studio (ADR-0041 decision 7): state shown in the `Studio`
//! global, commands to the control thread, the meters' 30 Hz refresh while the Mixer is
//! visible, the Program panel's preview layer, recording and streaming with their
//! output settings.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_app::canvas_profile::{self, CanvasProfile, RATES};
use rbs_app::edit_history::{EditKind, StepInfo};
use rbs_app::encoder_form::{Choices, EncoderForm};
use rbs_app::network_access::{self, Source};
use rbs_app::record_profile::{
    AUDIO_KBPS, Delivery, Encoding, Format, OutputSize, RateMode, RecordProfile, VIDEO_KBPS,
    VideoCodec, fit_output_size, output_choice_copy, output_size_options, recommended_video_kbps,
    vertical_recording_from_main,
};
use rbs_app::remote_control::{NetworkOptions, RemoteOptions, RemoteServer};
use rbs_app::stream_key::{self, KeyStore};
use rbs_app::stream_profile::{self, SRT_DEFAULT_LATENCY_MS, SRT_LATENCIES_MS, StreamProfile};
use rbs_app::studio_host::{
    Command, CropRegion, EncoderInfo, EncoderOptions, MainDisplay, OutputCanvas, RecordRequest,
    RecordingUpdate, SceneList, SourceKind, SourceRow, SourceSettingsView, SourceStatus, Stats,
    StreamRequest, StreamUpdate, StudioHost, Target, TransformOp, Update,
};
use rbs_app::user_session::{
    DEFAULT_NAME, RECORDING, RECORDING_VERTICAL, STREAM, STREAM_VERTICAL, UserSession,
};
use rbs_audio::LevelMeter;
use rbs_core::fault::SourceFault;
use rbs_record::Link;
use rbs_remote_settings::{self as remote_profile, Access, RemoteProfile};
use rbs_scene::model::{
    Border, ChromeFill, DisplayChrome, ItemId, ItemStyle, SceneId, Shape, SourceId, SourceRegion,
};
use rbs_transport::Secret;
use rbs_ui::fader;
use rbs_ui::meter::{self, Meter, SCALE_MARKS_DB};
use rbs_ui::transform::{DragKeys, MAX_MARKS, Mark, MarkKind, SourceBox};
use rbs_user_data::EntryId;
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::preview_edit::{Effect, Placed, PreviewEdit};
use crate::ui::{
    EncoderData, EntryRow, KindData, LookData, MarkData, MeterRow, ModeData, SampleData, ScaleMark,
    SceneData, SizeBand, SourceData, SourceSettingsData, SourceTargetData, StackData, StatRow,
    Studio, TokenRow,
};

/// Meter refresh while the Mixer is visible (U16: 30 Hz).
pub const METER_PERIOD: Duration = Duration::from_millis(33);
/// Accessible meter values change at most this often (U16).
const REPORT_EVERY: u32 = 30;

/// Codes of `Studio.rec-state`.
pub(crate) mod add_picker;
pub(crate) mod filters;
pub(crate) mod monitor;

pub mod rec_state {
    pub const OFF: i32 = 0;
    pub const STARTING: i32 = 1;
    pub const RECORDING: i32 = 2;
    pub const STOPPING: i32 = 3;
}

/// Codes of `Studio.st-state`.
pub mod st_state {
    pub const OFF: i32 = 0;
    pub const STARTING: i32 = 1;
    pub const LIVE: i32 = 2;
    pub const ENDING: i32 = 3;
}

/// Outputs of `Studio.set-encoding`.
pub mod output {
    pub const RECORDING: i32 = 0;
    pub const STREAM: i32 = 1;
}

/// Field codes of `Studio.set-encoding` (the `EncoderFields` component in studio.slint).
pub mod enc_field {
    pub const ENCODER: i32 = 0;
    pub const VIDEO_RATE: i32 = 1;
    pub const AUDIO_RATE: i32 = 2;
    pub const RATE_MODE: i32 = 3;
    pub const PEAK: i32 = 4;
    pub const QUALITY: i32 = 5;
    pub const PADDING: i32 = 6;
    pub const KEYFRAME: i32 = 7;
    pub const PRESET: i32 = 8;
    pub const PROFILE: i32 = 9;
    pub const B_FRAMES: i32 = 10;
    /// The video codec (H.264 or HEVC).
    pub const VIDEO_CODEC: i32 = 11;
    /// Colour delivery: 0 Video, 1 Screen.
    pub const DELIVERY: i32 = 12;
    /// Open shadows: 0 off, 1 on. Applies while Colour is Video.
    pub const OPEN_SHADOWS: i32 = 13;
}

/// The vertical canvas's picture panel (ADR-0093 decision 7).
pub const VERTICAL_PANEL: &str = "rbs.program?canvas=vertical";

/// Each picture panel's state, with the codes of [`preview_state`].
#[derive(Clone, Copy)]
pub struct PictureStates {
    /// The scene on air (Program, or Preview without studio mode).
    pub program: i32,
    /// The preview scene (studio mode).
    pub scene: i32,
    /// The vertical canvas.
    pub vertical: i32,
}

/// Codes of `Studio.preview-state`.
pub mod preview_state {
    pub const SHOWN: i32 = 0;
    pub const STARTING: i32 = 1;
    pub const NO_PLATFORM: i32 = 2;
    pub const NO_SESSION: i32 = 3;
    pub const OTHER_WINDOW: i32 = 4;
    pub const STOPPED: i32 = 5;
    /// No vertical canvas exists (the Vertical panel only).
    pub const NO_CANVAS: i32 = 7;
    /// The vertical canvas's picture did not start (the Vertical panel only).
    pub const FAILED: i32 = 8;
}

struct MeterSlot {
    source: SourceId,
    level: Arc<LevelMeter>,
    ballistics: Meter,
}

/// A vertical canvas output as the frame's Record and Go live menus show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalOut {
    Off,
    /// Asked to start; the host has not answered.
    Starting,
    On,
    /// Asked to stop or end; the host has not answered.
    Ending,
}

impl VerticalOut {
    /// It runs or is about to change: its settings and the output profile wait.
    pub fn busy(self) -> bool {
        self != VerticalOut::Off
    }

    /// It runs or starts: the menu offers to stop it, and the 9:16 badge shows.
    pub fn on(self) -> bool {
        matches!(self, VerticalOut::Starting | VerticalOut::On)
    }
}

/// Why "Go live vertical" cannot start without a server (the toast adds "Vertical
/// canvas: ").
const VERTICAL_SETUP: &str = "Set up the vertical stream in Settings, Stream.";

/// A vertical output's problem as a toast says it.
fn vertical_why(why: &str) -> String {
    format!("Vertical canvas: {why}")
}

/// What the window knows of the studio.
pub struct StudioUi {
    pub host: Option<StudioHost>,
    /// 0 starting, 1 running, 2 failed.
    pub state: i32,
    pub failure: String,
    rows: Vec<SourceRow>,
    sources: Rc<VecModel<SourceData>>,
    /// The scenes and sources as the host last sent them (ADR-0059).
    scene_list: Option<SceneList>,
    scenes: Rc<VecModel<SceneData>>,
    /// The sources of other scenes the scene on air does not have (Add source).
    existing: Vec<(SourceId, String)>,
    /// The open Add source picker (ADR-0094 B1) and whether the dialog must be redrawn.
    add_picker: Option<add_picker::AddPicker>,
    picker_dirty: bool,
    /// The Library's looks (set once) and sample pictures (made when the panel first
    /// shows, ADR-0075).
    library_looks: Rc<VecModel<LookData>>,
    library_samples: Rc<VecModel<SampleData>>,
    /// The selected source's id (-1 none). The Sources rows follow this.
    pub selected: i32,
    /// The source Properties last showed. It stays when the selection is cleared,
    /// so the panel is empty only when the scene has no sources.
    inspected: Cell<i32>,
    /// The Filters tab's selection and the drag in progress (ADR-0062).
    filter_ui: filters::FilterUi,
    /// An Add source is on its way: the next new source is selected.
    adding: bool,
    /// The source just added is a plain frame: Properties opens on its border.
    reveal_shape: bool,
    /// Taken by [`Self::apply`] when a plain frame was selected.
    focus_shape: Option<i32>,
    /// Moving and resizing on the picture panel (ADR-0048).
    edit: PreviewEdit,
    /// The selection as marks, panel coordinates; and as the toolkit draws them.
    mark_buf: Vec<Mark>,
    marks: Rc<VecModel<MarkData>>,
    kind_list: Vec<SourceKind>,
    kinds: Rc<VecModel<KindData>>,
    /// The Source tab's list (ADR-0077): what the selected source's kind can capture,
    /// filtered by the search field, the source's current target first.
    source_targets: Rc<VecModel<SourceTargetData>>,
    source_target_list: Vec<Target>,
    /// The pictures of the Source tab's targets by (adapter, target), kept only while
    /// the list shows them; dropped when it is listed again (ADR-0077).
    source_pictures: std::collections::HashMap<(String, String), slint::Image>,
    /// The Source settings window's content (ADR-0077): empty until the host answers,
    /// then one entry; and what its choices mean.
    source_settings: Rc<VecModel<SourceSettingsData>>,
    source_settings_view: Option<SettingsChoices>,
    /// The source the list is for, and what the search field holds.
    source_targets_for: Option<SourceId>,
    source_query: String,
    slots: Vec<MeterSlot>,
    /// The master strip (ADR-0081): the mixer's master meter and its ballistics, and
    /// the model of its one row (empty while the studio has no mixer).
    master_slot: Option<(Arc<LevelMeter>, Meter)>,
    master_meter: Rc<VecModel<MeterRow>>,
    master_audio: rbs_scene::model::MasterAudio,
    /// What the Mixer shows or hides, for its menus (ADR-0080): the heard sources.
    heard: Vec<HeardFact>,
    /// The heard sources' names by id, for the monitor's solo line.
    heard_names: Vec<(u32, String)>,
    /// The monitor output (ADR-0097): the Mixer's row and Settings › Audio.
    pub monitor: monitor::MonitorUi,
    meters: Rc<VecModel<MeterRow>>,
    /// Sources hidden from the Mixer (ADR-0080): names only, no meter slot.
    hidden_meters: Rc<VecModel<MeterRow>>,
    /// The Stats panel's lines ([`stat_lines`]).
    stats: Rc<VecModel<StatRow>>,
    scale: Rc<VecModel<ScaleMark>>,
    meter_start: Instant,
    meter_ticks: u32,
    /// The preview endpoint arrived; the platform has a preview.
    pub preview_offered: bool,
    /// The preview layer could not be made in this session.
    pub preview_failed: bool,
    /// The layer exists.
    pub preview_ready: bool,
    /// Studio mode (ADR-0061): the canvas has a preview scene.
    pub studio_mode: bool,
    /// Collection glide defaults (ADR-0100).
    glide: rbs_scene::CollectionGlide,
    /// The preview scene's picture layer exists (studio mode).
    pub scene_preview_ready: bool,
    /// The preview scene's picture layer could not be made in this session.
    pub scene_preview_failed: bool,
    /// The vertical canvas the host runs, if any (ADR-0093).
    pub vertical: Option<rbs_app::studio_host::VerticalState>,
    /// The host has said once whether a vertical canvas runs. Its first word is the
    /// state at start (a saved canvas that runs again), not an Add, so it opens no panel.
    vertical_known: bool,
    /// The vertical canvas's picture layer exists (ADR-0093).
    pub vertical_preview_ready: bool,
    /// The vertical canvas's picture layer could not be made in this session.
    pub vertical_preview_failed: bool,
    /// The studio could not make the vertical canvas's picture; asked again only after
    /// its panel was hidden and shown.
    pub vertical_picture_failed: bool,
    pub deleted: String,
    /// The toast's source was only removed from one scene (ADR-0065).
    pub deleted_from_scene: bool,
    pub deleted_serial: u64,
    /// Settings › Global sources (ADR-0065): the kind picked (an index into
    /// [`Self::kind_codes`], -1 none), its devices and the collection's sources of that
    /// kind that are not global, in the Source field's order.
    pub global_kind: i32,
    global_targets: Vec<Target>,
    global_existing: Vec<SourceId>,
    pub global_option: i32,
    /// What Undo and Redo would apply now (the studio's undo history).
    pub next_undo: Option<StepInfo>,
    pub next_redo: Option<StepInfo>,
    /// [`rec_state`].
    pub rec_state: i32,
    /// The running recording's file and when it started.
    pub rec_file: Option<(PathBuf, Instant)>,
    /// The last recording's outcome, or why one did not start.
    pub rec_note: String,
    /// The note is a problem (shown as an error).
    pub rec_note_bad: bool,
    /// Why this system cannot record (`None`: it can).
    pub cannot_record: Option<String>,
    encoders: Vec<EncoderInfo>,
    /// The output settings and where they are saved.
    pub profile: RecordProfile,
    config: PathBuf,
    /// The output profile and scene collection in use, and their writer (ADR-0051).
    pub user: UserSession,
    /// Why the settings could not be saved or loaded.
    pub profile_note: String,
    /// The folder field shows this; it changes only on load and on Return (typing is
    /// never overwritten by a sync).
    pub folder_serial: u64,
    /// The canvas (Settings, Video), as saved; the studio runs it once started.
    pub canvas: CanvasProfile,
    /// The main display's framebuffer and OS scale, when known.
    display: Option<MainDisplay>,
    /// A canvas change was sent; the studio restarts.
    canvas_restarting: bool,
    /// Why the canvas could not be saved, loaded or changed.
    canvas_note: String,
    /// Remote control settings (Settings, Remote control), as saved.
    remote: RemoteProfile,
    /// The control API server while remote control is on and running; `None` otherwise,
    /// so nothing of it exists while off (ADR-0013).
    remote_server: Option<RemoteServer>,
    /// Why remote control is not running although on.
    remote_failure: Option<String>,
    /// A problem with a token or the settings file.
    remote_note: String,
    /// The port field shows `remote_port_text`; bumped when Rust sets it.
    remote_port_serial: u64,
    remote_port_text: String,
    /// A token just made, and its name: shown once, until dismissed.
    new_token: Option<(String, rbs_api::Token)>,
    /// The token revoked last and where it was: "Undo" puts it back (U4).
    revoked: Option<(usize, rbs_api::TokenEntry)>,
    tokens: Rc<VecModel<TokenRow>>,
    /// Network access while it runs: what clients check (ADR-0046).
    network: Option<NetworkShown>,
    /// Why other computers cannot connect although network access is on.
    network_failure: Option<String>,
    /// A problem with a network access change (own files, new key, busy settings).
    network_note: String,
    /// The stream's settings (Settings, Stream), as saved.
    pub stream: StreamProfile,
    /// A problem with the stream settings, the key or their file.
    stream_note: String,
    /// The server field shows the saved server; bumped when Rust sets it.
    server_serial: u64,
    /// The server field's text while it is changed and not confirmed yet.
    server_draft: Option<String>,
    /// Bumped when a key was saved or removed: the key field is emptied.
    key_serial: u64,
    /// The key field's hint: where keys are kept.
    key_hint: String,
    /// [`st_state`].
    pub st_state: i32,
    /// The vertical canvas's recording and stream (ADR-0093 decision 6): the frame's
    /// Record and Go live menus start and stop them, and an output profile switch
    /// waits for them as for the main ones.
    pub vertical_rec: VerticalOut,
    pub vertical_st: VerticalOut,
    /// The canvas whose settings the Recording and Stream sections show and change
    /// (ADR-0093 decision 6); the main one while there is no vertical canvas.
    settings_canvas: OutputCanvas,
    /// The vertical canvas's recording settings as saved (`recording-vertical`);
    /// `None`: it uses `profile` until its first change.
    vertical_profile: Option<RecordProfile>,
    /// The vertical canvas's stream settings as saved (`stream-vertical`); `None`:
    /// none saved, so the vertical stream has no server.
    vertical_stream: Option<StreamProfile>,
    /// What the Stream section shows for a vertical stream not saved yet.
    no_stream: StreamProfile,
    /// The running stream's server host and when it went live.
    st_live: Option<(String, Instant)>,
    /// The stream's connection, as reported last.
    st_link: Link,
    /// The last stream's outcome, or why one did not start; `st_bad` marks a problem.
    st_note: String,
    st_bad: bool,
    /// `st_note` asks for a missing setting (server, key): hidden once it is set.
    st_setup_note: bool,
    /// Saved keys survive a restart (else `key_saved` is reset when a profile loads).
    keys_persistent: bool,
    /// The Output profile menu's entries.
    profile_rows: Rc<VecModel<EntryRow>>,
    /// The Scene collection menu's entries.
    collection_rows: Rc<VecModel<EntryRow>>,
    /// The number of the last [`Command::SwitchCollection`].
    switch_seq: u64,
    /// A scene collection switch the studio has not answered yet.
    switching: Option<Switching>,
    /// The `seq` of a control API switch ([`Update::SwitchRequested`]) to answer with
    /// the next [`Command::SetLibrary`] sent while no collection switch runs.
    api_switch: Option<u64>,
}

/// A scene collection the studio is asked to run.
struct Switching {
    seq: u64,
    id: EntryId,
    name: String,
    /// The collection to delete once the switch is done (deleting the one in use).
    delete_after: Option<EntryId>,
    /// After a successful switch and delete of the last collection, rename the
    /// replacement to [`DEFAULT_NAME`] once that name is free.
    prefer_default_name: bool,
}

/// What the Remote control section shows while other computers can connect.
struct NetworkShown {
    key_pin: String,
    fingerprint: String,
    /// Where the certificate came from, and whether it was just renewed.
    note: String,
    /// The name other computers may use, worked out once when the server started.
    host: Option<String>,
}

/// The remote control settings are locked by another program.
struct Busy;

impl StudioUi {
    /// The studio's state for the settings in `config` (remote control), the output
    /// profile and collection of `user`, and the canvas the studio starts on
    /// (`canvas`, the collection's first).
    pub fn new(
        config: &Path,
        keys: &dyn KeyStore,
        user: UserSession,
        canvas: (rbs_core::media::Size, rbs_core::timing::FrameRate),
    ) -> Self {
        let (profile, why) = section(&user, RECORDING, RecordProfile::new, RecordProfile::problem);
        let (mut stream, stream_why) =
            section(&user, STREAM, StreamProfile::new, StreamProfile::problem);
        // A key kept only in memory is gone after a restart.
        stream.key_saved &= keys.persistent();
        let canvas = CanvasProfile {
            width: canvas.0.width,
            height: canvas.0.height,
            fps: whole_fps(canvas.1),
            ..CanvasProfile::new()
        };
        let canvas_why: Option<String> = None;
        // Without the lock: `rbs-cli` replaces the file in one rename, so this reads
        // either version whole; a bad file it moves aside is never one `rbs-cli` wrote.
        let (remote, remote_why) = remote_profile::load(config);
        let scale = SCALE_MARKS_DB
            .iter()
            .map(|db| ScaleMark {
                label: format!("{db:.0}").into(),
                at: meter::position(*db),
            })
            .collect::<Vec<_>>();
        let mut ui = Self {
            host: None,
            state: 0,
            failure: String::new(),
            rows: Vec::new(),
            sources: Rc::new(VecModel::default()),
            scene_list: None,
            scenes: Rc::new(VecModel::default()),
            existing: Vec::new(),
            add_picker: None,
            picker_dirty: false,
            library_looks: Rc::new(VecModel::from(library_looks())),
            library_samples: Rc::new(VecModel::default()),
            filter_ui: filters::FilterUi::default(),
            selected: -1,
            inspected: Cell::new(-1),
            adding: false,
            reveal_shape: false,
            focus_shape: None,
            edit: PreviewEdit::default(),
            mark_buf: Vec::with_capacity(MAX_MARKS),
            marks: Rc::new(VecModel::default()),
            kind_list: Vec::new(),
            kinds: Rc::new(VecModel::default()),
            source_targets: Rc::new(VecModel::default()),
            source_settings: Rc::new(VecModel::default()),
            source_settings_view: None,
            source_target_list: Vec::new(),
            source_targets_for: None,
            source_pictures: std::collections::HashMap::new(),
            source_query: String::new(),
            slots: Vec::new(),
            master_slot: None,
            master_meter: Rc::new(VecModel::default()),
            master_audio: rbs_scene::model::MasterAudio::default(),
            heard: Vec::new(),
            heard_names: Vec::new(),
            monitor: monitor::MonitorUi::default(),
            meters: Rc::new(VecModel::default()),
            hidden_meters: Rc::new(VecModel::default()),
            stats: Rc::new(VecModel::default()),
            scale: Rc::new(VecModel::from(scale)),
            meter_start: Instant::now(),
            meter_ticks: 0,
            preview_offered: false,
            preview_failed: false,
            preview_ready: false,
            studio_mode: false,
            glide: rbs_scene::CollectionGlide::default(),
            scene_preview_ready: false,
            scene_preview_failed: false,
            vertical: None,
            vertical_known: false,
            vertical_preview_ready: false,
            vertical_preview_failed: false,
            vertical_picture_failed: false,
            deleted: String::new(),
            deleted_serial: 0,
            deleted_from_scene: false,
            global_kind: -1,
            global_targets: Vec::new(),
            global_existing: Vec::new(),
            global_option: -1,
            next_undo: None,
            next_redo: None,
            rec_state: rec_state::OFF,
            rec_file: None,
            rec_note: String::new(),
            rec_note_bad: false,
            cannot_record: Some("The studio is starting.".into()),
            encoders: Vec::new(),
            profile,
            config: config.to_owned(),
            user,
            profile_note: why
                .map(|w| format!("The saved recording settings were not used: {w}"))
                .unwrap_or_default(),
            folder_serial: 1,
            canvas,
            display: None,
            canvas_restarting: false,
            canvas_note: canvas_why
                .map(|w| format!("The saved canvas was not used: {w}"))
                .unwrap_or_default(),
            remote_port_text: remote.port.to_string(),
            remote,
            remote_server: None,
            remote_failure: None,
            remote_note: remote_why
                .map(|w| format!("The saved remote control settings were not used, so remote control is off: {w}"))
                .unwrap_or_default(),
            remote_port_serial: 1,
            new_token: None,
            revoked: None,
            tokens: Rc::new(VecModel::default()),
            network: None,
            network_failure: None,
            network_note: String::new(),
            stream,
            stream_note: stream_why
                .map(|w| format!("The saved stream settings were not used: {w}"))
                .unwrap_or_default(),
            server_serial: 1,
            server_draft: None,
            key_serial: 0,
            key_hint: stream_key::hint(keys),
            st_state: st_state::OFF,
            vertical_rec: VerticalOut::Off,
            vertical_st: VerticalOut::Off,
            settings_canvas: OutputCanvas::Main,
            vertical_profile: None,
            vertical_stream: None,
            no_stream: StreamProfile::new(),
            st_live: None,
            st_link: Link::default(),
            st_note: String::new(),
            st_bad: false,
            st_setup_note: false,
            keys_persistent: keys.persistent(),
            profile_rows: Rc::new(VecModel::default()),
            collection_rows: Rc::new(VecModel::default()),
            switch_seq: 0,
            switching: None,
            api_switch: None,
        };
        ui.refresh_tokens();
        ui.refresh_library();
        ui
    }

    /// Gives a window's `Studio` global the shared models (once per window).
    pub fn attach_models(&self, g: &Studio<'_>) {
        g.set_sources(ModelRc::from(self.sources.clone()));
        g.set_scenes(ModelRc::from(self.scenes.clone()));
        g.set_library_looks(ModelRc::from(self.library_looks.clone()));
        g.set_library_look_names(ModelRc::new(VecModel::from(library_look_names())));
        g.set_library_samples(ModelRc::from(self.library_samples.clone()));
        g.set_marks(ModelRc::from(self.marks.clone()));
        g.set_kinds(ModelRc::from(self.kinds.clone()));
        g.set_source_targets(ModelRc::from(self.source_targets.clone()));
        g.set_source_settings(ModelRc::from(self.source_settings.clone()));
        g.set_meters(ModelRc::from(self.meters.clone()));
        g.set_hidden_meters(ModelRc::from(self.hidden_meters.clone()));
        g.set_master_meter(ModelRc::from(self.master_meter.clone()));
        g.set_stats(ModelRc::from(self.stats.clone()));
        g.set_scale(ModelRc::from(self.scale.clone()));
        g.set_rc_tokens(ModelRc::from(self.tokens.clone()));
        g.set_profiles(ModelRc::from(self.profile_rows.clone()));
        g.set_collections(ModelRc::from(self.collection_rows.clone()));
    }

    /// The source Properties shows: the selection, else the last source it showed
    /// that still exists, else the first source in the list. `-1` only when the
    /// scene has no sources.
    fn remember_properties(&self) -> i32 {
        let ids: Vec<i32> = (0..self.sources.row_count())
            .filter_map(|i| self.sources.row_data(i).map(|s| s.id))
            .collect();
        let shown = properties_source(self.selected, self.inspected.get(), &ids);
        self.inspected.set(shown);
        shown
    }

    /// Pushes the scalar state into a window's `Studio` global.
    pub fn push(&self, g: &Studio<'_>, states: PictureStates, picture_panel: &str) {
        let preview_state = states.program;
        g.set_studio_mode(self.studio_mode);
        g.set_glide_duration(self.glide.duration_ms as i32);
        g.set_glide_easing(if self.glide.easing == rbs_scene::GlideEasing::Linear {
            0
        } else {
            1
        });
        g.set_scene_preview_state(states.scene);
        g.set_vertical_preview_state(states.vertical);
        g.set_selected_source(self.selected);
        let looks = (0..self.sources.row_count()).any(|i| {
            self.sources
                .row_data(i)
                .is_some_and(|s| s.id == self.selected && s.picture)
        });
        g.set_looks_ready(looks);
        g.set_properties_source(self.remember_properties());
        g.set_profile_name(self.user.profile_name.as_str().into());
        g.set_collection_name(self.user.collection_name.as_str().into());
        g.set_switching(self.switching.is_some());
        g.set_rec_state(self.rec_state);
        g.set_rec_file(
            self.rec_file
                .as_ref()
                .map(|(p, _)| file_name(p))
                .unwrap_or_default()
                .into(),
        );
        g.set_rec_note(self.rec_note.as_str().into());
        g.set_rec_note_bad(self.rec_note_bad);
        g.set_rec_unavailable(self.cannot_record.clone().unwrap_or_default().into());
        let canvases = match &self.vertical {
            Some(v) => vec![
                format!("Main ({} x {})", self.canvas.width, self.canvas.height),
                format!("Vertical ({} x {})", v.size.width, v.size.height),
            ],
            None => Vec::new(),
        };
        g.set_settings_canvases(strings(canvases));
        g.set_settings_canvas(match self.settings_canvas {
            OutputCanvas::Main => 0,
            OutputCanvas::Vertical => 1,
        });
        g.set_rec_form_idle(self.record_form_idle());
        g.set_st_form_idle(self.stream_form_idle());
        g.set_out_uses_main(
            self.settings_canvas == OutputCanvas::Vertical && self.vertical_profile.is_none(),
        );
        let p = self.record_form();
        g.set_out_format(Format::ALL.iter().position(|f| *f == p.format).unwrap_or(0) as i32);
        g.set_out_formats(strings(Format::ALL.iter().map(|f| f.label().to_owned())));
        g.set_out_enc(self.encoder_data(output::RECORDING));
        let canvas = self.settings_canvas_size();
        let rec_size = self.recording_output_size();
        let rec_sizes = output_size_options(canvas, rec_size);
        let rec_nest = output_nest(&rec_sizes, rec_size, canvas);
        g.set_out_same(rec_nest.same);
        g.set_out_portrait(canvas.1 > canvas.0);
        g.set_out_bands(choice_model(&rec_nest.bands));
        g.set_out_extras(choice_model(&rec_nest.extras));
        g.set_out_choice(rec_nest.index);
        g.set_out_value(rec_nest.value.into());
        g.set_out_long(output_long_side(rec_size, canvas) as i32);
        g.set_out_note(self.profile_note.as_str().into());
        g.set_out_summary(
            format!(
                "{} · {} · {} · audio {} kbit/s · {}",
                p.format.label(),
                self.encoder_name(&p.encoding()),
                p.encoding().video_summary(),
                p.audio_kbps,
                p.folder.display()
            )
            .into(),
        );
        g.set_out_kind(p.format.label().into());
        g.set_out_encoder_name(self.encoder_name(&p.encoding()).into());
        g.set_out_video_line(p.encoding().video_summary().into());
        g.set_out_audio_line(format!("{} kbit/s", p.audio_kbps).into());
        g.set_out_where(p.folder.display().to_string().into());
        if g.get_out_folder_serial() != self.folder_serial as i32 {
            g.set_out_folder(self.record_form().folder.display().to_string().into());
            g.set_out_folder_serial(self.folder_serial as i32);
        }
        let canvas_nest = self.canvas_nest();
        g.set_canvas_bands(choice_model(&canvas_nest.bands));
        g.set_canvas_extras(choice_model(&canvas_nest.extras));
        g.set_canvas_choice(canvas_nest.index);
        g.set_canvas_value(canvas_nest.value.into());
        g.set_canvas_width(self.canvas.width as i32);
        g.set_canvas_height(self.canvas.height as i32);
        g.set_canvas_portrait(self.canvas.height > self.canvas.width);
        g.set_canvas_label(format!("{} × {}", self.canvas.width, self.canvas.height).into());
        g.set_canvas_rates(strings(RATES.iter().map(|r| format!("{r}"))));
        g.set_canvas_rate(
            RATES
                .iter()
                .position(|r| *r == self.canvas.fps)
                .unwrap_or(0) as i32,
        );
        g.set_canvas_restarting(self.canvas_restarting);
        g.set_canvas_note(self.canvas_note.as_str().into());
        self.push_remote(g);
        self.push_stream(g);
        self.push_monitor(g);
        g.set_picture_panel(picture_panel.into());
        g.set_state(self.state);
        g.set_failure(self.failure.as_str().into());
        g.set_preview_state(preview_state);
        g.set_deleted(self.deleted.as_str().into());
        g.set_deleted_from_scene(self.deleted_from_scene);
        g.set_deleted_generation(self.deleted_serial as i32);
        g.set_global_kind_names(strings(
            self.kind_codes().iter().map(|(_, l)| (*l).to_owned()),
        ));
        g.set_global_kind(self.global_kind);
        g.set_global_options(strings(self.global_options()));
        g.set_global_devices(self.global_targets.len() as i32);
        g.set_global_option(self.global_option);
        g.set_global_count(self.rows.iter().filter(|r| r.global).count() as i32);
        let globals = self.meters.iter().filter(|m| m.global).count();
        g.set_meter_globals(globals as i32);
        g.set_meter_scenes((self.meters.row_count() - globals) as i32);
        let (kind, name) = step_view(self.next_undo.as_ref());
        g.set_undo_kind(kind);
        g.set_undo_name(name.into());
        let (kind, name) = step_view(self.next_redo.as_ref());
        g.set_redo_kind(kind);
        g.set_redo_name(name.into());
    }

    fn push_remote(&self, g: &Studio<'_>) {
        let r = &self.remote;
        g.set_rc_enabled(r.enabled);
        if g.get_rc_port_serial() != self.remote_port_serial as i32 {
            g.set_rc_port(self.remote_port_text.as_str().into());
            g.set_rc_port_serial(self.remote_port_serial as i32);
        }
        let (status, bad) = self.remote_status();
        g.set_rc_status(status.into());
        g.set_rc_status_bad(bad);
        g.set_rc_accesses(strings(Access::ALL.iter().map(|a| a.label().to_owned())));
        g.set_rc_access_hints(strings(Access::ALL.iter().map(|a| a.hint().to_owned())));
        let (name, token) = self
            .new_token
            .as_ref()
            .map(|(n, t)| (n.as_str(), t.expose()))
            .unwrap_or_default();
        g.set_rc_new_token(token.into());
        g.set_rc_new_token_name(name.into());
        g.set_rc_note(self.remote_note.as_str().into());
        g.set_rc_revoked(
            self.revoked
                .as_ref()
                .map(|(_, e)| e.name.as_str())
                .unwrap_or_default()
                .into(),
        );
        g.set_rc_network(r.network);
        g.set_rc_remote_admin(r.remote_admin);
        g.set_rc_own(r.own_certificate.is_some());
        let (cert, key) = r
            .own_certificate
            .as_ref()
            .map(|o| {
                (
                    o.certificate.display().to_string(),
                    o.key.display().to_string(),
                )
            })
            .unwrap_or_default();
        g.set_rc_own_cert(cert.into());
        g.set_rc_own_key(key.into());
        let shown = self
            .network
            .as_ref()
            .filter(|_| self.remote_server.is_some());
        g.set_rc_network_on(shown.is_some());
        g.set_rc_key_pin(shown.map(|n| n.key_pin.as_str()).unwrap_or_default().into());
        g.set_rc_fingerprint(
            shown
                .map(|n| n.fingerprint.as_str())
                .unwrap_or_default()
                .into(),
        );
        g.set_rc_cert_note(shown.map(|n| n.note.as_str()).unwrap_or_default().into());
        let (status, bad) = self.network_status();
        g.set_rc_network_status(status.into());
        g.set_rc_network_note(self.network_note.as_str().into());
        g.set_rc_network_status_bad(bad);
    }

    /// What the section says about network access, and whether it is a problem.
    fn network_status(&self) -> (String, bool) {
        if !self.remote.enabled || !self.remote.network || self.remote_server.is_none() {
            return (String::new(), false);
        }
        if let Some(why) = &self.network_failure {
            return (format!("Other computers cannot connect: {why}."), true);
        }
        let name = self
            .network
            .as_ref()
            .and_then(|n| n.host.clone())
            .unwrap_or_else(|| "<this computer's address>".into());
        (
            format!(
                "Other computers connect at wss://{name}:{}, or at this computer's address. They must check the key pin below.",
                self.remote.port
            ),
            false,
        )
    }

    /// What the Remote control section says about the server, and whether it is a
    /// problem.
    fn remote_status(&self) -> (String, bool) {
        if !self.remote.enabled {
            return (String::new(), false);
        }
        if let Some(why) = &self.remote_failure {
            return (
                format!(
                    "Remote control is not running: {why}. Choose another port, or turn remote control off{}.",
                    if self.remote.network {
                        " (or turn off access from other computers)"
                    } else {
                        ""
                    }
                ),
                true,
            );
        }
        if self.host.is_none() && self.state == 2 {
            return (
                "Remote control is not running because the studio did not start.".into(),
                true,
            );
        }
        if self.remote_server.is_none() {
            return ("Remote control starts with the studio.".into(), false);
        }
        let status = if self.remote.tokens.is_empty() {
            format!(
                "On. Apps on this computer connect at {}. Create an access token below for each app.",
                self.remote.address()
            )
        } else {
            format!(
                "On. Apps on this computer connect at {}.{}",
                self.remote.address(),
                if self.remote.network {
                    ""
                } else {
                    " Other computers cannot connect."
                }
            )
        };
        (status, false)
    }

    /// Starts or stops the control API server to match the settings: on while remote
    /// control is on and the studio's control thread runs, nothing at all otherwise.
    pub fn sync_remote(&mut self) {
        let want = self.remote.enabled && self.host.is_some();
        if !want {
            self.stop_remote();
            self.remote_failure = None;
            return;
        }
        if self.remote_server.is_some() {
            return;
        }
        if self.host.is_none() {
            return;
        }
        self.reload_tokens();
        let Some(host) = &self.host else {
            return;
        };
        // Local IPC too, so `rbs-cli token` changes reach this app at once.
        let ipc = match rbs_api_ipc::Endpoint::for_current_user() {
            Ok(e) => Some(e),
            Err(why) => {
                log(&format!("remote control: no local IPC endpoint: {why}"));
                None
            }
        };
        let options = RemoteOptions {
            control_waker: Box::new(host.api_waker()),
            audit: Box::new(|a| log(&format!("remote control: {a:?}"))),
            ipc,
            levels: Some(host.level_feed()),
        };
        // The settings may change on the way (a renewed certificate): `host` is taken
        // again below.
        let network = self.prepare_network();
        let started = match network {
            Some(n) => {
                RemoteServer::start_network(self.remote.port, n, self.remote.store(), options)
            }
            None => RemoteServer::start_on_port(self.remote.port, self.remote.store(), options),
        };
        let Some(host) = &self.host else {
            return;
        };
        match started {
            Ok((server, endpoint)) => {
                host.attach_api(endpoint, Some(server.api_tokens(self.config.clone())));
                log(&format!("remote control on at {}", self.remote.address()));
                match (server.ipc_endpoint(), server.ipc_problem()) {
                    (Some(e), _) => log(&format!("remote control: local IPC at {e}")),
                    (None, Some(why)) => log(&format!("remote control: no local IPC: {why}")),
                    (None, None) => {}
                }
                self.remote_server = Some(server);
                self.remote_failure = None;
            }
            Err(e) => {
                log(&format!("remote control did not start: {e}"));
                let mut why = e.to_string();
                if let Some(first) = why.get(..1) {
                    why.replace_range(..1, &first.to_uppercase());
                }
                self.remote_failure = Some(why);
            }
        }
    }

    /// The TLS identity and settings for network access when it is on, made or renewed
    /// under the settings lock (`network_access::prepare`); `None` (the reason in
    /// `network_failure`) when off or when it cannot be used, so only apps on this
    /// computer can connect.
    fn prepare_network(&mut self) -> Option<NetworkOptions> {
        self.network = None;
        self.network_failure = None;
        if !self.remote.network {
            return None;
        }
        let lock = match remote_profile::try_lock(&self.config) {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                self.network_failure = Some(
                    "another program (rbs-cli) was changing the remote control settings. Turn access from other computers off and on again".into(),
                );
                return None;
            }
            Err(e) => {
                log(&format!("remote control settings not locked: {e}"));
                None
            }
        };
        // Under the lock: making or renewing the certificate saves the whole settings
        // file, so it must hold the tokens as saved now (`rbs-cli` may have changed them).
        self.reload_tokens();
        let prepared =
            network_access::prepare(&self.config, &mut self.remote, network_access::now());
        drop(lock);
        let host_names = network_access::host_names();
        match prepared {
            Ok(p) => {
                let note = match &p.source {
                    Source::Own(path) => format!("Your own certificate, {}.", path.display()),
                    Source::Generated { created: true, .. } => {
                        log("remote control: made a key and certificate for network access");
                        "Made by RBS just now.".into()
                    }
                    Source::Generated { renewed: true, .. } => {
                        log(
                            "remote control: renewed the certificate (same key, so the key pin is unchanged)",
                        );
                        "Made by RBS. The certificate was renewed, so its fingerprint changed; the key pin did not.".into()
                    }
                    Source::Generated { .. } => "Made by RBS.".into(),
                };
                let note = format!(
                    "{note} For apps that can only compare the certificate itself: it changes when the certificate is renewed, so prefer the key pin."
                );
                self.network = Some(NetworkShown {
                    key_pin: p.identity.key_pin().to_owned(),
                    fingerprint: p.identity.fingerprint().to_owned(),
                    note,
                    host: host_names.first().cloned(),
                });
                log(&format!(
                    "remote control: network access on, key pin {}{}",
                    p.identity.key_pin(),
                    if self.remote.remote_admin {
                        ", remote admin allowed"
                    } else {
                        ""
                    }
                ));
                Some(NetworkOptions {
                    identity: p.identity,
                    host_names,
                    allow_remote_admin: self.remote.remote_admin,
                })
            }
            Err(why) => {
                log(&format!(
                    "remote control: network access not started: {why}"
                ));
                self.network_failure = Some(why);
                None
            }
        }
    }

    /// Starts the server again with the settings as they are now (connected apps are
    /// disconnected and reconnect).
    fn restart_remote(&mut self) {
        self.stop_remote();
        self.remote_failure = None;
        self.sync_remote();
    }

    /// "Allow access from other computers".
    pub fn set_network(&mut self, on: bool) {
        self.change_network(|r| {
            let changed = r.network != on;
            r.network = on;
            changed
        });
    }

    /// "Allow remote admin".
    pub fn set_remote_admin(&mut self, on: bool) {
        self.change_network(|r| {
            let changed = r.remote_admin != on;
            r.remote_admin = on;
            changed
        });
    }

    /// Uses the user's own certificate and key files.
    pub fn use_own_certificate(&mut self, cert: &str, key: &str) {
        let own = remote_profile::OwnCertificate {
            certificate: PathBuf::from(cert.trim()),
            key: PathBuf::from(key.trim()),
        };
        if let Some(why) = own.problem() {
            self.network_note = why;
            return;
        }
        if let Err(why) = remote_profile::read_own(&own).and_then(|pem| {
            rbs_api_ws::TlsIdentity::from_pem(&pem.certificate, &pem.key).map(|_| ())
        }) {
            self.network_note =
                format!("These files were not used: {why}. Pick other files, or use Made by RBS.");
            return;
        }
        self.change_network(|r| {
            let changed = r.own_certificate.as_ref() != Some(&own);
            r.own_certificate = Some(own);
            changed
        });
    }

    /// Goes back to the key and certificate RBS made.
    pub fn use_generated_certificate(&mut self) {
        self.change_network(|r| r.own_certificate.take().is_some());
    }

    /// "New key": every client must pin the new key; apps on other computers are
    /// disconnected.
    pub fn new_key(&mut self) {
        let Ok(lock) = self.lock_remote() else {
            self.network_note = std::mem::take(&mut self.remote_note);
            return;
        };
        self.reload_tokens();
        let made = network_access::new_key(&self.config, &mut self.remote, network_access::now());
        drop(lock);
        match made {
            Ok(()) => {
                log("remote control: made a new key; clients must pin it again");
                self.network_note.clear();
                self.restart_remote();
            }
            Err(why) => self.network_note = format!("No new key was made: {why}. Try again."),
        }
    }

    /// Changes a network access setting under the lock, saves it, and restarts the
    /// server when `f` changed something.
    fn change_network(&mut self, f: impl FnOnce(&mut RemoteProfile) -> bool) {
        let Ok(lock) = self.lock_remote() else {
            self.network_note = std::mem::take(&mut self.remote_note);
            return;
        };
        self.network_note.clear();
        self.reload_tokens();
        if !f(&mut self.remote) {
            return;
        }
        self.save_remote();
        // The lock is released first: starting network access takes it again.
        drop(lock);
        self.restart_remote();
    }

    /// Stops the control API server, if it runs: connected apps are disconnected.
    pub fn stop_remote(&mut self) {
        if let Some(server) = self.remote_server.take() {
            if let Some(h) = &self.host {
                h.detach_api();
            }
            drop(server);
            log("remote control off");
        }
    }

    pub fn set_remote_enabled(&mut self, on: bool) {
        if self.remote.enabled == on {
            return;
        }
        let Ok(lock) = self.lock_remote() else {
            return;
        };
        self.reload_tokens();
        self.remote.enabled = on;
        self.save_remote();
        // Starting network access takes the lock again.
        drop(lock);
        self.sync_remote();
    }

    pub fn set_remote_port(&mut self, text: &str) {
        match remote_profile::parse_port(text) {
            Ok(port) => {
                self.remote_note.clear();
                self.remote_port_text = port.to_string();
                let lock = (port != self.remote.port).then(|| self.lock_remote());
                if let Some(Err(Busy)) = lock {
                    self.remote_port_text = self.remote.port.to_string();
                } else if port != self.remote.port {
                    self.reload_tokens();
                    self.remote.port = port;
                    self.save_remote();
                    // Listen on the new port: connected apps reconnect there. Starting
                    // network access takes the lock again.
                    drop(lock);
                    self.restart_remote();
                }
            }
            Err(why) => {
                self.remote_note = format!("Port \"{}\" was not used. {why}", text.trim());
                self.remote_port_text = self.remote.port.to_string();
            }
        }
        // The field shows the port in use, also after a refused one.
        self.remote_port_serial += 1;
    }

    pub fn create_token(&mut self, name: &str, access: i32) {
        let access = Access::ALL
            .get(access as usize)
            .copied()
            .unwrap_or(Access::Control);
        let Ok(_lock) = self.lock_remote() else {
            return;
        };
        self.reload_tokens();
        match self.remote.create_token(name, access) {
            Ok(token) => {
                self.revoked = None;
                let name = self
                    .remote
                    .tokens
                    .last()
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                self.new_token = Some((name, token));
                self.save_remote();
                self.tokens_changed();
            }
            Err(why) => self.remote_note = why,
        }
    }

    pub fn revoke_token(&mut self, i: i32) {
        // The row the user picked, by name: the list may have changed on disk since it
        // was shown.
        let Some(name) = usize::try_from(i)
            .ok()
            .and_then(|i| self.remote.tokens.get(i))
            .map(|t| t.name.clone())
        else {
            return;
        };
        let Ok(_lock) = self.lock_remote() else {
            return;
        };
        self.reload_tokens();
        let Some(i) = self.remote.index_of(&name) else {
            return;
        };
        if let Some(entry) = self.remote.revoke(i) {
            if self
                .new_token
                .as_ref()
                .is_some_and(|(n, _)| *n == entry.name)
            {
                self.new_token = None;
            }
            self.revoked = Some((i, entry));
            self.save_remote();
            self.tokens_changed();
        }
    }

    /// Puts the token revoked last back: apps can connect with it again.
    pub fn undo_revoke(&mut self) {
        let Some((i, entry)) = self.revoked.take() else {
            return;
        };
        let Ok(_lock) = self.lock_remote() else {
            return;
        };
        self.reload_tokens();
        match self.remote.restore(i, entry) {
            Ok(()) => {
                self.save_remote();
                self.tokens_changed();
            }
            Err(why) => self.remote_note = why,
        }
    }

    /// The Settings window opened: it shows the access tokens as saved now, including
    /// changes `rbs-cli token` made while the app ran.
    pub fn settings_opened(&mut self) {
        // Under the lock, so an older list read here never replaces in the server one
        // the control thread saved meanwhile (`tokens.*` over IPC). While another
        // program holds the lock, only the list shown is updated: that program's
        // change reaches the server its own way, or with the next change made here.
        match remote_profile::try_lock(&self.config) {
            Ok(None) => self.read_tokens(false),
            Ok(Some(_lock)) => self.read_tokens(true),
            Err(_) => self.read_tokens(true),
        }
    }

    /// Holds the settings file while this window reads, changes and saves it, so a
    /// change `rbs-cli token` makes at the same time is not lost. `Ok(None)` (logged)
    /// when the lock file cannot be used; the change is still made.
    /// The window never waits for the lock: while `rbs-cli` holds it, the change is
    /// not made and the section says why (`Busy`).
    fn lock_remote(&mut self) -> Result<Option<remote_profile::SettingsLock>, Busy> {
        match remote_profile::try_lock(&self.config) {
            Ok(Some(lock)) => Ok(Some(lock)),
            Ok(None) => {
                self.remote_note =
                    "Another program (rbs-cli) is changing the remote control settings. Try again."
                        .into();
                Err(Busy)
            }
            Err(e) => {
                log(&format!("remote control settings not locked: {e}"));
                Ok(None)
            }
        }
    }

    /// Takes the access tokens as saved now: another program (`rbs-cli token`) may have
    /// added or revoked some since the file was read. Called before every change made
    /// here, so saving does not undo theirs, and a token revoked there stops working
    /// (its apps are disconnected). A file that cannot be used now is left alone and
    /// the tokens held are kept; the next change writes them back.
    fn reload_tokens(&mut self) {
        self.read_tokens(true);
    }

    /// [`Self::reload_tokens`]; with `to_server` false only the list shown changes.
    fn read_tokens(&mut self, to_server: bool) {
        match remote_profile::read(&self.config) {
            Ok(Some(saved)) => {
                if self.remote.take_tokens(saved) {
                    if self
                        .new_token
                        .as_ref()
                        .is_some_and(|(n, _)| self.remote.index_of(n).is_none())
                    {
                        self.new_token = None;
                    }
                    self.tokens_read(to_server);
                }
            }
            // Deleted: no tokens (the next change writes the file again).
            Ok(None) => {
                if self.remote.take_tokens(RemoteProfile::new()) {
                    self.new_token = None;
                    self.tokens_read(to_server);
                }
            }
            Err(why) => {
                log(&format!("remote control settings not read again: {why}"));
            }
        }
    }

    /// The Settings window closed: a token shown once is not shown again, and the
    /// revoke can no longer be undone there.
    pub fn settings_closed(&mut self) {
        self.commit_server();
        self.new_token = None;
        self.revoked = None;
        self.monitor_page_shown(false);
    }

    pub fn dismiss_new_token(&mut self) {
        self.new_token = None;
    }

    /// The token list changed: the server accepts the new set (apps using a removed
    /// token are disconnected) and the list shows it.
    fn tokens_read(&mut self, to_server: bool) {
        if to_server {
            self.tokens_changed();
        } else {
            self.refresh_tokens();
        }
    }

    fn tokens_changed(&mut self) {
        if let Some(server) = &self.remote_server {
            server.set_tokens(self.remote.store());
        }
        self.refresh_tokens();
    }

    /// Fills the token list from the settings.
    pub fn refresh_tokens(&self) {
        let rows = self
            .remote
            .tokens
            .iter()
            .map(|t| TokenRow {
                name: t.name.as_str().into(),
                access: Access::describe(t.scopes).into(),
            })
            .collect();
        set_rows(&self.tokens, rows);
    }

    fn save_remote(&mut self) {
        self.remote_note = match remote_profile::save(&self.config, &self.remote) {
            Ok(()) => String::new(),
            Err(e) => format!("The remote control settings could not be saved: {e}"),
        };
    }

    pub fn send(&self, c: Command) {
        if let Some(h) = &self.host {
            h.send(c);
        }
    }

    /// The source kinds this system offers: `(code, label)`, code being the index in
    /// `kind_list` (the Add source menu's).
    pub fn kind_codes(&self) -> Vec<(i32, &'static str)> {
        self.kind_list
            .iter()
            .enumerate()
            .map(|(index, kind)| (index as i32, kind.label()))
            .collect()
    }

    /// Add source menu rows: `(code, api kind id, label)`.
    pub fn add_source_kinds(&self) -> Vec<(i32, &'static str, &'static str)> {
        self.kind_list
            .iter()
            .enumerate()
            .map(|(index, kind)| (index as i32, kind.api_id(), kind.label()))
            .collect()
    }

    /// Index in `kind_list` for a `source.add?kind=` value.
    pub fn kind_code_for_api(&self, api_id: &str) -> Option<i32> {
        self.kind_list
            .iter()
            .position(|kind| kind.api_id() == api_id)
            .map(|index| index as i32)
    }

    /// The kind at a menu code, when it is still installed.
    pub fn kind_at(&self, code: i32) -> Option<SourceKind> {
        self.kind_list.get(code as usize).copied()
    }

    /// Whether another scene has a source the scene on air does not (Add source's
    /// "Existing source").
    pub fn has_existing_sources(&self) -> bool {
        !self.existing.is_empty()
    }

    pub fn list_targets(&mut self, code: i32) {
        let Some(kind) = self.kind_list.get(code as usize).copied() else {
            return;
        };
        self.send(Command::ListTargets(kind));
    }

    /// The Source tab opened for a source: its kind's targets are listed again.
    pub fn list_source_targets(&mut self, id: i32) {
        let id = SourceId::from_raw(id as u32);
        let Some(kind) = self.rows.iter().find(|r| r.id == id).and_then(|r| r.kind) else {
            return;
        };
        self.source_targets_for = Some(id);
        self.source_query.clear();
        self.source_pictures.clear();
        self.send(Command::StopThumbnails);
        self.source_targets.set_vec(Vec::new());
        self.send(Command::ListTargets(kind));
    }

    /// The search field of the Source tab changed.
    pub fn filter_source_targets(&mut self, query: &str) {
        self.source_query = query.trim().to_lowercase();
        self.refresh_source_targets();
    }

    /// Makes the Source tab's list from the last answer: no pickers (an OS picker
    /// needs the add flow), the source's current target first, filtered by the search.
    fn refresh_source_targets(&mut self) {
        let current = self
            .source_targets_for
            .and_then(|id| self.rows.iter().find(|r| r.id == id))
            .map(|r| r.target.clone())
            .unwrap_or_default();
        let rows = source_target_rows(&self.source_target_list, &current, &self.source_query);
        let kind = self
            .source_targets_for
            .and_then(|id| self.rows.iter().find(|r| r.id == id))
            .and_then(|r| r.kind);
        let pictured = matches!(kind, Some(SourceKind::Screen | SourceKind::Window));
        let data = rows
            .iter()
            .map(|r| {
                let t = &self.source_target_list[r.index];
                let picture = self
                    .source_pictures
                    .get(&(t.adapter.clone(), t.target.clone()));
                SourceTargetData {
                    name: t.name.as_str().into(),
                    current: r.current,
                    index: r.index as i32,
                    heading: r.heading.clone().into(),
                    has_picture: picture.is_some(),
                    picture: picture.cloned().unwrap_or_default(),
                    wants_picture: pictured,
                }
            })
            .collect::<Vec<_>>();
        self.source_targets.set_vec(data);
        // Pictures are asked for the rows shown that have none yet, a few at a time
        // (ADR-0077): the grabs are the host's, off the control thread.
        if let (true, Some(kind)) = (pictured, kind) {
            let asked: Vec<(String, String)> = rows
                .iter()
                .map(|r| &self.source_target_list[r.index])
                .filter(|t| {
                    !self
                        .source_pictures
                        .contains_key(&(t.adapter.clone(), t.target.clone()))
                })
                .take(THUMBS_PER_LIST)
                .map(|t| (t.adapter.clone(), t.target.clone()))
                .collect();
            if !asked.is_empty() {
                self.send(Command::GrabThumbnails {
                    kind,
                    rows: asked,
                    max: (THUMB_REQUEST.0, THUMB_REQUEST.1),
                });
            }
        }
    }

    /// One target's picture arrived for the Source tab's list.
    fn source_picture(
        &mut self,
        adapter: String,
        target: String,
        picture: Option<rbs_capture::Thumbnail>,
    ) {
        let Some(p) = picture else { return };
        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            p.rgba(),
            p.width(),
            p.height(),
        );
        self.source_pictures
            .insert((adapter, target), slint::Image::from_rgba8(buffer));
        let list = &self.source_target_list;
        let pictures = &self.source_pictures;
        for i in 0..self.source_targets.row_count() {
            let Some(mut row) = self.source_targets.row_data(i) else {
                continue;
            };
            let Some(t) = list.get(row.index as usize) else {
                continue;
            };
            let img = pictures.get(&(t.adapter.clone(), t.target.clone()));
            if let (Some(img), false) = (img, row.has_picture) {
                row.has_picture = true;
                row.picture = img.clone();
                self.source_targets.set_row_data(i, row);
            }
        }
    }

    /// The Source settings window opens for a source: what it shows is asked for.
    pub fn request_source_settings(&mut self, id: i32) {
        self.source_settings.set_vec(Vec::new());
        self.source_settings_view = None;
        self.send(Command::OpenSourceSettings(SourceId::from_raw(id as u32)));
    }

    /// The Source settings window was closed: its content is dropped.
    pub fn source_settings_closed(&mut self) {
        self.source_settings.set_vec(Vec::new());
        self.source_settings_view = None;
        self.send(Command::CloseSourceSettings);
    }

    /// Apply or OK in the Source settings window: the chosen mode, rate and cursor are
    /// sent as one patch (one undo step).
    pub fn apply_source_settings(&mut self, id: i32, mode: i32, rate: i32, cursor: bool) {
        let Some(choices) = &self.source_settings_view else {
            return;
        };
        let patch = choices.patch(mode, rate, cursor);
        if patch.is_empty() {
            return;
        }
        self.send(Command::SetSourceSettings {
            id: SourceId::from_raw(id as u32),
            patch,
        });
    }

    /// A target of the Source tab's list was chosen: the source points at it.
    pub fn retarget_source(&mut self, id: i32, index: i32) {
        let Some(target) = self.source_target_list.get(index as usize).cloned() else {
            return;
        };
        self.send(Command::RetargetSource {
            id: SourceId::from_raw(id as u32),
            target,
        });
    }

    /// Reload on the Source tab: a source that is not running is tried again.
    pub fn reload_source(&mut self, id: i32) {
        self.send(Command::ReloadSource(SourceId::from_raw(id as u32)));
    }

    /// Adds or removes the "Capture audio too" companion of a display or window.
    pub fn set_companion_audio(&mut self, id: i32, on: bool) {
        self.send(Command::SetCompanionAudio {
            source: SourceId::from_raw(id as u32),
            on,
        });
    }

    pub fn delete_source(&mut self, id: i32) {
        self.deleted.clear();
        self.send(Command::DeleteSource(SourceId::from_raw(id as u32)));
    }

    /// Remove from scene (ADR-0065): the Sources row's remove button and Delete key.
    pub fn remove_from_scene(&mut self, id: i32) {
        self.deleted.clear();
        self.send(Command::RemoveFromScene(SourceId::from_raw(id as u32)));
    }

    /// Show in every scene (Properties' Toggle, ADR-0065).
    pub fn set_global(&mut self, id: i32, global: bool) {
        self.deleted.clear();
        self.send(Command::SetGlobal {
            id: SourceId::from_raw(id as u32),
            global,
        });
    }

    /// Settings › Global sources' Remove.
    pub fn remove_global(&mut self, id: i32) {
        self.deleted.clear();
        self.send(Command::RemoveGlobal(SourceId::from_raw(id as u32)));
    }

    /// Settings › Global sources: a kind was picked; its devices are listed.
    pub fn pick_global_kind(&mut self, index: i32) {
        let Some((code, _)) = self.kind_codes().get(index as usize).copied() else {
            return;
        };
        self.global_kind = index;
        self.global_option = -1;
        self.global_targets.clear();
        self.refresh_global_existing();
        self.list_targets(code);
    }

    /// The kind picked in Settings › Global sources.
    fn global_source_kind(&self) -> Option<SourceKind> {
        self.kind_list.get(self.global_kind as usize).copied()
    }

    /// The collection's sources of the picked kind that are not global.
    fn refresh_global_existing(&mut self) {
        let kind = self.global_source_kind();
        self.global_existing = match (&self.scene_list, kind) {
            (Some(list), Some(kind)) => list
                .sources
                .iter()
                .filter(|s| s.kind == Some(kind) && !s.global)
                .map(|s| s.id)
                .collect(),
            _ => Vec::new(),
        };
        if self.global_option >= self.global_options().len() as i32 {
            self.global_option = -1;
        }
    }

    /// The Source field's options: devices, then sources the scenes have.
    fn global_options(&self) -> Vec<String> {
        let names = self.scene_list.as_ref();
        self.global_targets
            .iter()
            .map(|t| t.name.clone())
            .chain(self.global_existing.iter().map(|id| {
                let name = names
                    .and_then(|l| l.sources.iter().find(|s| s.id == *id))
                    .map_or_else(String::new, |s| s.name.clone());
                format!("{name} (in scenes)")
            }))
            .collect()
    }

    /// Settings › Global sources' Add: a device becomes a new global source, a source
    /// the scenes have becomes global (it leaves them).
    pub fn add_global(&mut self, option: i32) {
        let Ok(i) = usize::try_from(option) else {
            return;
        };
        self.deleted.clear();
        if let Some(target) = self.global_targets.get(i).cloned() {
            if let Some(kind) = self.global_source_kind() {
                self.send(Command::AddGlobalSource { kind, target });
            }
        } else if let Some(id) = self.global_existing.get(i - self.global_targets.len()) {
            self.send(Command::SetGlobal {
                id: *id,
                global: true,
            });
        }
        self.global_option = -1;
    }

    /// Lock or unlock a source on the picture panel (the Sources row's lock button).
    pub fn set_source_locked(&mut self, id: i32, locked: bool) {
        self.deleted.clear();
        self.send(Command::SetSourceLocked {
            id: SourceId::from_raw(id as u32),
            locked,
        });
    }

    /// Flip, turn, fit or centre a source's picture (ADR-0084).
    pub fn transform_source(&mut self, id: i32, op: TransformOp) {
        self.deleted.clear();
        self.send(Command::TransformSource {
            id: SourceId::from_raw(id as u32),
            op,
            gesture: 0,
        });
    }

    /// Sets the picture's rotation in degrees clockwise. Moves that share `gesture`
    /// are one undo step.
    pub fn set_source_rotation(&mut self, id: i32, degrees: f32, gesture: i32) {
        self.deleted.clear();
        self.send(Command::TransformSource {
            id: SourceId::from_raw(id as u32),
            op: TransformOp::SetAngle(degrees),
            gesture: gesture.max(0) as u64,
        });
    }

    /// Mute or unmute a source's sound (the Mixer row's speaker, Properties' Mute).
    pub fn set_source_muted(&mut self, id: i32, muted: bool) {
        self.send(Command::SetSourceMuted {
            id: SourceId::from_raw(id as u32),
            muted,
        });
    }

    /// How many heard sources the Mixer shows, how many of those are muted, and how
    /// many it hides (ADR-0080), for its menu.
    pub fn mixer_counts(&self) -> (usize, usize, usize) {
        let hidden = self.heard.iter().filter(|h| h.hidden).count();
        let muted = self.heard.iter().filter(|h| !h.hidden && h.muted).count();
        (self.heard.len() - hidden, muted, hidden)
    }

    /// Hide every shown source (`only_muted`: the muted ones) from the Mixer, or show
    /// every hidden one (`hide` false). One command each; they stay audible.
    pub fn hide_in_mixer(&mut self, hide: bool, only_muted: bool) {
        let ids: Vec<u32> = self
            .heard
            .iter()
            .filter(|h| h.hidden != hide && (!hide || !only_muted || h.muted))
            .map(|h| h.id)
            .collect();
        for id in ids {
            self.set_source_mixer_hidden(id as i32, hide);
        }
    }

    /// Hide a source from the Mixer or show it again (ADR-0080); it stays audible.
    pub fn set_source_mixer_hidden(&mut self, id: i32, hidden: bool) {
        self.send(Command::SetSourceMixerHidden {
            id: SourceId::from_raw(id as u32),
            hidden,
        });
    }

    /// A source's volume from the fader position (0 to 1, ADR-0059 fader law). Each
    /// pointer move sends one change; the host sends it on to the mixer at once.
    pub fn set_source_volume(&mut self, id: i32, position: f32) {
        self.send(Command::SetSourceVolume {
            id: SourceId::from_raw(id as u32),
            volume: fader::volume_at(position),
        });
    }

    pub fn rename_source(&mut self, id: i32, name: &str) {
        self.deleted.clear();
        self.send(Command::RenameSource {
            id: SourceId::from_raw(id as u32),
            name: name.to_owned(),
        });
    }

    /// Collection glide defaults (ADR-0100).
    pub fn set_glide(&mut self, duration_ms: i32, easing: i32) {
        self.send(Command::SetGlide {
            duration_ms: duration_ms.clamp(0, 5_000) as u32,
            easing: if easing == 0 { 0 } else { 1 },
        });
    }

    /// Per-source slide override (ADR-0100). `custom` false clears it.
    pub fn set_source_glide(&mut self, id: i32, custom: bool, duration_ms: i32, easing: i32) {
        self.send(Command::SetSourceGlide {
            id: SourceId::from_raw(id as u32),
            custom,
            duration_ms: duration_ms.clamp(0, 5_000) as u32,
            easing: if easing == 0 { 0 } else { 1 },
        });
    }

    /// Places a source of another scene into the scene on air (Add source, Existing
    /// source); it is selected once it is there.
    pub fn add_existing_source(&mut self, id: i32) {
        self.deleted.clear();
        self.adding = true;
        self.send(Command::AddExistingSource(SourceId::from_raw(id as u32)));
    }

    /// Opens the Add source picker for a source type (its devices are listed by the
    /// studio and fill the rows when they arrive) or for the sources of other scenes.
    /// A picker that is open is replaced.
    pub fn open_add_picker(&mut self, what: crate::menu_bar::AddWhat) {
        use crate::menu_bar::AddWhat;
        let picker = match what {
            AddWhat::Kind(code) => {
                let Some(kind) = self.kind_list.get(code as usize).copied() else {
                    return;
                };
                self.send(Command::ListTargets(kind));
                add_picker::AddPicker::for_kind(kind)
            }
            AddWhat::Existing => add_picker::AddPicker::existing(&self.existing),
        };
        self.add_picker = Some(picker);
        self.picker_dirty = true;
    }

    /// The picker was closed or cancelled: a late answer fills nothing.
    pub fn close_add_picker(&mut self) {
        self.add_picker = None;
        self.picker_dirty = false;
    }

    /// "Capture audio too" in the Add source dialog was checked or unchecked.
    pub fn set_picker_audio(&mut self, on: bool) {
        if let Some(p) = &mut self.add_picker {
            p.set_audio(on);
            self.picker_dirty = true;
        }
    }

    pub fn filter_add_picker(&mut self, text: &str) {
        if let Some(p) = &mut self.add_picker {
            p.set_query(text);
            self.picker_dirty = true;
        }
    }

    /// A row was chosen (`index` is of the picker's listing, see [`add_picker::Row`]).
    /// Returns whether something was added, so the caller closes the dialog.
    pub fn choose_add_picker(&mut self, index: usize) -> bool {
        let pick = self
            .add_picker
            .as_ref()
            .and_then(|p| p.pick(index))
            .cloned();
        match pick {
            Some(add_picker::Pick::Target(kind, target)) => {
                let audio = self.add_picker.as_ref().is_some_and(|p| p.audio());
                self.adding = true;
                self.deleted.clear();
                // A camera without a microphone of its own adds just the camera.
                let with_sound =
                    audio && !target.picker && (kind != SourceKind::Camera || target.mic.is_some());
                if with_sound {
                    self.send(Command::AddSourceWithAudio { kind, target });
                } else {
                    self.send(Command::AddSource { kind, target });
                }
            }
            Some(add_picker::Pick::Existing(id)) => self.add_existing_source(id.get() as i32),
            None => return false,
        }
        self.add_picker = None;
        true
    }

    /// What the dialog shows now, once after each change.
    pub fn take_picker_view(&mut self) -> Option<PickerView> {
        if !std::mem::take(&mut self.picker_dirty) {
            return None;
        }
        let p = self.add_picker.as_ref()?;
        let what = p
            .kind()
            .map_or("existing source".to_owned(), |k| k.label().to_lowercase());
        Some(PickerView {
            title: format!("Add {what}"),
            status: p.status(),
            rows: p.rows(),
            offers_audio: p.offers_audio(),
            offers_mic: p.offers_microphone(),
            audio: p.audio(),
        })
    }

    /// Puts a scene on air (a click in the Scenes panel); in studio mode, in preview.
    pub fn switch_scene(&mut self, id: i32) {
        self.send(Command::SwitchScene(SceneId::from_raw(id as u32)));
    }

    /// Turns studio mode on or off (ADR-0061).
    pub fn set_studio_mode(&mut self, on: bool) {
        self.send(Command::SetStudioMode(on));
    }

    /// The transition: the preview scene goes on air (ADR-0061).
    pub fn transition(&mut self) {
        if self.studio_mode {
            self.send(Command::Transition);
        }
    }

    pub fn add_scene(&mut self) {
        self.deleted.clear();
        self.send(Command::AddScene(None));
    }

    /// Copies a scene; it does not go on air.
    pub fn duplicate_scene(&mut self, id: i32) {
        self.deleted.clear();
        self.send(Command::DuplicateScene(SceneId::from_raw(id as u32)));
    }

    pub fn remove_scene(&mut self, id: i32) {
        self.deleted.clear();
        self.send(Command::RemoveScene(SceneId::from_raw(id as u32)));
    }

    pub fn rename_scene(&mut self, id: i32, name: &str) {
        self.deleted.clear();
        self.send(Command::RenameScene {
            id: SceneId::from_raw(id as u32),
            name: name.to_owned(),
        });
    }

    /// The sources Add source can place again: those of the collection the scene on
    /// air does not have yet.
    fn refresh_existing(&mut self) {
        let Some(list) = &self.scene_list else {
            return;
        };
        self.existing = list
            .sources
            .iter()
            .filter(|s| !s.global && !self.rows.iter().any(|r| r.id == s.id))
            .map(|s| (s.id, s.name.clone()))
            .collect();
    }

    /// One layer up (drawn over the source above it in the list) or down.
    /// Show or hide a source in the program scene (the Sources row's eye button).
    pub fn set_source_visible(&mut self, id: i32, visible: bool) {
        self.deleted.clear();
        self.send(Command::SetSourceVisible {
            id: SourceId::from_raw(id as u32),
            visible,
        });
    }

    pub fn move_scene(&mut self, id: i32, direction: rbs_api::request::LayerDirection) {
        self.deleted.clear();
        self.send(Command::MoveScene {
            id: SceneId::from_raw(id as u32),
            direction,
        });
    }

    /// Puts scene `id` where scene `to` is in the scene list (a row dragged onto
    /// another).
    pub fn move_scene_to(&mut self, id: i32, to: i32) {
        self.deleted.clear();
        self.send(Command::MoveSceneTo {
            id: SceneId::from_raw(id as u32),
            to: SceneId::from_raw(to as u32),
        });
    }

    /// Keeps a source's device open, or releases it. Hiding does not call this.
    pub fn set_source_active(&mut self, id: i32, active: bool) {
        self.deleted.clear();
        self.send(Command::SetSourceActive {
            id: SourceId::from_raw(id as u32),
            active,
        });
    }

    pub fn move_source(&mut self, id: i32, direction: rbs_api::request::LayerDirection) {
        self.deleted.clear();
        self.send(Command::MoveSource {
            id: SourceId::from_raw(id as u32),
            direction,
        });
    }

    /// Puts source `id` where source `to` is in the layer order (a row dragged onto
    /// another).
    pub fn move_source_to(&mut self, id: i32, to: i32) {
        self.deleted.clear();
        self.send(Command::MoveSourceTo {
            id: SourceId::from_raw(id as u32),
            to: SourceId::from_raw(to as u32),
        });
    }

    /// A video source's box on the canvas, in canvas pixels (the Properties fields;
    /// the picture panel's drags go through [`PreviewEdit`], numbered).
    pub fn set_source_box(&mut self, id: i32, x: f32, y: f32, w: f32, h: f32) {
        self.deleted.clear();
        self.send_box(0, id, SourceBox::new([x, y], [w, h]), 0);
    }

    /// Source pixels cut from each edge of a video source's picture (the Properties
    /// fields, ADR-0058); the box changes so the picture keeps its scale.
    pub fn set_source_crop(&mut self, id: i32, crop: [i32; 4]) {
        if self.host.is_none() {
            return;
        }
        self.deleted.clear();
        self.send(Command::SetSourceCrop {
            seq: 0,
            gesture: 0,
            id: SourceId::from_raw(id as u32),
            crop: crop.map(|v| v.max(0) as u32),
            keep_scale: true,
        });
    }

    /// Shows a part of a video source's picture (Properties' Show buttons): 0 the whole
    /// picture, 1 canvas size centred, 2 left half, 3 right half.
    pub fn set_source_region(&mut self, id: i32, region: i32) {
        let region = match region {
            0 => CropRegion::Full,
            1 => CropRegion::CanvasCentred,
            2 => CropRegion::LeftHalf,
            3 => CropRegion::RightHalf,
            _ => return,
        };
        if self.host.is_none() {
            return;
        }
        self.deleted.clear();
        self.send(Command::SetSourceRegion {
            id: SourceId::from_raw(id as u32),
            region,
        });
    }

    /// Capture region on the source (ADR-0103). `clear` removes the region.
    pub fn set_source_capture_region(
        &mut self,
        id: i32,
        clear: bool,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    ) {
        if self.host.is_none() {
            return;
        }
        let region = if clear {
            None
        } else {
            Some(SourceRegion::new(
                left.max(0) as u32,
                top.max(0) as u32,
                right.max(0) as u32,
                bottom.max(0) as u32,
            ))
        };
        self.send(Command::SetSourceCaptureRegion {
            id: SourceId::from_raw(id as u32),
            region,
        });
    }

    /// Display chrome (ADR-0104). `on` false turns chrome off.
    #[allow(clippy::too_many_arguments)] // One argument per field of the chrome form.
    pub fn set_display_chrome(
        &mut self,
        id: i32,
        on: bool,
        top: i32,
        right: i32,
        bottom: i32,
        left: i32,
        fill: i32,
    ) {
        if self.host.is_none() {
            return;
        }
        let chrome = if on {
            let fill = if fill == 1 {
                ChromeFill::Fit
            } else {
                ChromeFill::Cover
            };
            Some(
                DisplayChrome::new(
                    top.max(0) as u32,
                    right.max(0) as u32,
                    bottom.max(0) as u32,
                    left.max(0) as u32,
                )
                .with_fill(fill),
            )
        } else {
            None
        };
        self.send(Command::SetDisplayChrome {
            id: SourceId::from_raw(id as u32),
            chrome,
        });
    }

    pub fn fit_display_chrome(&mut self, id: i32) {
        if self.host.is_none() {
            return;
        }
        self.send(Command::FitDisplayChrome(SourceId::from_raw(id as u32)));
    }

    pub fn reset_display_chrome(&mut self, id: i32) {
        if self.host.is_none() {
            return;
        }
        self.send(Command::ResetDisplayChrome(SourceId::from_raw(id as u32)));
    }

    /// Stored timed-show duration on the item (ADR-0105). `show_for_ms` 0 clears.
    pub fn set_item_show_for(&mut self, item: i32, show_for_ms: i32) {
        if self.host.is_none() {
            return;
        }
        let show_for_ms = if show_for_ms <= 0 {
            None
        } else {
            Some((show_for_ms as u32).min(rbs_scene::MAX_SHOW_FOR_MS))
        };
        self.send(Command::SetItemShowFor {
            item: ItemId::from_raw(item as u32),
            show_for_ms,
        });
    }

    /// Live timed show for a hidden item (ADR-0105).
    pub fn show_item(&mut self, item: i32) {
        if self.host.is_none() {
            return;
        }
        self.send(Command::ShowItem(ItemId::from_raw(item as u32)));
    }

    /// Shape and border of a video source's picture (the Properties fields). `shape`:
    /// 0 rectangle, 1 circle, 2 ellipse, 3 triangle, 4 arrow. A colour that is not
    /// `#rrggbb` or `#rrggbbaa` changes nothing.
    pub fn set_source_style(
        &mut self,
        id: i32,
        shape: i32,
        radius: f32,
        border: bool,
        width: f32,
        color: &str,
    ) {
        let Some(color) = rbs_api::types::parse_color(color.trim()) else {
            return;
        };
        let shape = match shape {
            1 => Shape::Circle,
            2 => Shape::Ellipse,
            3 => Shape::Triangle,
            4 => Shape::Arrow,
            _ => Shape::Rectangle { radius },
        };
        // The shadow is not in the panel yet; keep one an API client set.
        let shadow = self.rows_style(id).and_then(|s| s.shadow);
        let style = ItemStyle::new(shape)
            .with_border(border.then_some(Border::new(width, color)))
            .with_shadow(shadow);
        self.deleted.clear();
        self.send(Command::SetSourceStyle {
            gesture: 0,
            id: SourceId::from_raw(id as u32),
            style,
        });
    }

    fn rows_style(&self, id: i32) -> Option<ItemStyle> {
        self.rows
            .iter()
            .find(|r| r.id.get() as i32 == id)
            .and_then(|r| r.layout.map(|l| l.style))
    }

    fn send_box(&mut self, seq: u64, id: i32, b: SourceBox, gesture: u64) {
        if self.host.is_none() {
            // Nothing will answer: forget what the picture panel has on its way.
            self.edit.reset();
            return;
        }
        let (position, size) = (b.position, b.size);
        self.send(Command::SetSourceBox {
            seq,
            gesture,
            id: SourceId::from_raw(id as u32),
            position,
            size,
        });
    }

    /// The video sources as the picture panel places them, bottom first.
    fn placed(&self) -> Vec<Placed> {
        self.placed_where(|d| !d.locked)
    }

    /// The source whose picture is under `at` on the picture panel of size `size`,
    /// locked ones included (a right-click there offers Unlock); it becomes the
    /// selection. `None` (and no selection) where no picture is.
    pub fn preview_source_at(&mut self, at: [f32; 2], size: [f32; 2]) -> Option<i32> {
        self.edit.set_panel(size[0], size[1]);
        let placed = self.placed_where(|_| true);
        let hit = self.edit.source_at(at, self.canvas_px(), &placed);
        self.selected = hit.unwrap_or(-1);
        self.refresh_marks();
        hit
    }

    fn placed_where(&self, keep: impl Fn(&SourceData) -> bool) -> Vec<Placed> {
        let mut v: Vec<Placed> = self
            .sources
            .iter()
            // Locked sources are not picked or moved on the picture panel (ADR-0059).
            .filter(|d| d.picture && keep(d))
            .map(|d| Placed {
                id: d.id,
                at: SourceBox::new([d.x, d.y], [d.w, d.h]),
                crop: [d.crop_l, d.crop_t, d.crop_r, d.crop_b].map(|v| v.max(0) as u32),
                picture: (d.pic_w > 0 && d.pic_h > 0).then_some((d.pic_w as u32, d.pic_h as u32)),
            })
            .collect();
        v.reverse();
        v
    }

    fn canvas_px(&self) -> [f32; 2] {
        [self.canvas.width as f32, self.canvas.height as f32]
    }

    fn run(&mut self, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::Select(id) => self.selected = id,
            Effect::Send(seq, id, b, gesture) => {
                self.deleted.clear();
                self.send_box(seq, id, b, gesture);
            }
            Effect::SendCrop(seq, id, crop, gesture) => {
                self.deleted.clear();
                if self.host.is_none() {
                    self.edit.reset();
                } else {
                    self.send(Command::SetSourceCrop {
                        seq,
                        gesture,
                        id: SourceId::from_raw(id as u32),
                        crop,
                        keep_scale: true,
                    });
                }
            }
        }
        self.refresh_marks();
    }

    /// A press on the picture panel (panel coordinates, its size `size`).
    pub fn preview_press(
        &mut self,
        at: [f32; 2],
        size: [f32; 2],
        shift: bool,
        no_snap: bool,
        crop: bool,
    ) {
        self.edit.set_panel(size[0], size[1]);
        let keys = DragKeys {
            free: shift,
            no_snap,
            crop,
        };
        let placed = self.placed();
        let e = self
            .edit
            .press(at, self.canvas_px(), &placed, self.selected, keys);
        self.run(e);
    }

    pub fn preview_drag(&mut self, at: [f32; 2]) {
        let e = self.edit.drag_to(at, self.canvas_px());
        self.run(e);
    }

    pub fn preview_release(&mut self) {
        self.edit.release();
        self.refresh_marks();
    }

    pub fn preview_nudge(&mut self, dx: f32, dy: f32) {
        let placed = self.placed();
        let e = self.edit.nudge(&placed, self.selected, dx, dy);
        self.run(e);
    }

    /// The picture panel's size in the main window, from the layout.
    pub fn set_picture_size(&mut self, w: f32, h: f32) {
        self.edit.set_panel(w, h);
        self.refresh_marks();
    }

    /// The selection as marks in the picture panel's coordinates.
    #[cfg_attr(
        not(any(target_os = "macos", windows)),
        allow(
            dead_code,
            reason = "no preview layer here; the toolkit draws the marks"
        )
    )]
    pub fn marks(&self) -> &[Mark] {
        &self.mark_buf
    }

    /// Works out the selection's marks again (selection, rows, drag or panel changed).
    pub fn refresh_marks(&mut self) {
        let placed = self.placed();
        let mut buf = std::mem::take(&mut self.mark_buf);
        self.edit
            .marks(self.canvas_px(), &placed, self.selected, &mut buf);
        let data: Vec<MarkData> = buf
            .iter()
            .map(|m| MarkData {
                x: m.rect.x,
                y: m.rect.y,
                w: m.rect.w,
                h: m.rect.h,
                kind: match m.kind {
                    MarkKind::Outline => 0,
                    MarkKind::HandleBorder => 1,
                    MarkKind::HandleFill => 2,
                },
            })
            .collect();
        self.mark_buf = buf;
        set_rows(&self.marks, data);
    }

    pub fn undo_delete(&mut self) {
        self.deleted.clear();
        self.send(Command::UndoDelete);
    }

    /// Undo (`back`) or redo the newest scene edit (Ctrl/Cmd+Z, the Undo and Redo
    /// buttons). A box on its way is still applied first: the studio takes commands
    /// in order.
    pub fn undo_edit(&mut self, back: bool) {
        self.deleted.clear();
        self.send(if back { Command::Undo } else { Command::Redo });
    }

    /// The encoder settings of an [`output`].
    fn encoding_of(&self, out: i32) -> Encoding {
        if out == output::STREAM {
            self.stream_form().encoding.clone()
        } else {
            self.record_form().encoding()
        }
    }

    /// The recording settings the Recording section shows: those of the picked canvas
    /// (the vertical one uses the main ones until its first change).
    fn record_form(&self) -> &RecordProfile {
        match self.settings_canvas {
            OutputCanvas::Main => &self.profile,
            OutputCanvas::Vertical => self.vertical_profile.as_ref().unwrap_or(&self.profile),
        }
    }

    /// The recording settings `c` records with.
    fn record_form_for(&self, c: OutputCanvas) -> &RecordProfile {
        match c {
            OutputCanvas::Main => &self.profile,
            OutputCanvas::Vertical => self.vertical_profile.as_ref().unwrap_or(&self.profile),
        }
    }

    /// The recording settings the Recording section changes; the vertical canvas's
    /// first change starts from a copy of the main ones.
    fn record_form_mut(&mut self) -> &mut RecordProfile {
        match self.settings_canvas {
            OutputCanvas::Main => &mut self.profile,
            OutputCanvas::Vertical => self
                .vertical_profile
                .get_or_insert_with(|| vertical_recording_from_main(&self.profile)),
        }
    }

    /// The stream settings the Stream section shows: those of the picked canvas.
    fn stream_form(&self) -> &StreamProfile {
        match self.settings_canvas {
            OutputCanvas::Main => &self.stream,
            OutputCanvas::Vertical => self.vertical_stream.as_ref().unwrap_or(&self.no_stream),
        }
    }

    /// The stream settings the Stream section changes.
    fn stream_form_mut(&mut self) -> &mut StreamProfile {
        match self.settings_canvas {
            OutputCanvas::Main => &mut self.stream,
            OutputCanvas::Vertical => self.vertical_stream.get_or_insert_with(StreamProfile::new),
        }
    }

    /// The Settings sections show the canvas picked in their selector (0: main,
    /// 1: vertical); the vertical one only while it exists.
    pub fn set_settings_canvas(&mut self, i: i32) {
        self.commit_server();
        self.settings_canvas = match i {
            1 if self.vertical.is_some() => OutputCanvas::Vertical,
            _ => OutputCanvas::Main,
        };
        self.folder_serial += 1;
        self.server_serial += 1;
        self.key_serial += 1;
    }

    /// The picked encoder's place in the list (0: automatic, also when the saved one is
    /// not on this system).
    fn encoder_index(&self, e: &Encoding) -> usize {
        e.encoder
            .as_ref()
            .and_then(|id| self.encoders.iter().position(|x| x.id == *id))
            .map_or(0, |i| i + 1)
    }

    /// The encoder's name in summaries.
    fn encoder_name(&self, e: &Encoding) -> String {
        match self.encoder_index(e) {
            0 => "best encoder".to_owned(),
            i => self
                .encoders
                .get(i - 1)
                .map_or_else(String::new, |x| x.name.clone()),
        }
    }

    /// The picked encoder's adapter id, `None` for automatic.
    fn encoder_id(&self, e: &Encoding) -> Option<String> {
        match self.encoder_index(e) {
            0 => None,
            i => self.encoders.get(i - 1).map(|x| x.id.clone()),
        }
    }

    /// What the picked encoder can do; with automatic choice, what any encoder can do.
    fn options(&self, e: &Encoding) -> EncoderOptions {
        match self.encoder_index(e) {
            0 => EncoderOptions::union(self.encoders.iter().map(|e| &e.options)),
            i => self
                .encoders
                .get(i - 1)
                .map(|e| e.options.clone())
                .unwrap_or_default(),
        }
    }

    /// The encoder fields of an [`output`]'s form, for its settings and encoder.
    fn form(&self, out: i32) -> EncoderForm {
        let e = self.encoding_of(out);
        if out == output::STREAM {
            EncoderForm::for_stream(&e, &self.options(&e))
        } else {
            EncoderForm::new(&e, &self.options(&e))
        }
    }

    /// The video bitrates offered: the presets, and the saved one if it is not one.
    fn video_options(e: &Encoding) -> Vec<u32> {
        let mut v = VIDEO_KBPS.to_vec();
        if !v.contains(&e.video_kbps) {
            v.push(e.video_kbps);
            v.sort_unstable();
        }
        v
    }

    /// Video codecs the settings control offers. A stream stays H.264: RTMP and
    /// MPEG-TS do not carry HEVC or AV1. A recording lists HEVC and AV1 when an
    /// encoder on this computer does, and keeps a saved codec visible.
    fn video_codecs(&self, out: i32, e: &Encoding) -> Vec<VideoCodec> {
        if out == output::STREAM {
            return vec![VideoCodec::H264];
        }
        let mut list = vec![VideoCodec::H264];
        let have = &self.options(e).codecs;
        if have.iter().any(|c| c == "hevc") {
            list.push(VideoCodec::Hevc);
        }
        if have.iter().any(|c| c == "av1") {
            list.push(VideoCodec::Av1);
        }
        if !list.contains(&e.video_codec) {
            list.push(e.video_codec);
        }
        list
    }

    /// The encoded picture of an [`output`]: its stored output size, or this
    /// settings canvas when the size is the same as the canvas.
    fn resolved_output_size(&self, out: i32) -> (u32, u32) {
        let canvas = self.settings_canvas_size();
        let saved = if out == output::STREAM {
            self.stream_form().output_size
        } else {
            self.recording_output_size()
        };
        saved.map(OutputSize::pair).unwrap_or(canvas)
    }

    /// The recommended video bitrate when the chosen CBR rate or VBR target is
    /// below it, otherwise 0 (no warning, including constant quality).
    fn bitrate_warning_kbps(&self, out: i32, e: &Encoding) -> i32 {
        if e.rate_control == RateMode::ConstantQuality {
            return 0;
        }
        let (w, h) = self.resolved_output_size(out);
        let Some(recommended) = recommended_video_kbps(w, h, self.canvas.fps, e.video_codec) else {
            return 0;
        };
        if e.video_kbps < recommended {
            i32::try_from(recommended).unwrap_or(0)
        } else {
            0
        }
    }

    /// What the `EncoderFields` of an [`output`] show.
    fn encoder_data(&self, out: i32) -> EncoderData {
        let e = self.encoding_of(out);
        let form = self.form(out);
        let mut encoders = vec!["Automatic (best available)".to_owned()];
        encoders.extend(self.encoders.iter().map(|e| e.name.clone()));
        let video = Self::video_options(&e);
        let (rate_modes, rate_mode) = choices(&form.rate_modes);
        let (peaks, peak) = choices(&form.peaks);
        let (qualities, quality) = choices(&form.qualities);
        let (keyframes, keyframe) = choices(&form.keyframes);
        let (presets, preset) = choices(&form.presets);
        let (profiles, profile) = choices(&form.profiles);
        let (b_frames, b_frame) = choices(&form.b_frames);
        let codecs = self.video_codecs(out, &e);
        let codec_index = codecs.iter().position(|c| *c == e.video_codec).unwrap_or(0);
        let (out_w, out_h) = self.resolved_output_size(out);
        EncoderData {
            codecs: strings(codecs.iter().map(|c| c.label().to_owned())),
            codec: codec_index as i32,
            encoders: strings(encoders),
            encoder: self.encoder_index(&e) as i32,
            video_rates: strings(video.iter().map(|k| kbps(*k))),
            video_rate: video.iter().position(|k| *k == e.video_kbps).unwrap_or(0) as i32,
            audio_rates: strings(AUDIO_KBPS.iter().map(|k| kbps(*k))),
            audio_rate: AUDIO_KBPS
                .iter()
                .position(|k| *k == e.audio_kbps)
                .unwrap_or(0) as i32,
            rate_modes,
            rate_mode,
            rate_kind: match e.rate_control {
                RateMode::Cbr => 0,
                RateMode::Vbr => 1,
                RateMode::ConstantQuality => 2,
            },
            bitrate_label: form.bitrate_label.into(),
            peaks,
            peak,
            qualities,
            quality,
            padding: e.cbr_padding,
            padding_available: form.padding_available,
            keyframes,
            keyframe,
            presets,
            preset,
            profiles,
            profile,
            b_frames,
            b_frame,
            b_frames_note: form.b_frames_note.unwrap_or_default().into(),
            video_kbps: i32::try_from(e.video_kbps).unwrap_or(0),
            recommended_kbps: self.bitrate_warning_kbps(out, &e),
            out_width: i32::try_from(out_w).unwrap_or(0),
            out_height: i32::try_from(out_h).unwrap_or(0),
            out_fps: i32::try_from(self.canvas.fps).unwrap_or(0),
            delivery: match e.delivery {
                Delivery::Video => 0,
                Delivery::Screen => 1,
            },
            open_shadows: e.open_shadows,
        }
    }

    /// An encoder field of an [`output`] was picked ([`enc_field`] codes); the
    /// setting is saved at once.
    pub fn set_encoding(&mut self, out: i32, field: i32, value: i32) {
        let mut e = self.encoding_of(out);
        let form = self.form(out);
        let changed = match field {
            enc_field::ENCODER => match value {
                0 => {
                    e.encoder = None;
                    true
                }
                i => match self.encoders.get(i as usize - 1) {
                    Some(x) => {
                        e.encoder = Some(x.id.clone());
                        true
                    }
                    None => false,
                },
            },
            enc_field::VIDEO_RATE => Self::video_options(&e)
                .get(value as usize)
                .copied()
                .map(|k| e.video_kbps = k)
                .is_some(),
            enc_field::AUDIO_RATE => AUDIO_KBPS
                .get(value as usize)
                .map(|k| e.audio_kbps = *k)
                .is_some(),
            enc_field::RATE_MODE => form
                .rate_modes
                .get(value)
                .map(|m| e.rate_control = m)
                .is_some(),
            enc_field::PEAK => form
                .peaks
                .get(value)
                .map(|k| e.max_video_kbps = k)
                .is_some(),
            enc_field::QUALITY => form.qualities.get(value).map(|q| e.quality = q).is_some(),
            enc_field::PADDING => {
                e.cbr_padding = value != 0;
                true
            }
            enc_field::KEYFRAME => form
                .keyframes
                .get(value)
                .map(|s| e.keyframe_seconds = s)
                .is_some(),
            enc_field::PRESET => form.presets.get(value).map(|p| e.preset = p).is_some(),
            enc_field::PROFILE => form.profiles.get(value).map(|p| e.profile = p).is_some(),
            enc_field::B_FRAMES => form.b_frames.get(value).map(|n| e.b_frames = n).is_some(),
            enc_field::DELIVERY => match value {
                0 => {
                    e.delivery = Delivery::Video;
                    true
                }
                1 => {
                    e.delivery = Delivery::Screen;
                    true
                }
                _ => false,
            },
            enc_field::OPEN_SHADOWS => {
                e.open_shadows = value != 0;
                true
            }
            enc_field::VIDEO_CODEC => {
                match self.video_codecs(out, &e).get(value as usize).copied() {
                    Some(codec) => {
                        e.video_codec = codec;
                        if !codec.allows_profile(e.profile.as_deref()) {
                            e.profile = None;
                        }
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        };
        if !changed {
            return;
        }
        if out == output::STREAM {
            self.stream_form_mut().encoding = e;
            self.save_stream_form();
        } else {
            self.record_form_mut().set_encoding(e);
            self.save_profile_form();
        }
    }

    /// Starts recording with the output settings.
    pub fn record(&mut self) {
        // A running stream does not stop it: both run at once (ADR-0047).
        if self.rec_state != rec_state::OFF {
            return;
        }
        if let Some(why) = &self.cannot_record {
            self.set_note(why.clone(), true);
            return;
        }
        let p = &self.profile;
        if let Some(why) = p.problem() {
            self.set_note(why, true);
            return;
        }
        let encoder = self.encoder_id(&p.encoding());
        let request = RecordRequest {
            path: p.file_for(chrono::Local::now().naive_local()),
            container: p.format.container(),
            encoder,
            video: p.video_settings(),
            audio_bitrate_bps: u64::from(p.audio_kbps) * 1000,
            size: p.output_size.map(OutputSize::pair),
        };
        self.rec_state = rec_state::STARTING;
        self.set_note(String::new(), false);
        self.send(Command::StartRecording(request));
    }

    pub fn stop_recording(&mut self) {
        if self.rec_state == rec_state::RECORDING {
            self.rec_state = rec_state::STOPPING;
            self.send(Command::StopRecording);
        }
    }

    fn set_note(&mut self, note: String, bad: bool) {
        self.rec_note = note;
        self.rec_note_bad = bad;
    }

    /// The folder field was confirmed.
    pub fn set_folder(&mut self, text: &str) {
        let folder = PathBuf::from(text.trim());
        if folder.is_absolute() {
            self.record_form_mut().folder = folder;
            self.save_profile_form();
        } else {
            self.profile_note = "The folder must be a full path, for example C:\\Videos.".into();
        }
        // The field shows the folder in use (also after a refused entry).
        self.folder_serial += 1;
    }

    pub fn set_format(&mut self, i: i32) {
        if let Some(f) = Format::ALL.get(i as usize) {
            self.record_form_mut().format = *f;
            self.save_profile_form();
        }
    }

    /// The canvas whose Recording and Stream sections are showing, in pixels.
    fn settings_canvas_size(&self) -> (u32, u32) {
        match self.settings_canvas {
            OutputCanvas::Vertical => self
                .vertical
                .map(|v| (v.size.width, v.size.height))
                .unwrap_or((self.canvas.width, self.canvas.height)),
            OutputCanvas::Main => (self.canvas.width, self.canvas.height),
        }
    }

    /// The output size the Recording section shows. A vertical canvas with no
    /// section of its own is the vertical canvas, not the main recording's size.
    fn recording_output_size(&self) -> Option<OutputSize> {
        match self.settings_canvas {
            OutputCanvas::Main => self.profile.output_size,
            OutputCanvas::Vertical => self.vertical_profile.as_ref().and_then(|p| p.output_size),
        }
    }

    /// An Output size row was picked. Saved for the next recording or stream; the
    /// studio is not restarted and scene items stay where they are (ADR-0102).
    pub fn set_output_size(&mut self, out: i32, index: i32) {
        let canvas = self.settings_canvas_size();
        let current = if out == output::STREAM {
            self.stream_form().output_size
        } else {
            self.recording_output_size()
        };
        let Some(picked) = output_size_options(canvas, current)
            .get(index as usize)
            .map(|o| o.size)
        else {
            return;
        };
        if picked == current {
            return;
        }
        self.store_output_size(out, picked);
    }

    /// A long side was typed. The short side is the canvas-aspect fit. The canvas's
    /// own long side stores "Same as canvas".
    pub fn set_output_long_side(&mut self, out: i32, long: f32) {
        let canvas = self.settings_canvas_size();
        let Some(long) = whole_side(long) else {
            self.output_size_note(out, "Long side must be a whole number of pixels.".into());
            return;
        };
        if !long.is_multiple_of(2) {
            self.output_size_note(
                out,
                format!("Long side {long} must be even, from 2 to 7680."),
            );
            return;
        }
        let canvas_long = canvas.0.max(canvas.1);
        let picked = if long == canvas_long {
            None
        } else {
            match fit_output_size(long, canvas) {
                Some(size) => Some(size),
                None => {
                    self.output_size_note(
                        out,
                        format!(
                            "Long side {long} does not fit this canvas: both sides must be from 2 to 7680."
                        ),
                    );
                    return;
                }
            }
        };
        let current = if out == output::STREAM {
            self.stream_form().output_size
        } else {
            self.recording_output_size()
        };
        if picked == current {
            self.output_size_note(out, String::new());
            return;
        }
        self.store_output_size(out, picked);
    }

    /// Same as canvas stores no size. Turning it off picks 1080p when that differs
    /// from the canvas, otherwise the next prepared size that does.
    pub fn set_output_same(&mut self, out: i32, on: bool) {
        let canvas = self.settings_canvas_size();
        let current = if out == output::STREAM {
            self.stream_form().output_size
        } else {
            self.recording_output_size()
        };
        if on {
            if current.is_none() {
                return;
            }
            self.store_output_size(out, None);
            return;
        }
        if current.is_some() {
            return;
        }
        for long in [1920, 1280, 3840, 2560] {
            if let Some(size) = fit_output_size(long, canvas)
                && size.pair() != canvas
            {
                self.store_output_size(out, Some(size));
                return;
            }
        }
        self.output_size_note(out, "This canvas has no other prepared size.".into());
    }

    fn store_output_size(&mut self, out: i32, picked: Option<OutputSize>) {
        if out == output::STREAM {
            self.stream_form_mut().output_size = picked;
            self.save_stream_form();
        } else {
            self.record_form_mut().output_size = picked;
            self.save_profile_form();
        }
    }

    fn output_size_note(&mut self, out: i32, text: String) {
        if out == output::STREAM {
            self.stream_note = text;
        } else {
            self.profile_note = text;
        }
    }

    /// Goes live with the stream settings: the host reads the key from the store.
    pub fn go_live(&mut self) {
        if self.st_state != st_state::OFF {
            return;
        }
        self.commit_server();
        let fail = |ui: &mut Self, why: String| {
            ui.st_note = why;
            ui.st_bad = true;
        };
        if self.cannot_record.is_some() {
            // Live controls already say why (the button is disabled).
            return;
        }
        if let Some(why) = self.stream.missing() {
            fail(self, why.to_owned());
            self.st_setup_note = true;
            return;
        }
        self.st_setup_note = false;
        if let Some(why) = self.stream.problem() {
            return fail(self, why);
        }
        let Some(server) = self.stream.parsed() else {
            return fail(self, "Enter the server in Settings, Stream.".to_owned());
        };
        let request =
            match StreamRequest::from_profile(&self.stream, self.encoder_id(&self.stream.encoding))
            {
                Ok(r) => r,
                Err(why) => return fail(self, why),
            };
        self.st_state = st_state::STARTING;
        self.st_live = Some((server.host, Instant::now()));
        self.st_link = Link::default();
        self.st_note.clear();
        self.st_bad = false;
        self.send(Command::StartStream(request));
    }

    /// Ends the stream (the panel asked first).
    pub fn end_stream(&mut self) {
        if self.st_state == st_state::LIVE || self.st_state == st_state::STARTING {
            self.st_state = st_state::ENDING;
            self.send(Command::EndStream);
        }
    }

    /// "Record vertical": records the vertical canvas with its own recording settings
    /// (the main ones until changed) to a file of its own (ADR-0093 decision 6). `Err`:
    /// why it cannot start, for a toast.
    pub fn record_vertical(&mut self) -> Result<(), String> {
        if self.vertical.is_none() || self.vertical_rec != VerticalOut::Off {
            return Ok(());
        }
        if let Some(why) = &self.cannot_record {
            return Err(vertical_why(why));
        }
        let p = self.record_form_for(OutputCanvas::Vertical);
        if let Some(why) = p.problem() {
            return Err(vertical_why(&why));
        }
        let request = RecordRequest {
            path: p.vertical_file_for(chrono::Local::now().naive_local()),
            container: p.format.container(),
            encoder: self.encoder_id(&p.encoding()),
            video: p.video_settings(),
            audio_bitrate_bps: u64::from(p.audio_kbps) * 1000,
            // No vertical section yet: encode at the vertical canvas, not at the
            // main recording's size (ADR-0102).
            size: self
                .vertical_profile
                .as_ref()
                .and_then(|v| v.output_size)
                .map(OutputSize::pair),
        };
        self.vertical_rec = VerticalOut::Starting;
        self.send(Command::StartVerticalRecording(request));
        Ok(())
    }

    /// "Stop vertical recording".
    pub fn stop_vertical_recording(&mut self) {
        if self.vertical_rec == VerticalOut::On {
            self.vertical_rec = VerticalOut::Ending;
            self.send(Command::StopVerticalRecording);
        }
    }

    /// "Go live vertical": streams the vertical canvas to its own server with its own
    /// key (ADR-0093 decision 6). `Err`: why it cannot start, for a toast.
    pub fn go_live_vertical(&mut self) -> Result<(), String> {
        if self.vertical.is_none() || self.vertical_st != VerticalOut::Off {
            return Ok(());
        }
        if let Some(why) = &self.cannot_record {
            return Err(vertical_why(why));
        }
        let p = match &self.vertical_stream {
            Some(p) if p.missing().is_none() => p,
            _ => return Err(vertical_why(VERTICAL_SETUP)),
        };
        if let Some(why) = p.problem() {
            return Err(vertical_why(&why));
        }
        let request = StreamRequest::from_profile(p, self.encoder_id(&p.encoding))
            .map_err(|why| vertical_why(&why))?;
        self.vertical_st = VerticalOut::Starting;
        self.send(Command::StartVerticalStream(request));
        Ok(())
    }

    /// Ends the vertical stream (the frame asked first).
    pub fn end_vertical_stream(&mut self) {
        if matches!(self.vertical_st, VerticalOut::On | VerticalOut::Starting) {
            self.vertical_st = VerticalOut::Ending;
            self.send(Command::EndVerticalStream);
        }
    }

    /// The server field was confirmed.
    pub fn set_server(&mut self, text: &str) {
        self.server_draft = None;
        let url = text.trim();
        if url == self.stream_form().server {
            return;
        }
        if !url.is_empty()
            && let Err(why) = stream_profile::parse_server(url)
        {
            self.stream_note = why;
            // The field shows the server in use.
            self.server_serial += 1;
            return;
        }
        self.stream_form_mut().server = url.to_owned();
        self.server_serial += 1;
        self.save_stream_form();
    }

    /// The server field changed without being confirmed (Return, leaving the field).
    pub fn server_edited(&mut self, text: &str) {
        self.server_draft = Some(text.to_owned());
    }

    /// Confirms a server typed but not confirmed: closing Settings or pressing Go live
    /// keeps what the field shows.
    fn commit_server(&mut self) {
        if let Some(text) = self.server_draft.take() {
            self.set_server(&text);
        }
    }

    /// The key field was confirmed: the host saves it in the store.
    pub fn save_key(&mut self, text: &str) {
        let key = text.trim();
        if key.is_empty() || !self.stream_form_idle() {
            return;
        }
        let key = Some(Secret::new(key));
        self.send(match self.settings_canvas {
            OutputCanvas::Main => Command::SaveStreamKey(key),
            OutputCanvas::Vertical => Command::SaveVerticalStreamKey(key),
        });
    }

    /// "Remove key".
    pub fn remove_key(&mut self) {
        if self.stream_form_idle() {
            self.send(match self.settings_canvas {
                OutputCanvas::Main => Command::SaveStreamKey(None),
                OutputCanvas::Vertical => Command::SaveVerticalStreamKey(None),
            });
        }
    }

    /// No recording of the picked canvas runs: its settings can change.
    fn record_form_idle(&self) -> bool {
        match self.settings_canvas {
            OutputCanvas::Main => self.rec_state == rec_state::OFF,
            OutputCanvas::Vertical => !self.vertical_rec.busy(),
        }
    }

    /// No stream of the picked canvas runs: its settings can change.
    fn stream_form_idle(&self) -> bool {
        match self.settings_canvas {
            OutputCanvas::Main => self.st_state == st_state::OFF,
            OutputCanvas::Vertical => !self.vertical_st.busy(),
        }
    }

    /// "Record when the stream starts".
    pub fn set_auto_record(&mut self, on: bool) {
        self.stream_form_mut().auto_record = on;
        self.save_stream_form();
    }

    pub fn set_reconnect(&mut self, on: bool) {
        self.stream_form_mut().reconnect = on;
        self.save_stream_form();
    }

    /// The SRT latencies offered: the default, the presets and a saved other one.
    fn latencies(&self) -> Vec<Option<u16>> {
        let mut v: Vec<Option<u16>> = std::iter::once(None)
            .chain(SRT_LATENCIES_MS.iter().map(|ms| Some(*ms)))
            .collect();
        let saved = self.stream_form().srt_latency_ms;
        if !v.contains(&saved) {
            v.push(saved);
        }
        v
    }

    pub fn set_latency(&mut self, i: i32) {
        if let Some(ms) = self.latencies().get(i as usize).copied() {
            self.stream_form_mut().srt_latency_ms = ms;
            self.save_stream_form();
        }
    }

    /// Saves the stream settings the Stream section changed.
    fn save_stream_form(&mut self) {
        match self.settings_canvas {
            OutputCanvas::Main => self.save_stream(),
            OutputCanvas::Vertical => self.save_vertical_stream(),
        }
    }

    /// Saves the vertical stream settings (`stream-vertical`) and hands them to the
    /// studio.
    fn save_vertical_stream(&mut self) {
        let Some(v) = self.vertical_stream.clone() else {
            return;
        };
        self.stream_note = match self.user.set_section(STREAM_VERTICAL, &v) {
            Ok(()) => String::new(),
            Err(e) => format!("The vertical stream settings could not be saved: {e}"),
        };
        self.send(Command::SetVerticalStreamProfile(Some(v)));
    }

    /// Saves the recording settings the Recording section changed.
    fn save_profile_form(&mut self) {
        match self.settings_canvas {
            OutputCanvas::Main => self.save_profile(),
            OutputCanvas::Vertical => {
                let Some(v) = self.vertical_profile.clone() else {
                    return;
                };
                self.profile_note = match self.user.set_section(RECORDING_VERTICAL, &v) {
                    Ok(()) => String::new(),
                    Err(e) => format!("The vertical recording settings could not be saved: {e}"),
                };
                self.send(Command::SetVerticalProfile(Some(v)));
            }
        }
    }

    fn save_stream(&mut self) {
        self.stream_note = match self.user.set_section(STREAM, &self.stream) {
            Ok(()) => String::new(),
            Err(e) => format!("The stream settings could not be saved: {e}"),
        };
        // The control thread keeps a copy for `stream.start` (control API).
        self.send(Command::SetStreamProfile(self.stream.clone()));
    }

    /// The stream's duration so far and its server's host, while one runs.
    pub fn streaming_for(&self) -> Option<(Duration, String, bool)> {
        if self.st_state == st_state::OFF {
            return None;
        }
        self.st_live
            .as_ref()
            .map(|(host, since)| (since.elapsed(), host.clone(), self.st_link.connected))
    }

    /// The stream's state in words for Live controls.
    fn stream_text(&self) -> String {
        let host = self
            .st_live
            .as_ref()
            .map(|(h, _)| h.clone())
            .unwrap_or_default();
        match self.st_state {
            st_state::STARTING => format!("Connecting to {host}…"),
            st_state::LIVE if self.st_link.connected => format!("Live on {host}"),
            st_state::LIVE if self.st_link.reconnect_attempts > 0 => format!(
                "Not connected to {host}, trying again (attempt {})",
                self.st_link.reconnect_attempts
            ),
            st_state::LIVE => format!("Connecting to {host}…"),
            st_state::ENDING => "Ending the stream…".to_owned(),
            // What is still missing, nothing once all is set.
            _ if self.st_setup_note => self.stream.missing().unwrap_or_default().to_owned(),
            _ => self.st_note.clone(),
        }
    }

    /// The note asked for settings that are all set now (Live controls then show
    /// neither a note nor "Stream settings").
    fn setup_note_stale(&self) -> bool {
        self.st_setup_note && self.stream.missing().is_none()
    }

    fn push_stream(&self, g: &Studio<'_>) {
        // The live controls show the main stream; the section's fields the picked
        // canvas's settings.
        let st = self.stream_form();
        g.set_st_state(self.st_state);
        g.set_st_connected(self.st_state == st_state::LIVE && self.st_link.connected);
        g.set_st_text(self.stream_text().into());
        g.set_st_bad(
            self.st_state == st_state::OFF
                && self.rec_state == rec_state::OFF
                && self.st_bad
                && !self.setup_note_stale(),
        );
        g.set_st_needs_setup(self.stream.missing().is_some());
        if g.get_st_server_serial() != self.server_serial as i32 {
            g.set_st_server(st.server.as_str().into());
            g.set_st_server_serial(self.server_serial as i32);
        }
        g.set_st_key_saved(st.key_saved);
        g.set_st_key_place(self.key_hint.as_str().into());
        g.set_st_key_serial(self.key_serial as i32);
        g.set_st_reconnect(st.reconnect);
        g.set_st_auto_record(st.auto_record);
        let record = self.record_form_for(self.settings_canvas).encoding();
        let same_encoder = self.encoder_id(&record) == self.encoder_id(&st.encoding);
        g.set_st_auto_record_note(
            stream_profile::auto_record_note(&record, &st.encoding, same_encoder).into(),
        );
        let parsed = st.parsed();
        g.set_st_srt(
            parsed
                .as_ref()
                .is_some_and(|s| s.protocol == stream_profile::Protocol::Srt),
        );
        let latencies = self.latencies();
        g.set_st_latencies(strings(latencies.iter().map(|ms| match ms {
            None => format!("Default ({SRT_DEFAULT_LATENCY_MS} ms)"),
            Some(ms) => format!("{ms} ms"),
        })));
        g.set_st_latency(
            latencies
                .iter()
                .position(|ms| *ms == st.srt_latency_ms)
                .unwrap_or(0) as i32,
        );
        g.set_st_enc(self.encoder_data(output::STREAM));
        let st_canvas = self.settings_canvas_size();
        let st_sizes = output_size_options(st_canvas, st.output_size);
        let st_nest = output_nest(&st_sizes, st.output_size, st_canvas);
        g.set_st_same(st_nest.same);
        g.set_st_bands(choice_model(&st_nest.bands));
        g.set_st_extras(choice_model(&st_nest.extras));
        g.set_st_choice(st_nest.index);
        g.set_st_value(st_nest.value.into());
        g.set_st_long(output_long_side(st.output_size, st_canvas) as i32);
        g.set_st_note(self.stream_note.as_str().into());
        let summary = match &parsed {
            None => "No server yet".to_owned(),
            Some(server) => format!(
                "{} · {} · {} · {} · audio {} kbit/s",
                server.protocol.label(),
                st.server,
                self.encoder_name(&st.encoding),
                st.encoding.video_summary(),
                st.encoding.audio_kbps
            ),
        };
        g.set_st_summary(summary.into());
        match &parsed {
            None => {
                g.set_st_kind("No server yet".into());
                g.set_st_where(String::new().into());
                g.set_st_encoder_name(String::new().into());
                g.set_st_video_line(String::new().into());
                g.set_st_audio_line(String::new().into());
            }
            Some(server) => {
                g.set_st_kind(server.protocol.label().into());
                g.set_st_where(st.server.clone().into());
                g.set_st_encoder_name(self.encoder_name(&st.encoding).into());
                g.set_st_video_line(st.encoding.video_summary().into());
                g.set_st_audio_line(format!("{} kbit/s", st.encoding.audio_kbps).into());
            }
        }
    }

    fn stream_update(&mut self, u: StreamUpdate, out: &mut Applied) {
        match u {
            StreamUpdate::Started {
                url,
                video_encoder,
                audio_encoder,
            } => {
                log(&format!(
                    "streaming to {url} (video: {video_encoder}, audio: {})",
                    audio_encoder.as_deref().unwrap_or("none")
                ));
                if self.st_state == st_state::OFF {
                    // A control API client went live: show it as if Go live was pressed.
                    let host = stream_profile::parse_server(&url)
                        .map(|s| s.host)
                        .unwrap_or_default();
                    self.st_live = Some((host, Instant::now()));
                    self.st_link = Link::default();
                    self.st_note.clear();
                    self.st_bad = false;
                    self.st_setup_note = false;
                }
                self.st_state = st_state::LIVE;
                if let Some((_, since)) = &mut self.st_live {
                    *since = Instant::now();
                }
            }
            // A control API client went live first while Go live was on its way: the
            // stream that runs is shown already.
            StreamUpdate::NotStarted(why) if self.st_state != st_state::STARTING => {
                log(&format!("go live refused: {why}"));
            }
            StreamUpdate::NotStarted(why) => {
                self.st_state = st_state::OFF;
                self.st_live = None;
                self.st_note = format!("The stream did not start: {why}");
                self.st_bad = true;
            }
            StreamUpdate::NoKey => {
                self.st_state = st_state::OFF;
                self.st_live = None;
                // The store has none (removed outside RBS): the settings say so too.
                self.stream.key_saved = false;
                self.save_stream();
                self.st_note =
                    "The stream did not start: no stream key is saved. Enter it in Settings, Stream."
                        .into();
                self.st_bad = true;
                self.st_setup_note = true;
            }
            StreamUpdate::Link(link) => {
                if link.connected != self.st_link.connected {
                    log(&format!(
                        "stream {} (connected {} times, {} reconnect attempts)",
                        if link.connected {
                            "connected"
                        } else {
                            "not connected"
                        },
                        link.connects,
                        link.reconnect_attempts
                    ));
                }
                self.st_link = link;
            }
            StreamUpdate::Ended {
                failure,
                frames,
                dropped,
                connects,
            } => {
                let duration = self
                    .st_live
                    .take()
                    .map(|(_, since)| since.elapsed())
                    .unwrap_or_default();
                // The user, or a control API client (`stream.stop`), ended it: only a
                // stream asked to end has no failure.
                let asked = self.st_state == st_state::ENDING || failure.is_none();
                self.st_state = st_state::OFF;
                self.st_link = Link::default();
                log(&format!(
                    "stream ended: {frames} frames, {dropped} dropped, {connects} connections, failure: {failure:?}"
                ));
                let secs = duration.as_secs();
                let time = if secs >= 3600 {
                    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
                } else {
                    format!("{}:{:02}", secs / 60, secs % 60)
                };
                match failure {
                    None if connects == 0 && asked => {
                        self.st_note = "The stream ended before it reached the server.".into();
                        self.st_bad = false;
                    }
                    None if connects == 0 => {
                        self.st_note = "The stream ended without ever reaching the server.".into();
                        self.st_bad = true;
                    }
                    None => {
                        let mut note = format!("The stream ended after {time}.");
                        if dropped > 0 {
                            note.push_str(&format!(" {dropped} frames or packets were dropped."));
                        }
                        self.st_note = note;
                        self.st_bad = false;
                    }
                    Some(why) => {
                        let text = if connects == 0 {
                            format!("The stream never reached the server: {why}")
                        } else {
                            format!("The stream stopped after {time}: {why}")
                        };
                        out.error = Some(text.clone());
                        self.st_note = text;
                        self.st_bad = true;
                    }
                }
            }
        }
    }

    fn save_profile(&mut self) {
        self.profile_note = match self.user.set_section(RECORDING, &self.profile) {
            Ok(()) => String::new(),
            Err(e) => format!("The recording settings could not be saved: {e}"),
        };
        // The control thread keeps a copy for control API clients (ADR-0044).
        self.send(Command::SetProfile(self.profile.clone()));
    }

    /// The canvas tiles for the current orientation: the broadcast sizes, and the
    /// main display when it is not already one of them. A custom size is only the
    /// fields, so it is absent here.
    fn canvas_tiles(&self) -> Vec<CanvasTile> {
        let portrait = self.canvas.height > self.canvas.width;
        let display = self.display.and_then(|main| {
            let size = canvas_profile::canvas_for_display(main.size.0, main.size.1)?;
            let percent = rbs_capture::choose_framebuffer(rbs_capture::DisplayReading {
                monitor: None,
                mode: Some(size),
                scale: main.scale,
            })
            .and_then(|c| c.percent);
            Some((size, percent))
        });
        let mut tiles: Vec<_> = canvas_profile::SIZES
            .iter()
            .map(|&(w, h)| {
                let (width, height) = if portrait { (h, w) } else { (w, h) };
                let title = canvas_profile::broadcast_name(width, height).unwrap_or("Custom");
                let detail = match display {
                    Some((size, percent)) if size == (width, height) => match percent {
                        Some(percent) => {
                            format!("{width} × {height} · this display, {percent}%")
                        }
                        None => format!("{width} × {height} · this display"),
                    },
                    _ => format!("{width} × {height}"),
                };
                CanvasTile {
                    title,
                    detail,
                    width,
                    height,
                }
            })
            .collect();
        if let Some(((width, height), percent)) = display
            && !tiles.iter().any(|t| (t.width, t.height) == (width, height))
        {
            let detail = match percent {
                Some(percent) => format!("{width} × {height} · {percent}%"),
                None => format!("{width} × {height}"),
            };
            tiles.push(CanvasTile {
                title: "This display",
                detail,
                width,
                height,
            });
        }
        tiles
    }

    /// Width and height were typed or a tile was picked. `close_preview` runs before
    /// the studio restarts: the old studio's preview ends with it.
    pub fn set_canvas_sides(&mut self, width: f32, height: f32, close_preview: impl FnOnce()) {
        let Some(width) = whole_side(width) else {
            self.canvas_note = "Canvas width must be a whole number of pixels.".into();
            return;
        };
        let Some(height) = whole_side(height) else {
            self.canvas_note = "Canvas height must be a whole number of pixels.".into();
            return;
        };
        let next = CanvasProfile {
            width,
            height,
            ..self.canvas
        };
        self.change_canvas(next, close_preview);
    }

    /// Portrait swaps the sides. Turning it on while the canvas is already portrait,
    /// or off while it is landscape, changes nothing.
    pub fn set_canvas_portrait(&mut self, on: bool, close_preview: impl FnOnce()) {
        let (width, height) = (self.canvas.width, self.canvas.height);
        if on == (height > width) {
            return;
        }
        self.set_canvas_sides(height as f32, width as f32, close_preview);
    }

    /// A frame rate was picked.
    pub fn set_canvas_rate(&mut self, i: i32, close_preview: impl FnOnce()) {
        if let Some(fps) = RATES.get(i as usize) {
            let next = CanvasProfile {
                fps: *fps,
                ..self.canvas
            };
            self.change_canvas(next, close_preview);
        }
    }

    fn change_canvas(&mut self, next: CanvasProfile, close_preview: impl FnOnce()) {
        if next == self.canvas || self.canvas_restarting {
            return;
        }
        if self.rec_state != rec_state::OFF {
            self.canvas_note = "Stop the recording before changing the canvas.".into();
            return;
        }
        if self.st_state != st_state::OFF {
            self.canvas_note = "End the stream before changing the canvas.".into();
            return;
        }
        if let Some(why) = next.problem() {
            self.canvas_note = why;
            return;
        }
        if self.state != 1 || self.host.is_none() {
            // The canvas is part of the scene collection, which the studio keeps.
            self.canvas_note = "The canvas can be changed once the studio runs.".into();
            return;
        }
        self.canvas = next;
        self.canvas_note.clear();
        close_preview();
        self.preview_ready = false;
        self.preview_failed = false;
        self.scene_preview_ready = false;
        self.scene_preview_failed = false;
        self.vertical_preview_ready = false;
        self.vertical_preview_failed = false;
        self.canvas_restarting = true;
        self.send(Command::SetCanvas {
            size: next.size(),
            fps: next.frame_rate(),
        });
    }

    /// Reads the output profiles and scene collections for the menus again.
    /// The Library panel shows: makes the sample thumbnails the first time.
    pub fn library_shown(&mut self) {
        if self.library_samples.row_count() == 0 {
            self.library_samples.set_vec(library_samples());
        }
    }

    /// Applies a built-in look to the selected source (ADR-0075).
    pub fn apply_look(&mut self, look: &str) {
        if self.selected < 0 {
            return;
        }
        self.deleted.clear();
        self.send(Command::ApplyLook {
            source: SourceId::from_raw(self.selected as u32),
            look: look.to_owned(),
        });
    }

    /// Adds the look at `index` in the Library's list.
    pub fn apply_look_at(&mut self, index: i32) {
        let looks = rbs_scene::look::built_in();
        let Some(look) = looks.get(index as usize) else {
            return;
        };
        self.apply_look(look.id);
    }

    /// Adds a colour shape (0 circle … 6 arrow) and opens Shape and border.
    pub fn add_stamp(&mut self, kind: i32) {
        let Ok(kind) = u8::try_from(kind) else {
            return;
        };
        self.deleted.clear();
        self.adding = true;
        self.reveal_shape = true;
        self.send(Command::AddStamp { kind });
    }

    /// Adds a PNG the user picked. The new source is selected.
    pub fn add_image_file(&mut self, path: &std::path::Path) {
        self.deleted.clear();
        self.adding = true;
        self.reveal_shape = false;
        self.send(Command::AddImage {
            path: path.to_owned(),
        });
    }

    /// Adds a media file the user picked (ADR-0108). The scripted demo decoder
    /// ignores file bytes; any regular file path is enough for a Program test.
    pub fn add_media_file(&mut self, path: &std::path::Path) {
        self.deleted.clear();
        self.adding = true;
        self.reveal_shape = false;
        self.send(Command::AddMedia {
            path: path.to_owned(),
        });
    }

    /// Adds a browser page (ADR-0109). The helper paints synthetic frames until CEF ships.
    pub fn add_browser_page(&mut self, url: &str) {
        self.deleted.clear();
        self.adding = true;
        self.reveal_shape = false;
        self.send(Command::AddBrowser {
            url: url.to_owned(),
        });
    }

    /// The fill of a colour source. A colour that is not `#rrggbb` or `#rrggbbaa`
    /// changes nothing.
    pub fn set_source_fill(&mut self, id: i32, color: &str) {
        let Some(color) = rbs_api::types::parse_color(color.trim()) else {
            return;
        };
        self.deleted.clear();
        self.send(Command::SetSourceFill {
            id: SourceId::from_raw(id as u32),
            color: rbs_api::types::format_color(color),
        });
    }

    /// Turns a colour source's fill on or off. Off leaves the inside empty.
    pub fn set_source_filled(&mut self, id: i32, on: bool) {
        self.deleted.clear();
        self.send(Command::SetSourceFilled {
            id: SourceId::from_raw(id as u32),
            on,
        });
    }

    /// Adds a sample picture to the scene being edited (ADR-0075). The new source
    /// is selected. A plain frame also asks Properties to open on its border.
    pub fn add_sample(&mut self, sample: &str) {
        self.deleted.clear();
        self.adding = true;
        self.reveal_shape = rbs_sources::still::plain_frame(sample).is_some();
        self.send(Command::AddSample {
            sample: sample.to_owned(),
        });
    }

    /// Where the Library gear sends the selected source: the Properties tab
    /// (0 Transform, 1 Filters) and the filter of the look's type, when the source
    /// already has one.
    pub fn look_target(&self, id: &str) -> Option<(i32, i32, Option<i32>)> {
        let look = rbs_scene::look::find(id)?;
        if self.selected < 0 {
            return None;
        }
        let tab = match look.group {
            rbs_scene::look::LookGroup::Shape => 0,
            _ => 1,
        };
        let filter = look.filters.first().and_then(|wanted| {
            self.rows
                .iter()
                .find(|r| r.id.get() as i32 == self.selected)
                .and_then(|r| {
                    r.filters
                        .iter()
                        .find(|f| f.type_id == wanted.type_id)
                        .map(|f| f.id.get() as i32)
                })
        });
        Some((self.selected, tab, filter))
    }

    pub fn refresh_library(&mut self) {
        let rows = |list: Vec<(EntryId, String)>, current: &EntryId| -> Vec<EntryRow> {
            list.into_iter()
                .map(|(id, name)| EntryRow {
                    current: &id == current,
                    id: id.as_str().into(),
                    name: name.into(),
                })
                .collect()
        };
        let profiles = self.user.profiles();
        let collections = self.user.collections();
        // A control API switch is answered once nothing of it is left to do.
        let answered = if self.switching.is_none() {
            self.api_switch.take()
        } else {
            None
        };
        self.send(Command::SetLibrary {
            profiles: api_library(&profiles, &self.user.profile_id),
            collections: api_library(&collections, &self.user.collection_id),
            answered,
        });
        self.profile_rows
            .set_vec(rows(profiles, &self.user.profile_id));
        self.collection_rows
            .set_vec(rows(collections, &self.user.collection_id));
    }

    /// The output profiles (`collection` false) or scene collections as last read,
    /// for the File menu's pickers: `(name, id, in use)`, by name.
    pub fn library_choices(&self, collection: bool) -> Vec<(String, String, bool)> {
        let rows = if collection {
            &self.collection_rows
        } else {
            &self.profile_rows
        };
        rows.iter()
            .map(|r| (r.name.to_string(), r.id.to_string(), r.current))
            .collect()
    }

    /// The id of the scene collection in use, if known.
    pub fn current_collection(&self) -> Option<String> {
        self.collection_rows
            .iter()
            .find(|r| r.current)
            .map(|r| r.id.to_string())
    }

    /// The canvas's scenes as picker choices `(name, id, on air)`, in panel order.
    pub fn scene_choices(&self) -> Vec<(String, String, bool)> {
        self.scene_list.as_ref().map_or_else(Vec::new, |l| {
            l.scenes
                .iter()
                .map(|s| (s.name.clone(), s.id.get().to_string(), s.id == l.program))
                .collect()
        })
    }

    /// The scene `scene` names if the canvas has it, else (`None`) the scene being
    /// worked on: the one on air, or in studio mode the preview scene.
    pub fn scene_target(&self, scene: Option<&str>) -> Option<i32> {
        let l = self.scene_list.as_ref()?;
        let id = match scene {
            Some(text) => {
                let raw: u32 = text.parse().ok()?;
                l.scenes.iter().find(|s| s.id.get() == raw)?.id
            }
            None => l.preview.unwrap_or(l.program),
        };
        Some(id.get() as i32)
    }

    /// How many scenes the canvas has.
    pub fn scene_count(&self) -> usize {
        self.scene_list.as_ref().map_or(0, |l| l.scenes.len())
    }

    /// A source of the scene being edited, by its source id, by its item's id (both
    /// as id parameter text), or (both `None`) the selected source.
    pub fn source_facts(&self, source: Option<&str>, item: Option<&str>) -> Option<SourceFacts> {
        let layers: Vec<(u32, bool, bool)> = self
            .rows
            .iter()
            .map(|r| (r.id.get(), r.global, r.layout.is_some()))
            .collect();
        let room = |id: u32| -> (bool, bool) {
            let Some(index) = layers.iter().position(|r| r.0 == id) else {
                return (false, false);
            };
            if !layers[index].2 {
                return (false, false);
            }
            let global = layers[index].1;
            let pics: Vec<usize> = layers
                .iter()
                .enumerate()
                .filter(|(_, r)| r.1 == global && r.2)
                .map(|(i, _)| i)
                .collect();
            let pos = pics.iter().position(|i| *i == index);
            (
                pos.is_some_and(|p| p > 0),
                pos.is_some_and(|p| p + 1 < pics.len()),
            )
        };
        let row = match (source, item) {
            (Some(text), _) => {
                let raw: u32 = text.parse().ok()?;
                match self.rows.iter().find(|r| r.id.get() == raw) {
                    Some(row) => row,
                    // Not in the scene being edited, but heard in the Mixer (studio
                    // mode, global sources): only its sound can be asked about.
                    None => {
                        let h = self.heard.iter().find(|h| h.id == raw)?;
                        return Some(SourceFacts {
                            id: raw as i32,
                            item: 0,
                            visible: true,
                            active: true,
                            locked: false,
                            muted: h.muted,
                            mixer_hidden: h.hidden,
                            monitor: h.monitor,
                            soloed: self.is_soloed(raw),
                            picture: false,
                            audio: true,
                            reloadable: true,
                            has_settings: false,
                            sized: false,
                            flip: rbs_scene::FlipState::default(),
                            can_up: false,
                            can_down: false,
                        });
                    }
                }
            }
            (None, Some(text)) => {
                let raw: u32 = text.parse().ok()?;
                self.rows.iter().find(|r| r.item.get() == raw)?
            }
            (None, None) => self
                .rows
                .iter()
                .find(|r| r.id.get() as i32 == self.selected)?,
        };
        let (can_up, can_down) = room(row.id.get());
        Some(SourceFacts {
            id: row.id.get() as i32,
            item: row.item.get(),
            visible: row.visible,
            active: row.active,
            locked: row.locked,
            muted: row.audio.muted,
            mixer_hidden: row.audio.mixer_hidden,
            monitor: row.audio.monitor,
            soloed: self.is_soloed(row.id.get()),
            picture: row.layout.is_some(),
            audio: row.kind.is_some_and(SourceKind::is_audio),
            reloadable: row.kind.is_some(),
            has_settings: matches!(
                row.kind,
                Some(SourceKind::Screen | SourceKind::Window | SourceKind::Camera)
            ),
            sized: row.picture.is_some(),
            flip: row.layout.map(|l| l.flip_state()).unwrap_or_default(),
            can_up,
            can_down,
        })
    }

    /// Whether the scene can move up and down in the list.
    pub fn scene_ends(&self, id: i32) -> Option<(bool, bool)> {
        let n = self.scenes.row_count();
        let i = (0..n).find(|&i| self.scenes.row_data(i).is_some_and(|s| s.id == id))?;
        Some((i > 0, i + 1 < n))
    }

    /// The id of the scene at 1-based position `n` in panel order.
    pub fn scene_at(&self, n: usize) -> Option<i32> {
        let l = self.scene_list.as_ref()?;
        let s = l.scenes.get(n.checked_sub(1)?)?;
        Some(s.id.get() as i32)
    }

    /// The id of the scene `step` places after (or, negative, before) the one on
    /// air, wrapping; `None` with fewer than two scenes.
    pub fn scene_step(&self, step: isize) -> Option<i32> {
        let l = self.scene_list.as_ref()?;
        let len = l.scenes.len();
        if len < 2 {
            return None;
        }
        let at = l.scenes.iter().position(|s| s.id == l.program).unwrap_or(0);
        let next = (at as isize + step).rem_euclid(len as isize) as usize;
        Some(l.scenes.get(next)?.id.get() as i32)
    }

    /// Refused, in words, while a recording or stream runs: `doing` is what would
    /// change ("switching output profiles").
    fn idle_for(&self, doing: &str) -> Result<(), String> {
        if self.rec_state != rec_state::OFF {
            return Err(format!("Stop the recording before {doing}."));
        }
        if self.st_state != st_state::OFF {
            return Err(format!("End the stream before {doing}."));
        }
        if self.vertical_rec.busy() {
            return Err(format!("Stop the vertical recording before {doing}."));
        }
        if self.vertical_st.busy() {
            return Err(format!("End the vertical stream before {doing}."));
        }
        Ok(())
    }

    /// Output profile menu: uses the profile `id` (refused while live).
    pub fn switch_profile(&mut self, id: &str) -> Result<(), String> {
        let id = EntryId::parse(id).ok_or("that output profile is gone")?;
        if id == self.user.profile_id {
            return Ok(());
        }
        self.idle_for("switching output profiles")?;
        let result = self.user.switch_profile(&id);
        // The studio gets the profile's settings and key account before the list
        // that shows it in use.
        if result.is_ok() {
            self.profile_loaded();
        }
        self.refresh_library();
        result
    }

    /// Shows the output profile in use and hands its settings and stream key account
    /// to the studio.
    fn profile_loaded(&mut self) {
        let (profile, why) = section(
            &self.user,
            RECORDING,
            RecordProfile::new,
            RecordProfile::problem,
        );
        let (mut stream, stream_why) = section(
            &self.user,
            STREAM,
            StreamProfile::new,
            StreamProfile::problem,
        );
        stream.key_saved &= self.keys_persistent;
        // The vertical canvas's sections (ADR-0093 decision 6): one that does not load
        // counts as not saved; one that loads with a problem is kept, so a start says
        // it cannot be used.
        self.vertical_profile = self
            .user
            .section::<RecordProfile>(RECORDING_VERTICAL)
            .ok()
            .flatten();
        self.vertical_stream = self
            .user
            .section::<StreamProfile>(STREAM_VERTICAL)
            .ok()
            .flatten()
            .map(|mut v| {
                v.key_saved &= self.keys_persistent;
                v
            });
        self.profile = profile;
        self.profile_note = why
            .map(|w| format!("The saved recording settings were not used: {w}"))
            .unwrap_or_default();
        self.folder_serial += 1;
        self.stream = stream;
        self.stream_note = stream_why
            .map(|w| format!("The saved stream settings were not used: {w}"))
            .unwrap_or_default();
        self.server_draft = None;
        self.server_serial += 1;
        self.key_serial += 1;
        self.send(Command::SetKeyAccount(stream_key::account(
            self.user.profile_id.as_str(),
        )));
        self.send(Command::SetProfile(self.profile.clone()));
        self.send(Command::SetStreamProfile(self.stream.clone()));
        self.send_vertical_profiles();
    }

    /// Hands the vertical canvas's saved recording and stream settings to the studio
    /// (ADR-0093 decision 6). A section that does not load counts as not saved; one
    /// that loads with a problem is sent as it is, so a start says it cannot be used.
    fn send_vertical_profiles(&self) {
        self.send(Command::SetVerticalProfile(self.vertical_profile.clone()));
        self.send(Command::SetVerticalStreamProfile(
            self.vertical_stream.clone(),
        ));
    }

    /// Output profile menu: makes a profile named `name`, with default settings or a
    /// copy of the one in use (`copy`), and uses it.
    pub fn new_profile(&mut self, name: &str, copy: bool) -> Result<(), String> {
        self.idle_for("switching output profiles")?;
        let id = self.user.new_profile(name, copy)?;
        let result = self.user.switch_profile(&id);
        if result.is_ok() {
            self.profile_loaded();
        }
        self.refresh_library();
        result
    }

    /// Output profile menu: renames the profile in use.
    pub fn rename_profile(&mut self, name: &str) -> Result<(), String> {
        let result = self.user.rename_profile(name);
        self.refresh_library();
        result
    }

    /// Output profile menu: deletes the profile `id` and its stream key. Deleting the
    /// one in use first switches to another (refused while live, and for the only one).
    pub fn delete_profile(&mut self, id: &str) -> Result<(), String> {
        let id = EntryId::parse(id).ok_or("that output profile is gone")?;
        if id == self.user.profile_id {
            let other = self
                .user
                .profiles()
                .into_iter()
                .find(|(other, _)| *other != id)
                .ok_or("The only output profile cannot be deleted. Make another first.")?;
            self.idle_for("deleting the output profile in use")?;
            self.user.switch_profile(&other.0)?;
            self.profile_loaded();
        }
        let result = self.user.delete_profile(&id);
        self.refresh_library();
        result?;
        // ADR-0051 decision 5: the keys go with the profile.
        let account = stream_key::account(id.as_str());
        self.send(Command::RemoveStreamKey(stream_key::vertical_account(
            &account,
        )));
        self.send(Command::RemoveStreamKey(account));
        Ok(())
    }

    /// Scene collection menu: runs the collection `id` (refused while live; the studio
    /// restarts, so the preview connects again).
    pub fn switch_collection(
        &mut self,
        id: &str,
        close_preview: impl FnOnce(),
    ) -> Result<(), String> {
        let id = EntryId::parse(id).ok_or("that scene collection is gone")?;
        if id == self.user.collection_id {
            return Ok(());
        }
        self.can_switch_collection()?;
        let (name, collection) = self.user.load_collection(&id)?;
        self.ask_switch(id, name, collection, None, false, close_preview);
        Ok(())
    }

    fn can_switch_collection(&self) -> Result<(), String> {
        self.idle_for("switching scene collections")?;
        if self.state != 1 || self.host.is_none() {
            return Err("Scene collections can be switched once the studio runs.".into());
        }
        if self.switching.is_some() || self.canvas_restarting {
            return Err("The studio is still restarting.".into());
        }
        Ok(())
    }

    /// Drops the Scenes, Sources and Mixer lists at once so a collection switch or
    /// delete never leaves the previous collection's rows on screen.
    fn clear_collection_view(&mut self) {
        self.selected = -1;
        self.inspected.set(-1);
        self.rows.clear();
        self.heard.clear();
        self.heard_names.clear();
        self.slots.clear();
        self.master_slot = None;
        self.scene_list = None;
        self.source_settings_view = None;
        self.sources.set_vec(Vec::new());
        self.scenes.set_vec(Vec::new());
        self.meters.set_vec(Vec::new());
        self.hidden_meters.set_vec(Vec::new());
        self.master_meter.set_vec(Vec::new());
        self.source_settings.set_vec(Vec::new());
        self.source_targets.set_vec(Vec::new());
        self.marks.set_vec(Vec::new());
        self.send(Command::CloseSourceSettings);
        self.refresh_existing();
        self.refresh_global_existing();
        self.refresh_marks();
    }

    fn ask_switch(
        &mut self,
        id: EntryId,
        name: String,
        collection: rbs_scene::SceneCollection,
        delete_after: Option<EntryId>,
        prefer_default_name: bool,
        close_preview: impl FnOnce(),
    ) {
        self.switch_seq += 1;
        let autosave = self.user.saver_for(&id, &name);
        close_preview();
        self.clear_collection_view();
        self.preview_ready = false;
        self.preview_failed = false;
        self.scene_preview_ready = false;
        self.scene_preview_failed = false;
        self.vertical_preview_ready = false;
        self.vertical_preview_failed = false;
        self.canvas_restarting = true;
        self.switching = Some(Switching {
            seq: self.switch_seq,
            id,
            name,
            delete_after,
            prefer_default_name,
        });
        self.send(Command::SwitchCollection {
            seq: self.switch_seq,
            collection: Box::new(collection),
            autosave,
        });
    }

    /// The studio answered a collection switch; returns what to tell the user.
    fn collection_switched(&mut self, seq: u64, result: Result<(), String>) -> Option<String> {
        let s = self.switching.take_if(|s| s.seq == seq)?;
        let told = match result {
            Ok(()) => {
                self.user.collection_switched(&s.id, &s.name);
                self.selected = -1;
                let mut told = s
                    .delete_after
                    .and_then(|old| self.user.delete_collection(&old).err())
                    .map(|e| format!("The scene collection was not deleted: {e}"));
                // The last collection was replaced: once it is gone, prefer the name
                // "Default" for the empty starter that took its place.
                if told.is_none()
                    && s.prefer_default_name
                    && !self.user.collection_name.eq_ignore_ascii_case(DEFAULT_NAME)
                    && let Err(e) = self.user.rename_collection(DEFAULT_NAME)
                {
                    told = Some(format!(
                        "The new scene collection could not be named {DEFAULT_NAME}: {e}"
                    ));
                }
                told
            }
            Err(why) => Some(why),
        };
        self.refresh_library();
        told
    }

    /// Why an Output profile or Scene collection command cannot run now (the menus
    /// disable it with this reason): live, restarting, or (for output profiles) the
    /// only entry. `op` is `new`, `duplicate`, `rename`, `delete`, `import` or `export`.
    pub fn entry_blocked(&self, collection: bool, op: &str) -> Result<(), String> {
        let restarting = self.switching.is_some() || self.canvas_restarting;
        match (collection, op) {
            (_, "import" | "export") => Ok(()),
            (true, "rename") if restarting => Err("The studio is still restarting.".into()),
            (_, "rename") => Ok(()),
            (false, "delete") => {
                if self.user.profiles().len() < 2 {
                    return Err(
                        "The only output profile cannot be deleted. Make another first.".into(),
                    );
                }
                self.idle_for("deleting the output profile in use")
            }
            // The only scene collection can be deleted: a "Default" one with one empty
            // scene replaces it.
            (true, "delete") => self.can_switch_collection(),
            (true, _) => self.can_switch_collection(),
            (false, _) => self.idle_for("switching output profiles"),
        }
    }

    /// Scene collection menu: makes a collection named `name`, new (one canvas the
    /// size of the one in use, one scene) or a copy of the one in use (`copy`), and
    /// runs it.
    pub fn new_collection(
        &mut self,
        name: &str,
        copy: bool,
        close_preview: impl FnOnce(),
    ) -> Result<(), String> {
        self.can_switch_collection()?;
        let id = self.user.new_collection(name, &self.canvas, copy)?;
        self.refresh_library();
        let (name, collection) = self.user.load_collection(&id)?;
        self.ask_switch(id, name, collection, None, false, close_preview);
        Ok(())
    }

    /// Scene collection menu: renames the collection in use.
    pub fn rename_collection(&mut self, name: &str) -> Result<(), String> {
        let result = self.user.rename_collection(name);
        self.refresh_library();
        result
    }

    /// Scene collection menu: deletes the collection `id`. Deleting the one in use
    /// first runs another (refused while live). The only scene collection is replaced
    /// by a new [`DEFAULT_NAME`] collection with one empty scene.
    pub fn delete_collection(
        &mut self,
        id: &str,
        close_preview: impl FnOnce(),
    ) -> Result<(), String> {
        let id = EntryId::parse(id).ok_or("that scene collection is gone")?;
        if id != self.user.collection_id {
            let result = self.user.delete_collection(&id);
            self.refresh_library();
            return result;
        }
        self.can_switch_collection()?;
        let (other, prefer_default_name) = match self
            .user
            .collections()
            .into_iter()
            .find(|(other, _)| *other != id)
        {
            Some((other, _)) => (other, false),
            None => {
                let name = free_collection_name(&self.user, DEFAULT_NAME);
                let prefer_default_name = !name.eq_ignore_ascii_case(DEFAULT_NAME);
                let other = self.user.new_collection(&name, &self.canvas, false)?;
                self.refresh_library();
                (other, prefer_default_name)
            }
        };
        let (name, collection) = self.user.load_collection(&other)?;
        self.ask_switch(
            other,
            name,
            collection,
            Some(id),
            prefer_default_name,
            close_preview,
        );
        Ok(())
    }

    /// The recording's duration so far, while one runs.
    pub fn recording_for(&self) -> Option<(Duration, String)> {
        self.rec_file
            .as_ref()
            .map(|(p, since)| (since.elapsed(), file_name(p)))
    }

    /// Clears one source's clip latch. `id` is the source id: the Mixer list also holds
    /// audio sources that are not running, and those have no meter slot.
    pub fn reset_clip(&mut self, id: i32) {
        if let Some(s) = self.slots.iter_mut().find(|s| s.source.get() as i32 == id) {
            s.ballistics.reset_clip();
        }
    }

    /// Takes an update from the control thread. Returns an error text for the error bar,
    /// and the preview endpoint when it arrives.
    pub fn apply(&mut self, u: Update) -> Applied {
        let mut out = Applied::default();
        // A studio that starts, stops or restarts answers nothing sent before.
        if matches!(
            u,
            Update::Started { .. } | Update::Failed(_) | Update::Canvas { .. }
        ) {
            self.edit.reset();
        }
        if self.monitor_update(&u) {
            return out;
        }
        match u {
            // Taken above.
            Update::Monitor(_) | Update::MonitorDevices(_) => {}
            Update::Thumbnail {
                adapter,
                target,
                picture,
            } => self.source_picture(adapter, target, picture),
            #[cfg(feature = "fault-injection")]
            Update::Recovered { .. } => {}
            Update::BoxPlaced { seq, ok } => {
                let next = self.edit.answered(seq, ok);
                self.run(next);
            }
            // Crops from the Properties fields are numbered 0, which the picture
            // panel never uses.
            Update::CropPlaced { seq, ok } => {
                let next = self.edit.answered(seq, ok);
                self.run(next);
            }
            Update::Started {
                preview,
                kinds,
                notes,
                encoders,
                cannot_record,
                display,
                ..
            } => {
                self.display = display;
                self.encoders = encoders;
                self.cannot_record = cannot_record;
                for n in notes {
                    log(&n);
                }
                self.state = 1;
                self.send(Command::SetProfile(self.profile.clone()));
                self.send(Command::SetStreamProfile(self.stream.clone()));
                self.send_vertical_profiles();
                self.refresh_library();
                self.kind_list = kinds;
                let rows: Vec<KindData> = self
                    .kind_list
                    .iter()
                    .enumerate()
                    .map(|(index, k)| KindData {
                        code: index as i32,
                        label: k.label().into(),
                    })
                    .collect();
                self.kinds.set_vec(rows);
                self.preview_offered = preview.is_some();
                out.preview = preview;
            }
            Update::Canvas { preview, size, fps } => {
                self.canvas_restarting = false;
                let running = (size.width, size.height, whole_fps(fps));
                if running != (self.canvas.width, self.canvas.height, self.canvas.fps) {
                    // Refused: keep what runs (the error says why).
                    self.canvas = CanvasProfile {
                        width: size.width,
                        height: size.height,
                        fps: whole_fps(fps),
                        ..self.canvas
                    };
                }
                self.preview_offered = preview.is_some();
                out.preview = preview;
            }
            Update::Failed(why) => {
                out.camera = Some(None);
                self.state = 2;
                self.cannot_record = Some(format!("The studio stopped: {why}"));
                // The studio's shutdown ended the vertical outputs too.
                self.vertical_rec = VerticalOut::Off;
                self.vertical_st = VerticalOut::Off;
                if self.rec_state != rec_state::OFF {
                    // The studio's shutdown closed the file.
                    self.rec_state = rec_state::OFF;
                    if let Some((p, _)) = self.rec_file.take() {
                        self.set_note(
                            format!(
                                "The recording stopped: {why}. What was recorded is in {}.",
                                p.display()
                            ),
                            true,
                        );
                    }
                }
                if self.st_state != st_state::OFF {
                    self.st_state = st_state::OFF;
                    self.st_live = None;
                    self.st_link = Link::default();
                    self.st_note = format!("The stream stopped: {why}");
                    self.st_bad = true;
                }
                self.failure = why.clone();
                out.error = Some(format!("The studio stopped: {why}"));
            }
            Update::Heard(rows) => self.set_heard(&rows),
            Update::ScenePreview(endpoint) => {
                self.scene_preview_ready = false;
                self.scene_preview_failed = false;
                out.scene_preview = Some(endpoint);
            }
            Update::VerticalPreview(endpoint) => {
                self.vertical_preview_ready = false;
                self.vertical_preview_failed = false;
                out.vertical_preview = Some(endpoint);
            }
            Update::VerticalPictureFailed => self.vertical_picture_failed = true,
            Update::Sources(rows) => {
                self.set_rows(rows);
                out.focus_shape = self.focus_shape.take();
                self.refresh_existing();
                self.refresh_marks();
            }
            Update::Vertical(v) => {
                if v.is_none() {
                    self.settings_canvas = OutputCanvas::Main;
                    self.folder_serial += 1;
                    self.server_serial += 1;
                    self.vertical_preview_ready = false;
                    self.vertical_preview_failed = false;
                } else if self.vertical.is_none() && self.vertical_known {
                    out.vertical_added = true;
                }
                self.vertical = v;
                self.vertical_known = true;
            }
            Update::MasterMeter(level) => {
                self.master_slot = level.map(|l| (l, Meter::new()));
                self.show_master();
            }
            Update::Scenes(list) => {
                if self.master_audio != list.master {
                    self.master_audio = list.master;
                    self.show_master();
                }
                self.glide = list.glide;
                let data: Vec<SceneData> = list
                    .scenes
                    .iter()
                    .enumerate()
                    .map(|(i, s)| SceneData {
                        id: s.id.get() as i32,
                        name: s.name.as_str().into(),
                        live: s.id == list.program,
                        preview: Some(s.id) == list.preview,
                        number: i as i32 + 1,
                    })
                    .collect();
                set_rows(&self.scenes, data);
                let studio = list.preview.is_some();
                if studio != self.studio_mode {
                    self.studio_mode = studio;
                    out.studio_mode_started = studio;
                    // The picture handles move between the panels.
                    self.selected = -1;
                    self.refresh_marks();
                }
                self.scene_list = Some(list);
                self.refresh_existing();
                self.refresh_global_existing();
            }
            Update::SourceSettings(view) => {
                let (data, choices) = source_settings_data(&view);
                self.source_settings.set_vec(vec![data]);
                self.source_settings_view = Some(choices);
            }
            Update::RegionPreview {
                id,
                generation: _,
                picture,
            } => {
                if let Some(mut row) = self.source_settings.row_data(0)
                    && row.id == id.get() as i32
                {
                    if let Some(grab) = picture {
                        if row.pic_w <= 0 || row.pic_h <= 0 {
                            row.pic_w = grab.width() as i32;
                            row.pic_h = grab.height() as i32;
                        }
                        let buffer =
                            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                grab.rgba(),
                                grab.width(),
                                grab.height(),
                            );
                        row.region_preview = slint::Image::from_rgba8(buffer);
                        row.has_region_preview = true;
                    } else {
                        row.has_region_preview = false;
                    }
                    self.source_settings.set_row_data(0, row);
                }
            }
            Update::Targets(kind, list) => {
                if let Some(p) = &mut self.add_picker
                    && p.answer(kind, &list)
                {
                    self.picker_dirty = true;
                }
                let picked = self.kind_list.get(self.global_kind as usize).copied();
                if picked == Some(kind) {
                    self.global_targets = list.clone();
                    self.refresh_global_existing();
                }
                // The Source tab's list is for the source whose kind this is.
                let for_kind = self
                    .source_targets_for
                    .and_then(|id| self.rows.iter().find(|r| r.id == id))
                    .and_then(|r| r.kind);
                if for_kind == Some(kind) {
                    self.source_target_list = list.clone();
                    self.refresh_source_targets();
                }
            }
            Update::Deleted { name, from_scene } => {
                self.deleted = name;
                self.deleted_from_scene = from_scene;
                self.deleted_serial += 1;
            }
            Update::Error(e) => {
                self.adding = false;
                out.error = Some(e);
            }
            Update::History { undo, redo } => {
                // The toast undoes only the delete it names: once another edit (also
                // one from a control API client) or an undo changed what is newest,
                // it goes.
                let still = undo
                    .as_ref()
                    .is_some_and(|s| s.kind == EditKind::Delete && s.name == self.deleted);
                if !still {
                    self.deleted.clear();
                }
                self.next_undo = undo;
                self.next_redo = redo;
            }
            Update::Recording(r) => self.recording(r, &mut out),
            Update::Stream(u) => self.stream_update(u, &mut out),
            // The vertical canvas's outputs start from the frame's Record and Go live
            // menus or through the control API (protocol 1.29): what went wrong is a
            // toast, and whether they run drives the menus, the 9:16 badge and the
            // profile switch.
            Update::VerticalRecording(r) => {
                match &r {
                    RecordingUpdate::Started { .. } => self.vertical_rec = VerticalOut::On,
                    RecordingUpdate::Stopped { .. } => self.vertical_rec = VerticalOut::Off,
                    // A refused start leaves a running one running.
                    RecordingUpdate::NotStarted(_) => {
                        if self.vertical_rec == VerticalOut::Starting {
                            self.vertical_rec = VerticalOut::Off;
                        }
                    }
                }
                let why = match r {
                    RecordingUpdate::NotStarted(why) => Some(why),
                    RecordingUpdate::Stopped { failure, .. } => failure,
                    _ => None,
                };
                if let Some(why) = why {
                    out.error = Some(vertical_why(&why));
                }
            }
            Update::VerticalStream(u) => {
                match &u {
                    StreamUpdate::Started { .. } => self.vertical_st = VerticalOut::On,
                    StreamUpdate::Ended { .. } => self.vertical_st = VerticalOut::Off,
                    StreamUpdate::NotStarted(_) | StreamUpdate::NoKey => {
                        if self.vertical_st == VerticalOut::Starting {
                            self.vertical_st = VerticalOut::Off;
                        }
                    }
                    StreamUpdate::Link(_) => {}
                }
                let why = match u {
                    StreamUpdate::NotStarted(why) => Some(why),
                    StreamUpdate::NoKey => {
                        Some("no stream key is saved for the vertical stream".to_owned())
                    }
                    StreamUpdate::Ended { failure, .. } => failure,
                    _ => None,
                };
                if let Some(why) = why {
                    out.error = Some(vertical_why(&why));
                }
            }
            Update::CollectionSwitched { seq, result } => {
                if let Some(e) = self.collection_switched(seq, result) {
                    out.error = Some(e);
                }
            }
            Update::VerticalStreamKey(r) => match r {
                Ok(saved) => {
                    let v = self.vertical_stream.get_or_insert_with(StreamProfile::new);
                    v.key_saved = saved;
                    self.key_serial += 1;
                    self.save_vertical_stream();
                }
                Err(why) => self.stream_note = why,
            },
            Update::StreamKey(r) => match r {
                Ok(saved) => {
                    self.stream.key_saved = saved;
                    self.key_serial += 1;
                    self.save_stream();
                }
                Err(why) => self.stream_note = why,
            },
            Update::Profile(p) => {
                // A control API client changed the settings: show and save them.
                self.profile = p;
                self.folder_serial += 1;
                self.save_profile();
            }
            Update::StreamProfile(p) => {
                // A control API client changed the stream settings: show and save
                // them. It cannot change the server or the key, so a server being
                // typed here, or a key saved meanwhile, stays.
                self.stream.reconnect = p.reconnect;
                self.stream.srt_latency_ms = p.srt_latency_ms;
                self.stream.encoding = p.encoding;
                self.stream.auto_record = p.auto_record;
                self.save_stream();
            }
            // `rbs-cli token` over local IPC made or revoked a token; the control
            // thread gave it to the server already. Only the list shown changes here.
            Update::TokensChanged => self.read_tokens(false),
            // `profile.switch` or `collection.switch`: the caller switches, since a
            // collection switch needs the window's preview.
            Update::SwitchRequested {
                seq,
                collection,
                id,
            } => {
                self.api_switch = Some(seq);
                out.switch = Some((collection, id));
            }
            Update::VirtualCamera(status) => out.camera = Some(status),
            // The replay buffer's page in the shell takes these (ADR-0088).
            Update::Replay(_) | Update::ReplaySaved(_) => out.replay.push(u),
            Update::Stats(stats) => {
                let rows: Vec<StatRow> = stat_lines(&stats)
                    .into_iter()
                    .map(|l| StatRow {
                        heading: l.heading,
                        label: l.label.into(),
                        value: l.value.into(),
                        bad: l.bad,
                    })
                    .collect();
                self.stats.set_vec(rows);
            }
        }
        out
    }

    fn recording(&mut self, r: RecordingUpdate, out: &mut Applied) {
        match r {
            RecordingUpdate::Started {
                path,
                video_encoder,
                audio_encoder,
            } => {
                log(&format!(
                    "recording to {} (video: {video_encoder}, audio: {})",
                    path.display(),
                    audio_encoder.as_deref().unwrap_or("none")
                ));
                self.rec_state = rec_state::RECORDING;
                self.rec_file = Some((path, Instant::now()));
                self.set_note(format!("Video encoder: {video_encoder}"), false);
            }
            // A recording started with the stream (or by a control API client) runs
            // while Record was on its way: the recording that runs is shown already.
            RecordingUpdate::NotStarted(why) if self.rec_state != rec_state::STARTING => {
                log(&format!("record refused: {why}"));
            }
            RecordingUpdate::NotStarted(why) => {
                self.rec_state = rec_state::OFF;
                self.rec_file = None;
                self.set_note(format!("The recording did not start: {why}"), true);
            }
            RecordingUpdate::Stopped {
                path,
                failure,
                frames,
                dropped,
                flush_lost,
            } => {
                self.rec_state = rec_state::OFF;
                self.rec_file = None;
                log(&format!(
                    "recording stopped: {frames} frames, {dropped} dropped, {flush_lost} packets lost in the encoder, failure: {failure:?}"
                ));
                match failure {
                    None => {
                        let mut note = format!("Saved {}", path.display());
                        if dropped > 0 {
                            note.push_str(&format!(
                                " ({dropped} of {} frames dropped)",
                                frames + dropped
                            ));
                        }
                        self.set_note(note, false);
                    }
                    Some(why) => {
                        let text = format!(
                            "The recording stopped: {why}. What was recorded is in {}.",
                            path.display()
                        );
                        out.error = Some(text.clone());
                        self.set_note(text, true);
                    }
                }
            }
        }
    }

    /// Shows the rows again (a selection inside them changed).
    fn refresh_rows(&mut self) {
        let rows = self.rows.clone();
        self.set_rows(rows);
    }

    fn set_rows(&mut self, rows: Vec<SourceRow>) {
        // Layers move within their group: the global sources, then the scene's.
        let pictures = |global: bool| -> Vec<usize> {
            (0..rows.len())
                .filter(|i| rows[*i].layout.is_some() && rows[*i].global == global)
                .collect()
        };
        let (global_pictures, scene_pictures) = (pictures(true), pictures(false));
        self.filter_ui.retain(&rows);
        let sounds = filters::sound_sources(&rows);
        let data: Vec<SourceData> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (status, ok) = match &r.status {
                    SourceStatus::Running => ("Running".to_owned(), true),
                    SourceStatus::Idle => ("Not shown, closed".to_owned(), true),
                    SourceStatus::Failed(why) => (format!("Not running: {why}"), false),
                    SourceStatus::Unavailable { fault, .. } => {
                        (unavailable_text(*fault, r.kind).to_owned(), false)
                    }
                };
                let (x, y, w, h) = r.layout.map_or((0.0, 0.0, 0.0, 0.0), |l| {
                    (l.position[0], l.position[1], l.size[0], l.size[1])
                });
                let style = r.layout.map(|l| l.style).unwrap_or_default();
                let crop = r.layout.map(|l| l.crop).unwrap_or_default();
                let (shape, radius) = match style.shape {
                    Shape::Circle => (1, 0.0),
                    Shape::Ellipse => (2, 0.0),
                    Shape::Triangle => (3, 0.0),
                    Shape::Arrow => (4, 0.0),
                    Shape::Rectangle { radius } => (0, radius),
                    _ => (0, 0.0),
                };
                // Turning a border on starts from these.
                let border = style
                    .border
                    .unwrap_or(Border::new(DEFAULT_BORDER_WIDTH, [255, 255, 255, 255]));
                let [cr, cg, cb, ca] = border.color;
                let (filters, selected_filter) = self.filter_ui.data(r, &sounds);
                let pictures = if r.global {
                    &global_pictures
                } else {
                    &scene_pictures
                };
                SourceData {
                    id: r.id.get() as i32,
                    name: r.name.as_str().into(),
                    kind: r.kind.map_or("Unknown type", SourceKind::label).into(),
                    kind_code: r
                        .kind
                        .and_then(|k| SourceKind::ALL.iter().position(|a| *a == k))
                        .map_or(-1, |i| i as i32),
                    icon: source_icon(r.kind).into(),
                    status: status.into(),
                    ok,
                    picture: r.layout.is_some(),
                    visible: r.visible,
                    x,
                    y,
                    w,
                    h,
                    // Rows are top first.
                    can_up: r.layout.is_some() && pictures.first().is_some_and(|f| *f < i),
                    can_down: r.layout.is_some() && pictures.last().is_some_and(|l| *l > i),
                    shape,
                    radius,
                    border: style.border.is_some(),
                    border_width: border.width,
                    border_color: color_text(border.color).into(),
                    border_swatch: slint::Color::from_argb_u8(ca, cr, cg, cb),
                    crop_l: crop.left as i32,
                    crop_t: crop.top as i32,
                    crop_r: crop.right as i32,
                    crop_b: crop.bottom as i32,
                    pic_w: r.picture.map_or(0, |p| p.0 as i32),
                    pic_h: r.picture.map_or(0, |p| p.1 as i32),
                    locked: r.locked,
                    flip_h: r.layout.is_some_and(|l| l.flip_state().horizontal),
                    flip_v: r.layout.is_some_and(|l| l.flip_state().vertical),
                    rotation: r.layout.map_or(0.0, |l| l.rotation),
                    audio: r.kind.is_some_and(SourceKind::is_audio),
                    muted: r.audio.muted,
                    volume: fader::position_of(r.audio.volume),
                    volume_text: fader::volume_text(r.audio.volume).into(),
                    filters,
                    selected_filter,
                    sync_offset: r.audio.sync_offset_ms,
                    global: r.global,
                    also_in: r.also_in.join(", ").into(),
                    in_globals: r.in_globals,
                    places: (1 + r.also_in.len() + usize::from(r.in_globals)) as i32,
                    companion: rows.iter().any(|c| c.companion_of == Some(r.id)),
                    custom_glide: r.glide.is_some(),
                    glide_duration: r.glide.map(|g| g.duration_ms as i32).unwrap_or(400),
                    glide_easing: r.glide.map_or(1, |g| {
                        if g.easing == rbs_scene::GlideEasing::Linear {
                            0
                        } else {
                            1
                        }
                    }),
                    solid: r.kind == Some(SourceKind::Color),
                    fill: {
                        let text = if r.fill.is_empty() {
                            "#ffffff"
                        } else {
                            r.fill.as_str()
                        };
                        text.into()
                    },
                    fill_swatch: {
                        let text = if r.fill.is_empty() {
                            "#ffffff"
                        } else {
                            r.fill.as_str()
                        };
                        let [red, green, blue, alpha] =
                            rbs_api::types::parse_color(text).unwrap_or([255, 255, 255, 255]);
                        slint::Color::from_argb_u8(alpha, red, green, blue)
                    },
                    filled: r.filled,
                    item_id: r.item.get() as i32,
                    cap_has: r.cap_has,
                    cap_l: r.cap_l as i32,
                    cap_t: r.cap_t as i32,
                    cap_r: r.cap_r as i32,
                    cap_b: r.cap_b as i32,
                    chrome_on: r.chrome_on,
                    chrome_t: r.chrome_t as i32,
                    chrome_r: r.chrome_r as i32,
                    chrome_b: r.chrome_b as i32,
                    chrome_l: r.chrome_l as i32,
                    chrome_fill: r.chrome_fill as i32,
                    has_work_rect: r.has_work_rect,
                    show_for_ms: r.show_for_ms as i32,
                    cue_remaining_ms: r.cue_remaining_ms as i32,
                }
            })
            .collect();
        // A source just added is selected, as the add-source flow ends (ui-design-system
        // spec, "Add source"); a selected source that is gone is not.
        let old: Vec<i32> = self.sources.iter().map(|d| d.id).collect();
        let mut new = data.iter().filter(|d| !old.contains(&d.id));
        if let (true, Some(added), None) = (self.adding, new.next(), new.next()) {
            self.selected = added.id;
            self.adding = false;
            if self.reveal_shape {
                self.focus_shape = Some(added.id);
                self.reveal_shape = false;
            }
        } else if !data.iter().any(|d| d.id == self.selected) {
            self.selected = -1;
        }
        set_rows(&self.sources, data);
        self.rows = rows;
        // The Source tab marks the source's own target: it may have changed.
        self.refresh_source_targets();
    }

    /// The Mixer: what is heard, the program scene's audio (ADR-0061; the edited scene's
    /// outside studio mode).
    fn set_heard(&mut self, rows: &[SourceRow]) {
        // Meters: one per playing audio source, in list order; ballistics survive for
        // sources that stay.
        self.heard = rows
            .iter()
            .filter(|r| r.meter.is_some())
            .map(|r| HeardFact {
                id: r.id.get(),
                muted: r.audio.muted,
                hidden: r.audio.mixer_hidden,
                monitor: r.audio.monitor,
            })
            .collect();
        self.heard_names = rows
            .iter()
            .filter(|r| r.meter.is_some())
            .map(|r| (r.id.get(), r.name.as_str().to_owned()))
            .collect();
        let mut old: Vec<MeterSlot> = std::mem::take(&mut self.slots);
        for r in rows.iter().filter(|r| r.visible && !r.audio.mixer_hidden) {
            let Some(level) = &r.meter else { continue };
            let slot = match old.iter().position(|s| s.source == r.id) {
                Some(i) if Arc::ptr_eq(&old[i].level, level) => old.swap_remove(i),
                _ => MeterSlot {
                    source: r.id,
                    level: Arc::clone(level),
                    ballistics: Meter::new(),
                },
            };
            self.slots.push(slot);
        }
        let meters: Vec<MeterRow> = rows
            .iter()
            .filter(|r| !r.audio.mixer_hidden && mixer_source(r))
            .map(|r| {
                let live = r.meter.is_some();
                MeterRow {
                    id: r.id.get() as i32,
                    muted: r.audio.muted,
                    volume: fader::position_of(r.audio.volume),
                    volume_text: if live {
                        fader::volume_text(r.audio.volume).into()
                    } else {
                        "Not running".into()
                    },
                    name: r.name.as_str().into(),
                    icon: source_icon(r.kind).into(),
                    value: "silent".into(),
                    global: r.global,
                    monitor: monitor::mode_code(r.audio.monitor),
                    solo: self.is_soloed(r.id.get()),
                    live,
                    ..MeterRow::default()
                }
            })
            .collect();
        set_rows(&self.meters, meters);
        // Hidden sources are heard but not metered: names only (ADR-0080).
        let hidden: Vec<MeterRow> = rows
            .iter()
            .filter(|r| r.audio.mixer_hidden && mixer_source(r))
            .map(|r| MeterRow {
                id: r.id.get() as i32,
                muted: r.audio.muted,
                name: r.name.as_str().into(),
                icon: source_icon(r.kind).into(),
                global: r.global,
                ..MeterRow::default()
            })
            .collect();
        set_rows(&self.hidden_meters, hidden);
    }

    /// True while meters need refreshing.
    pub fn has_meters(&self) -> bool {
        !self.slots.is_empty() || self.master_slot.is_some()
    }

    /// The master strip's row: its name, fader and mute from the model, its meter
    /// parts from the last refresh (ADR-0081). One row while the studio has a mixer.
    fn show_master(&mut self) {
        if self.master_slot.is_none() {
            self.master_meter.set_vec(Vec::new());
            return;
        }
        let m = self.master_audio;
        let mut row = self.master_meter.row_data(0).unwrap_or_default();
        row.muted = m.muted;
        row.volume = fader::position_of(m.volume);
        row.volume_text = fader::volume_text(m.volume).into();
        row.icon = "speaker".into();
        row.live = true;
        if self.master_meter.row_count() == 0 {
            self.master_meter.set_vec(vec![row]);
        } else {
            self.master_meter.set_row_data(0, row);
        }
    }

    /// The master volume from the fader position (0 to 1, the fader law of the strips).
    pub fn set_master_volume(&mut self, position: f32) {
        self.send(Command::SetMasterVolume {
            volume: fader::volume_at(position),
        });
    }

    /// Mutes or unmutes the master.
    pub fn set_master_muted(&mut self, muted: bool) {
        self.send(Command::SetMasterMuted { muted });
    }

    /// Clears the master's clip latch.
    pub fn reset_master_clip(&mut self) {
        if let Some((_, ballistics)) = &mut self.master_slot {
            ballistics.reset_clip();
        }
    }

    /// The live levels of a filter's meter, as bar positions (see `FilterUi::levels`).
    pub fn filter_levels(&self, filter: i32) -> (f32, f32) {
        self.filter_ui.levels(filter)
    }

    /// One meter refresh: takes each source's peaks since the last call.
    pub fn tick_meters(&mut self) {
        let now = self.meter_start.elapsed();
        self.meter_ticks = self.meter_ticks.wrapping_add(1);
        let report = self.meter_ticks.is_multiple_of(REPORT_EVERY);
        for s in self.slots.iter_mut() {
            let levels = s.level.take();
            let db = (0..levels.channels())
                .map(|ch| levels.peak_db(ch))
                .fold(f32::NEG_INFINITY, f32::max);
            s.ballistics.update(db, now);
            let view = s.ballistics.view(now);
            let id = s.source.get() as i32;
            let value = report.then(|| s.ballistics.take_report().english());
            let count = self.meters.row_count();
            for i in 0..count {
                let Some(mut row) = self.meters.row_data(i) else {
                    continue;
                };
                if row.id != id {
                    continue;
                }
                paint(&mut row, &view);
                if let Some(text) = &value {
                    row.value = text.as_str().into();
                }
                self.meters.set_row_data(i, row);
                break;
            }
        }
        if let Some((level, ballistics)) = &mut self.master_slot {
            let levels = level.take();
            let db = (0..levels.channels())
                .map(|ch| levels.peak_db(ch))
                .fold(f32::NEG_INFINITY, f32::max);
            ballistics.update(db, now);
            let view = ballistics.view(now);
            if let Some(mut row) = self.master_meter.row_data(0) {
                paint(&mut row, &view);
                if report {
                    row.value = ballistics.take_report().english().into();
                }
                self.master_meter.set_row_data(0, row);
            }
        }
    }
}

/// A Mixer row: a shown audio source, including one whose device is not running yet.
/// Picture sources stay in the Sources list only. A source hidden with Show/Hide in
/// Sources is left out until it is shown again; that is the scene item, not
/// `mixer_hidden` (ADR-0080), which still lists the source under Hidden.
fn mixer_source(r: &SourceRow) -> bool {
    r.visible && (r.meter.is_some() || r.kind.is_some_and(SourceKind::is_audio))
}

/// Copies one meter view into a row: the zones, the peak hold and its zone, the clip.
fn paint(row: &mut MeterRow, view: &rbs_ui::meter::MeterView) {
    let [lo, mi, hi] = view.zones();
    row.lo_from = lo.0;
    row.lo_to = lo.1;
    row.mi_from = mi.0;
    row.mi_to = mi.1;
    row.hi_from = hi.0;
    row.hi_to = hi.1;
    row.peak = view.peak;
    row.peak_zone = if view.peak >= meter::position(meter::HIGH_FROM_DB) {
        2
    } else if view.peak >= meter::position(meter::MID_FROM_DB) {
        1
    } else {
        0
    };
    row.clipped = view.clipped;
}

/// What [`StudioUi::apply`] asks of the shell.
/// The Add source dialog's content ([`StudioUi::take_picker_view`]).
pub struct PickerView {
    pub title: String,
    pub status: add_picker::Status,
    pub rows: Vec<add_picker::Row>,
    /// Show "Capture audio too" (displays and windows) and whether it is checked.
    pub offers_audio: bool,
    /// The dialog offers "Add its microphone too" (a camera with a microphone of its own).
    pub offers_mic: bool,
    pub audio: bool,
}

#[derive(Default)]
pub struct Applied {
    pub error: Option<String>,
    pub preview: Option<rbs_gpu::share::PreviewEndpoint>,
    /// The preview scene's picture endpoint changed (studio mode, ADR-0061): connect to
    /// it, or close that picture for `None`.
    pub scene_preview: Option<Option<rbs_gpu::share::PreviewEndpoint>>,
    /// The vertical canvas's picture endpoint changed (ADR-0093): connect to it, or
    /// close that picture for `None`.
    pub vertical_preview: Option<Option<rbs_gpu::share::PreviewEndpoint>>,
    /// A vertical canvas was just added: a closed Vertical panel opens.
    pub vertical_added: bool,
    /// Studio mode just started: a closed Preview panel opens (UI spec, Studio mode).
    pub studio_mode_started: bool,
    /// A plain frame was just added and selected: open Properties on its border.
    pub focus_shape: Option<i32>,
    /// A control API client asked to switch: scene collections (`true`) or output
    /// profiles to the entry with this id. The caller switches as the File menu
    /// does and then calls [`StudioUi::refresh_library`], which answers the host.
    pub switch: Option<(bool, String)>,
    /// The virtual camera started or stopped sending (`Some(None)`: it does not send).
    pub camera: Option<Option<rbs_app::studio_host::CameraStatus>>,
    /// Replay buffer updates (ADR-0088), for the shell's replay page.
    pub replay: Vec<Update>,
}

/// A library as `profile.list` and `collection.list` show it.
fn api_library(list: &[(EntryId, String)], current: &EntryId) -> rbs_api::types::Library {
    rbs_api::types::Library {
        entries: list
            .iter()
            .map(|(id, name)| rbs_api::types::LibraryEntry {
                id: id.as_str().to_owned(),
                name: name.clone(),
            })
            .collect(),
        current: current.as_str().to_owned(),
        pending: false,
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// A field's labels and the index in use, for the window.
fn choices<T: Clone + PartialEq>(c: &Choices<T>) -> (ModelRc<SharedString>, i32) {
    (strings(c.labels()), c.index as i32)
}

fn kbps(k: u32) -> String {
    format!("{k} kbit/s")
}

/// A free display name for a new scene collection, based on `wanted` (`Default`, then
/// `Default 2`, …), matching import's numbering.
fn free_collection_name(user: &UserSession, wanted: &str) -> String {
    let names: Vec<String> = user.collections().into_iter().map(|(_, n)| n).collect();
    let free = |n: &str| !names.iter().any(|m| m.eq_ignore_ascii_case(n));
    if free(wanted) {
        return wanted.to_owned();
    }
    (2..u32::MAX)
        .map(|n| format!("{wanted} {n}"))
        .find(|n| free(n))
        .unwrap_or_else(|| format!("{wanted} new"))
}

/// One canvas tile. `title` is a broadcast name or "This display".
struct CanvasTile {
    title: &'static str,
    detail: String,
    width: u32,
    height: u32,
}

struct SizeNestModel {
    bands: Vec<SizeBand>,
    extras: Vec<SizeBand>,
    index: i32,
    value: String,
    same: bool,
}

impl StudioUi {
    fn canvas_nest(&self) -> SizeNestModel {
        let mut tiles: Vec<(String, String, u32, u32, Option<f32>)> = self
            .canvas_tiles()
            .into_iter()
            .map(|tile| {
                (
                    tile.title.to_owned(),
                    tile.detail,
                    tile.width,
                    tile.height,
                    canvas_profile::broadcast_scale(tile.width, tile.height),
                )
            })
            .collect();
        let current = (self.canvas.width, self.canvas.height);
        if !tiles.iter().any(|tile| (tile.2, tile.3) == current) {
            tiles.push((
                "Custom".to_owned(),
                format!("{} × {}", current.0, current.1),
                current.0,
                current.1,
                None,
            ));
        }
        let mut bands = Vec::new();
        let mut extras = Vec::new();
        for (title, detail, width, height, scale) in tiles {
            let band = size_band(&title, &detail, width, height, 0, scale.unwrap_or(0.0));
            if scale.is_some() {
                bands.push(band);
            } else {
                extras.push(band);
            }
        }
        bands.sort_by(|a, b| {
            b.scale
                .partial_cmp(&a.scale)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut index = -1;
        let mut value = format!("{} × {}", current.0, current.1);
        for (next, band) in (0..).zip(bands.iter_mut().chain(extras.iter_mut())) {
            band.index = next;
            if (band.picture_w, band.picture_h) == (current.0 as i32, current.1 as i32) {
                index = next;
                value = format!("{} · {}", band.title, band.detail);
            }
        }
        SizeNestModel {
            bands,
            extras,
            index,
            value,
            same: false,
        }
    }
}

fn output_nest(
    options: &[rbs_app::record_profile::OutputSizeOption],
    saved: Option<OutputSize>,
    canvas: (u32, u32),
) -> SizeNestModel {
    let mut named = Vec::new();
    let mut extras = Vec::new();
    for (index, option) in options.iter().enumerate() {
        let Some(size) = option.size else {
            continue;
        };
        let (title, detail) = output_choice_copy(size, canvas);
        let scale = canvas_profile::broadcast_scale(size.width, size.height);
        let band = size_band(
            &title,
            &detail,
            size.width,
            size.height,
            index as i32,
            scale.unwrap_or(0.0),
        );
        if scale.is_some() {
            named.push(band);
        } else {
            extras.push(band);
        }
    }
    named.sort_by(|a, b| {
        b.scale
            .partial_cmp(&a.scale)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let same = saved.is_none();
    let index = if same { -1 } else { size_index(options, saved) };
    let value = if same {
        match canvas_profile::broadcast_name(canvas.0, canvas.1) {
            Some(name) => format!("{name} · {} × {}", canvas.0, canvas.1),
            None => format!("{} × {}", canvas.0, canvas.1),
        }
    } else {
        options
            .iter()
            .find_map(|option| {
                let size = option.size?;
                (size == saved?).then(|| {
                    let (title, detail) = output_choice_copy(size, canvas);
                    format!("{title} · {detail}")
                })
            })
            .unwrap_or_else(|| format!("{} × {}", canvas.0, canvas.1))
    };
    SizeNestModel {
        bands: named,
        extras,
        index,
        value,
        same,
    }
}

fn size_band(
    title: &str,
    detail: &str,
    width: u32,
    height: u32,
    index: i32,
    scale: f32,
) -> SizeBand {
    SizeBand {
        title: title.into(),
        detail: detail.into(),
        picture_w: width as i32,
        picture_h: height as i32,
        index,
        scale,
    }
}

fn output_long_side(size: Option<OutputSize>, canvas: (u32, u32)) -> u32 {
    size.map(|s| s.width.max(s.height))
        .unwrap_or(canvas.0.max(canvas.1))
}

fn whole_side(value: f32) -> Option<u32> {
    if !value.is_finite() || value < 0.0 || value > u32::MAX as f32 {
        return None;
    }
    Some(value.round() as u32)
}

fn choice_model(items: &[SizeBand]) -> ModelRc<SizeBand> {
    ModelRc::from(Rc::new(VecModel::from(items.to_vec())))
}

fn size_index(
    options: &[rbs_app::record_profile::OutputSizeOption],
    saved: Option<OutputSize>,
) -> i32 {
    options.iter().position(|o| o.size == saved).unwrap_or(0) as i32
}

fn strings(items: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        items
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )))
}

/// One source of the scene being edited, as the command registry asks about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFacts {
    /// Its source id (the UI's and the control API's).
    pub id: i32,
    /// Its item in the scene (the control API's `item`).
    pub item: u32,
    pub visible: bool,
    /// The device stays open. False when the user deactivated the source.
    pub active: bool,
    pub locked: bool,
    pub muted: bool,
    /// Kept out of the Mixer (ADR-0080).
    pub mixer_hidden: bool,
    /// Heard on the monitor (ADR-0097): off, also or only.
    pub monitor: rbs_scene::model::SourceMonitor,
    /// Soloed on the monitor.
    pub soloed: bool,
    /// It has a picture (it can be locked on the picture panel).
    pub picture: bool,
    /// It makes sound only (it can be muted in the Mixer).
    pub audio: bool,
    /// It captures something that can be opened again (a screen, window, camera or
    /// audio endpoint), so it can be reloaded (ADR-0077).
    pub reloadable: bool,
    /// It has a Source settings window (screen, window or camera; ADR-0077).
    pub has_settings: bool,
    /// Its picture size is known, so it can be fitted or reset (ADR-0084).
    pub sized: bool,
    /// Its picture is mirrored left to right or top to bottom as seen on the canvas.
    pub flip: rbs_scene::FlipState,
    /// Another source with a picture is above it, so it can move up.
    pub can_up: bool,
    /// Another source with a picture is below it, so it can move down.
    pub can_down: bool,
}

/// One heard source as the Mixer's menus ask about it (ADR-0080).
#[derive(Clone, Copy, Debug)]
struct HeardFact {
    id: u32,
    muted: bool,
    hidden: bool,
    monitor: rbs_scene::model::SourceMonitor,
}

/// Rows are replaced only where they changed, so unchanged rows keep their views.
fn set_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (i, r) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&r) {
            model.set_row_data(i, r);
        }
    }
}

/// The rectangle (dock-area coordinates, logical pixels) of the content of the stack
/// showing panel `id` as its active tab, if one does.
pub fn active_content(
    stacks: &VecModel<StackData>,
    id: &str,
    bar: f32,
    border: f32,
) -> Option<[f32; 4]> {
    stacks.iter().find_map(|s| {
        let tab = s.tabs.row_data(s.active as usize)?;
        (tab.id == id).then(|| {
            [
                s.x + border,
                s.y + bar + border,
                (s.w - 2.0 * border).max(0.0),
                (s.h - bar - 2.0 * border).max(0.0),
            ]
        })
    })
}

/// A line for the developer log (stderr; nothing is shown to the user).
pub fn log(line: &str) {
    #[allow(clippy::print_stderr)]
    {
        eprintln!("rbs: {line}");
    }
}

fn whole_fps(rate: rbs_core::timing::FrameRate) -> u32 {
    let r = rate.per_second();
    r.num().div_ceil(r.den().max(1))
}

/// One line of the Stats panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatLine {
    /// A section heading: `label` only.
    pub heading: bool,
    /// What is counted.
    pub label: String,
    /// The count, in words where it needs them.
    pub value: String,
    /// The value shows a loss.
    pub bad: bool,
}

/// The Stats panel's lines for `stats`: a section per running output, then the
/// engine's late ticks; none while no output runs.
pub fn stat_lines(stats: &Stats) -> Vec<StatLine> {
    let heading = |label: &str| StatLine {
        heading: true,
        label: label.to_owned(),
        value: String::new(),
        bad: false,
    };
    let line = |label: &str, value: String, bad: bool| StatLine {
        heading: false,
        label: label.to_owned(),
        value,
        bad,
    };
    let dropped = |o: &rbs_app::studio_host::OutputStats| {
        let total = o.frames + o.dropped;
        if o.dropped == 0 || total == 0 {
            o.dropped.to_string()
        } else {
            format!(
                "{} ({:.1} %)",
                o.dropped,
                o.dropped as f64 * 100.0 / total as f64
            )
        }
    };
    let mut lines = Vec::new();
    if let Some(o) = &stats.recording {
        lines.push(heading("Recording"));
        lines.push(line("Frames encoded", o.frames.to_string(), false));
        lines.push(line("Frames dropped", dropped(o), o.dropped > 0));
        lines.push(line("Packets written", o.packets.to_string(), false));
    }
    if let Some(o) = &stats.stream {
        lines.push(heading("Stream"));
        lines.push(line(
            "Connection",
            if o.connected {
                "Live".to_owned()
            } else {
                "Not connected".to_owned()
            },
            !o.connected,
        ));
        lines.push(line("Frames encoded", o.frames.to_string(), false));
        lines.push(line("Frames dropped", dropped(o), o.dropped > 0));
        lines.push(line(
            "Dropped while the connection was slow or down",
            o.transport_drops.to_string(),
            o.transport_drops > 0,
        ));
        lines.push(line(
            "Reconnect attempts",
            o.reconnect_attempts.to_string(),
            false,
        ));
    }
    if !lines.is_empty() {
        lines.push(heading("Engine"));
        lines.push(line(
            "Late render ticks",
            stats.late_render_ticks.to_string(),
            stats.late_render_ticks > 0,
        ));
        lines.push(line(
            "Late audio ticks",
            stats.late_audio_ticks.to_string(),
            stats.late_audio_ticks > 0,
        ));
    }
    lines
}

/// A border's width when it is turned on.
const DEFAULT_BORDER_WIDTH: f32 = 4.0;

/// `#rrggbb` for an opaque colour, else `#rrggbbaa`.
fn color_text(c: [u8; 4]) -> String {
    if c[3] == 255 {
        format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
    } else {
        rbs_api::types::format_color(c)
    }
}

/// The Library's looks, in the order the panel lists them (ADR-0075).
fn library_look_names() -> Vec<slint::SharedString> {
    library_looks().into_iter().map(|l| l.name).collect()
}

fn library_looks() -> Vec<LookData> {
    rbs_scene::look::built_in()
        .into_iter()
        .map(|l| LookData {
            id: l.id.into(),
            name: l.name.into(),
            group: match l.group {
                rbs_scene::look::LookGroup::Color => 0,
                rbs_scene::look::LookGroup::Effect => 1,
                _ => 2,
            },
        })
        .collect()
}

/// Pixels of a sample picture's thumbnail, in the aspect of the canvas it is for.
const THUMB: (u32, u32) = (288, 162);

/// How many rows of the Source tab's list get a picture, and the size asked for (twice the
/// token size, so it stays sharp on a high-density screen).
const THUMBS_PER_LIST: usize = 24;
const THUMB_REQUEST: (u32, u32) = (192, 108);

/// The sample pictures with their thumbnails (made once, when the Library first shows).
fn library_samples() -> Vec<SampleData> {
    let mut background = 0i32;
    let mut frame = 0i32;
    rbs_sources::still::SAMPLES
        .iter()
        .filter_map(|s| {
            let px = rbs_sources::still::sample_picture(s.id, THUMB.0, THUMB.1)?;
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &px, THUMB.0, THUMB.1,
            );
            let is_frame = s.kind == rbs_sources::still::SampleKind::Frame;
            // Place within its own kind, so the two columns stay even when the
            // other kind is hidden.
            let slot = if is_frame {
                let n = frame;
                frame += 1;
                n
            } else {
                let n = background;
                background += 1;
                n
            };
            Some(SampleData {
                id: s.id.into(),
                name: s.name.into(),
                frame: is_frame,
                slot,
                thumb: slint::Image::from_rgba8(buffer),
            })
        })
        .collect()
}

/// The icon a source's row shows for its kind (design system icon set).
/// The reason a source is unavailable, in plain words for its kind (ADR-0077).
fn unavailable_text(fault: SourceFault, kind: Option<SourceKind>) -> &'static str {
    use SourceKind as K;
    match (fault, kind) {
        (SourceFault::TargetGone, Some(K::Window)) => "Window closed",
        (SourceFault::TargetGone, Some(K::Screen)) => "Display disconnected",
        (SourceFault::TargetGone, Some(K::Camera)) => "Camera unplugged",
        (SourceFault::TargetGone, Some(K::AudioInput)) => "Microphone not found",
        (SourceFault::TargetGone, Some(K::SystemAudio)) => "Audio output not found",
        (SourceFault::TargetGone, Some(K::ApplicationAudio)) => "Application not running",
        (SourceFault::TargetGone, _) => "Source not found",
        (SourceFault::PermissionDenied, Some(K::Camera)) => "Camera access denied",
        (SourceFault::PermissionDenied, Some(K::AudioInput)) => "Microphone access denied",
        (SourceFault::PermissionDenied, _) => "Access denied",
        (SourceFault::Busy, Some(K::Camera)) => "Camera in use by another app",
        (SourceFault::Busy, _) => "In use by another app",
        _ => "Not available",
    }
}

fn source_icon(kind: Option<SourceKind>) -> &'static str {
    match kind {
        Some(SourceKind::Screen) => "monitor",
        Some(SourceKind::Window) => "window",
        Some(SourceKind::Camera) => "camera",
        Some(SourceKind::AudioInput) => "mic",
        Some(SourceKind::SystemAudio | SourceKind::ApplicationAudio) => "speaker",
        Some(SourceKind::Image) => "image",
        Some(SourceKind::Color) => "palette",
        Some(SourceKind::Media) => "film",
        Some(SourceKind::Browser) => "globe",
        None => "window",
    }
}

/// The Studio global's `undo-kind` / `redo-kind` code and the source's name for a
/// step: 0 nothing, 1 add, 2 delete, 3 layer order, 4 move or resize, 5 shape or
/// border, 6 crop, 7 shown or hidden, 8 lock, 9 source rename, 10 scene added, 11
/// scene removed, 12 scene renamed (the name is the scene's, or the old name), 13
/// filters changed, 14 removed from one scene, 15 made global or moved back into a
/// scene, 16 look applied, 17 source pointed at another target, 18 source settings
/// changed, 19 flipped, turned, fitted or centred, 20 glide duration or easing,
/// 21 how long Show draws a hidden item (the cue itself is not an undo step),
/// 22 the source's capture region.
fn step_view(step: Option<&StepInfo>) -> (i32, &str) {
    match step {
        None => (0, ""),
        Some(s) => (
            match s.kind {
                EditKind::Add => 1,
                EditKind::Delete => 2,
                EditKind::Order => 3,
                EditKind::Place => 4,
                EditKind::Style => 5,
                EditKind::Crop => 6,
                EditKind::Visibility => 7,
                EditKind::Lock => 8,
                EditKind::RenameSource => 9,
                EditKind::AddScene => 10,
                EditKind::RemoveScene => 11,
                EditKind::RenameScene => 12,
                EditKind::Filter => 13,
                EditKind::Remove => 14,
                EditKind::Global => 15,
                EditKind::Look => 16,
                EditKind::Retarget => 17,
                EditKind::SourceSettings => 18,
                EditKind::Transform => 19,
                EditKind::Glide => 20,
                EditKind::ShowFor => 21,
                EditKind::Region => 22,
            },
            s.name.as_str(),
        ),
    }
}

/// A profile section, or its default and why the saved one was not used.
fn section<S: serde::de::DeserializeOwned>(
    user: &UserSession,
    name: &str,
    default: impl Fn() -> S,
    problem: impl Fn(&S) -> Option<String>,
) -> (S, Option<String>) {
    match user.section::<S>(name) {
        Ok(None) => (default(), None),
        Ok(Some(s)) => match problem(&s) {
            None => (s, None),
            Some(why) => (default(), Some(why)),
        },
        Err(why) => (default(), Some(why)),
    }
}

/// The frame rates a screen, a window or a camera mode is offered, lowest first.
const STANDARD_RATES: [u32; 12] = [5, 10, 15, 20, 24, 25, 30, 48, 50, 60, 90, 120];

/// What the choices of the Source settings window stand for.
#[derive(Debug, PartialEq, Eq)]
struct SettingsChoices {
    /// Camera modes: the size of each, in the order listed.
    modes: Vec<(u32, u32)>,
    /// The rates of each mode (one list for a screen or window).
    rates: Vec<Vec<u32>>,
    /// The kind has a cursor choice.
    has_cursor: bool,
}

impl SettingsChoices {
    /// The settings patch for the chosen mode and rate (`-1`: nothing chosen, the stored
    /// value stays) and the cursor.
    fn patch(
        &self,
        mode: i32,
        rate: i32,
        cursor: bool,
    ) -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut patch = std::collections::BTreeMap::new();
        let mode = usize::try_from(mode).ok();
        if let Some((w, h)) = mode.and_then(|m| self.modes.get(m)) {
            patch.insert("width".to_owned(), serde_json::json!(w));
            patch.insert("height".to_owned(), serde_json::json!(h));
        }
        let list = if self.modes.is_empty() {
            self.rates.first()
        } else {
            mode.and_then(|m| self.rates.get(m))
        };
        if let Some(fps) = usize::try_from(rate)
            .ok()
            .and_then(|r| list.and_then(|l| l.get(r)))
        {
            patch.insert("fps".to_owned(), serde_json::json!(fps));
        }
        if self.has_cursor {
            patch.insert("cursor".to_owned(), serde_json::json!(cursor));
        }
        patch
    }
}

/// The rates of a mode that runs from `min` to `max` frames per second, plus the stored
/// one so that it can be shown.
fn rates_between(min: u32, max: u32, stored: Option<u32>) -> Vec<u32> {
    let mut rates: Vec<u32> = STANDARD_RATES
        .into_iter()
        .filter(|r| (min..=max).contains(r))
        .collect();
    if rates.is_empty() {
        rates.push(max.max(1));
    }
    if let Some(s) = stored.filter(|s| !rates.contains(s)) {
        rates.push(s);
        rates.sort_unstable();
    }
    rates
}

fn source_settings_data(view: &SourceSettingsView) -> (SourceSettingsData, SettingsChoices) {
    let camera = view.kind == SourceKind::Camera;
    let (modes, rates): (Vec<(u32, u32)>, Vec<Vec<u32>>) = if camera {
        view.modes
            .iter()
            .map(|m| {
                (
                    (m.width, m.height),
                    rates_between(m.min_fps, m.max_fps, None),
                )
            })
            .unzip()
    } else {
        (Vec::new(), vec![rates_between(15, 60, view.fps)])
    };
    let mode_index = view
        .size
        .and_then(|s| modes.iter().position(|m| *m == s))
        .map_or(-1, |i| i as i32);
    let shown_rates = if camera {
        usize::try_from(mode_index)
            .ok()
            .and_then(|m| rates.get(m))
            .cloned()
            .unwrap_or_default()
    } else {
        rates[0].clone()
    };
    let rate_index = view
        .fps
        .and_then(|f| shown_rates.iter().position(|r| *r == f))
        .map_or(-1, |i| i as i32);
    let strings = |v: &[u32]| -> ModelRc<SharedString> {
        ModelRc::new(VecModel::from(
            v.iter()
                .map(|r| SharedString::from(format!("{r} fps")))
                .collect::<Vec<_>>(),
        ))
    };
    let has_cursor = matches!(view.kind, SourceKind::Screen | SourceKind::Window);
    let data = SourceSettingsData {
        id: view.id.get() as i32,
        name: view.name.as_str().into(),
        kind_code: SourceKind::ALL
            .iter()
            .position(|k| *k == view.kind)
            .map_or(-1, |i| i as i32),
        modes: ModelRc::new(VecModel::from(
            modes
                .iter()
                .zip(&rates)
                .map(|((w, h), r)| ModeData {
                    label: format!("{w} \u{d7} {h}").into(),
                    rates: strings(r),
                })
                .collect::<Vec<_>>(),
        )),
        mode_labels: ModelRc::new(VecModel::from(
            modes
                .iter()
                .map(|(w, h)| SharedString::from(format!("{w} \u{d7} {h}")))
                .collect::<Vec<_>>(),
        )),
        mode_index,
        rates: strings(&shown_rates),
        rate_index,
        has_cursor,
        cursor: view.cursor,
        cap_has: view.cap_has,
        cap_l: view.cap_l as i32,
        cap_t: view.cap_t as i32,
        cap_r: view.cap_r as i32,
        cap_b: view.cap_b as i32,
        chrome_on: view.chrome_on,
        chrome_t: view.chrome_t as i32,
        chrome_r: view.chrome_r as i32,
        chrome_b: view.chrome_b as i32,
        chrome_l: view.chrome_l as i32,
        chrome_fill: view.chrome_fill as i32,
        has_work_rect: view.has_work_rect,
        pic_w: view.pic_w as i32,
        pic_h: view.pic_h as i32,
        has_region_preview: false,
        region_preview: slint::Image::default(),
    };
    (
        data,
        SettingsChoices {
            modes,
            rates,
            has_cursor,
        },
    )
}

/// One row of the Source tab's target list.
#[derive(Debug, PartialEq, Eq)]
struct SourceTargetRow {
    /// Index in the listed targets.
    index: usize,
    /// The source's current target.
    current: bool,
    /// Set on the first row of a group, empty otherwise.
    heading: String,
}

/// The Source tab's list: no pickers (an OS picker needs the add flow), filtered by the
/// lower-case `query`, the current target first, then the recent targets, then the other
/// windows of the same application, then the rest (ADR-0077).
fn source_target_rows(list: &[Target], current: &str, query: &str) -> Vec<SourceTargetRow> {
    let current_app = list
        .iter()
        .find(|t| !current.is_empty() && t.target == current)
        .and_then(|t| t.app.clone());
    let rank = |t: &Target| {
        if !current.is_empty() && t.target == current {
            0
        } else if t.recent.is_some() {
            1
        } else if current_app.is_some() && t.app == current_app {
            2
        } else {
            3
        }
    };
    let mut shown: Vec<(usize, &Target)> = list
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.picker)
        .filter(|(_, t)| query.is_empty() || t.name.to_lowercase().contains(query))
        .collect();
    // Recent targets keep their order of use; the others keep the OS order.
    shown.sort_by_key(|(_, t)| (rank(t), if rank(t) == 1 { t.recent } else { None }));
    let any_grouped = shown.iter().any(|(_, t)| matches!(rank(t), 1 | 2));
    let mut last = 0;
    shown
        .into_iter()
        .map(|(index, t)| {
            let group = rank(t);
            let heading = if group == last {
                String::new()
            } else if group == 1 {
                "Recent".to_owned()
            } else if group == 2 {
                format!(
                    "Same application: {}",
                    current_app.as_deref().unwrap_or_default()
                )
            } else if any_grouped {
                "Other".to_owned()
            } else {
                String::new()
            };
            last = group;
            SourceTargetRow {
                index,
                current: group == 0,
                heading,
            }
        })
        .collect()
}

/// Which source Properties shows. The selection wins; otherwise the last source
/// the panel showed, if it is still in `ids`; otherwise the first source.
fn properties_source(selected: i32, remembered: i32, ids: &[i32]) -> i32 {
    if selected >= 0 && ids.contains(&selected) {
        return selected;
    }
    if remembered >= 0 && ids.contains(&remembered) {
        return remembered;
    }
    ids.first().copied().unwrap_or(-1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn window(name: &str, app: Option<&str>) -> Target {
        Target {
            name: name.to_owned(),
            adapter: "test".to_owned(),
            target: name.to_owned(),
            is_default: false,
            size: None,
            app: app.map(str::to_owned),
            recent: None,
            picker: false,
            mic: None,
            sound: true,
        }
    }

    #[test]
    fn properties_keeps_the_last_source_when_nothing_is_selected() {
        let ids = [4, 7, 9];
        assert_eq!(properties_source(7, 4, &ids), 7);
        assert_eq!(properties_source(-1, 7, &ids), 7);
        // The remembered source was removed: the first remaining source fills the panel.
        assert_eq!(properties_source(-1, 3, &ids), 4);
        assert_eq!(properties_source(-1, -1, &ids), 4);
        assert_eq!(properties_source(-1, 7, &[]), -1);
    }

    #[test]
    fn the_source_tab_lists_the_current_target_then_its_application_then_the_rest() {
        let list = [
            window("Other 1", Some("Other")),
            window("Editor A", Some("Editor")),
            window("Editor B", Some("Editor")),
            window("Plain", None),
        ];
        let rows = source_target_rows(&list, "Editor A", "");
        let order: Vec<_> = rows.iter().map(|r| r.index).collect();
        assert_eq!(order, [1, 2, 0, 3]);
        assert!(rows[0].current && rows[0].heading.is_empty());
        assert_eq!(rows[1].heading, "Same application: Editor");
        assert!(rows[2].heading == "Other" && rows[3].heading.is_empty());
    }

    fn camera_view(size: Option<(u32, u32)>, fps: Option<u32>) -> SourceSettingsView {
        SourceSettingsView {
            id: SourceId::from_raw(4),
            name: "Cam".to_owned(),
            kind: SourceKind::Camera,
            modes: vec![
                rbs_sources::SourceMode {
                    width: 1920,
                    height: 1080,
                    min_fps: 30,
                    max_fps: 60,
                },
                rbs_sources::SourceMode {
                    width: 640,
                    height: 360,
                    min_fps: 15,
                    max_fps: 30,
                },
            ],
            size,
            fps,
            cursor: true,
            cap_has: false,
            cap_l: 0,
            cap_t: 0,
            cap_r: 0,
            cap_b: 0,
            chrome_on: false,
            chrome_t: 0,
            chrome_r: 0,
            chrome_b: 0,
            chrome_l: 0,
            chrome_fill: 0,
            has_work_rect: false,
            pic_w: 0,
            pic_h: 0,
        }
    }

    #[test]
    fn a_camera_offers_its_modes_and_the_rates_of_the_chosen_one() {
        let (data, choices) = source_settings_data(&camera_view(Some((640, 360)), Some(24)));
        assert_eq!(data.mode_index, 1);
        // 640 x 360 runs at 15..=30: 15, 20, 24, 25, 30; the stored 24 is the fourth entry... third.
        assert_eq!(data.rate_index, 2);
        assert_eq!(data.rates.row_count(), 5);
        assert!(!data.has_cursor);
        // Choosing the first mode and its first rate sets size and rate, not the cursor.
        let patch = choices.patch(0, 0, true);
        assert_eq!(patch["width"], 1920);
        assert_eq!(patch["height"], 1080);
        assert_eq!(patch["fps"], 30);
        assert!(!patch.contains_key("cursor"));
    }

    #[test]
    fn nothing_chosen_leaves_the_stored_values() {
        let (data, choices) = source_settings_data(&camera_view(None, None));
        assert_eq!((data.mode_index, data.rate_index), (-1, -1));
        assert!(choices.patch(-1, -1, true).is_empty());
    }

    #[test]
    fn a_screen_has_rates_and_a_cursor_choice_and_shows_a_stored_odd_rate() {
        let view = SourceSettingsView {
            id: SourceId::from_raw(1),
            name: "Screen".to_owned(),
            kind: SourceKind::Screen,
            modes: Vec::new(),
            size: None,
            fps: Some(144),
            cursor: false,
            cap_has: false,
            cap_l: 0,
            cap_t: 0,
            cap_r: 0,
            cap_b: 0,
            chrome_on: false,
            chrome_t: 0,
            chrome_r: 0,
            chrome_b: 0,
            chrome_l: 0,
            chrome_fill: 0,
            has_work_rect: true,
            pic_w: 3440,
            pic_h: 1440,
        };
        let (data, choices) = source_settings_data(&view);
        assert!(data.has_cursor && !data.cursor);
        let last = data.rates.row_count() - 1;
        assert_eq!(data.rate_index as usize, last, "144 is kept and shown last");
        let patch = choices.patch(-1, data.rate_index, true);
        assert_eq!(patch["fps"], 144);
        assert_eq!(patch["cursor"], true);
    }

    #[test]
    fn recent_targets_come_after_the_current_one_in_the_order_of_use() {
        let mut older = window("Older", None);
        older.recent = Some(1);
        let mut newer = window("Newer", None);
        newer.recent = Some(0);
        let list = [
            window("Cur", Some("Editor")),
            window("Plain", None),
            older,
            newer,
        ];
        let rows = source_target_rows(&list, "Cur", "");
        let order: Vec<_> = rows.iter().map(|r| r.index).collect();
        assert_eq!(order, [0, 3, 2, 1]);
        assert_eq!(rows[1].heading, "Recent");
        assert!(rows[2].heading.is_empty());
        assert_eq!(rows[3].heading, "Other");
    }

    #[test]
    fn without_an_application_the_list_has_no_headings() {
        let list = [window("A", None), window("B", None)];
        let rows = source_target_rows(&list, "B", "");
        assert_eq!(rows.iter().map(|r| r.index).collect::<Vec<_>>(), [1, 0]);
        assert!(rows.iter().all(|r| r.heading.is_empty()));
    }

    #[test]
    fn the_search_filters_and_pickers_are_left_out() {
        let mut picker = window("Choose a window…", None);
        picker.picker = true;
        let list = [picker, window("Alpha", None), window("Beta", None)];
        let rows = source_target_rows(&list, "", "bet");
        assert_eq!(rows.iter().map(|r| r.index).collect::<Vec<_>>(), [2]);
    }
    use rbs_app::stream_key::MemoryKeyStore;

    #[test]
    fn stats_show_each_running_output_and_mark_losses() {
        use rbs_app::studio_host::OutputStats;
        assert!(stat_lines(&Stats::default()).is_empty(), "nothing runs");
        let stats = Stats {
            recording: Some(OutputStats {
                frames: 999,
                dropped: 1,
                packets: 1500,
                ..OutputStats::default()
            }),
            stream: Some(OutputStats {
                frames: 600,
                connected: true,
                ..OutputStats::default()
            }),
            late_render_ticks: 0,
            late_audio_ticks: 2,
        };
        let lines = stat_lines(&stats);
        let find = |section: &str, label: &str| {
            let start = lines
                .iter()
                .position(|l| l.heading && l.label == section)
                .unwrap();
            lines[start + 1..]
                .iter()
                .take_while(|l| !l.heading)
                .find(|l| l.label == label)
                .cloned()
                .unwrap()
        };
        let rec = find("Recording", "Frames dropped");
        assert_eq!((rec.value.as_str(), rec.bad), ("1 (0.1 %)", true));
        let live = find("Stream", "Connection");
        assert_eq!((live.value.as_str(), live.bad), ("Live", false));
        assert!(!find("Stream", "Frames dropped").bad);
        let late = find("Engine", "Late audio ticks");
        assert_eq!((late.value.as_str(), late.bad), ("2", true));
        let only_stream = Stats {
            stream: Some(OutputStats::default()),
            ..Stats::default()
        };
        let lines = stat_lines(&only_stream);
        assert!(!lines.iter().any(|l| l.label == "Recording"));
        let c = lines.iter().find(|l| l.label == "Connection").unwrap();
        assert_eq!((c.value.as_str(), c.bad), ("Not connected", true));
    }

    fn ui(name: &str) -> (StudioUi, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("rbs-app-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (user, col) = rbs_app::user_session::open(&d, std::sync::Arc::new(|_| {}));
        let canvas = rbs_app::user_session::first_canvas(&col);
        let mut ui = StudioUi::new(&d, &MemoryKeyStore::new(), user, canvas);
        // As after `Update::Started`: Go live is possible.
        ui.cannot_record = None;
        (ui, d)
    }

    #[test]
    fn a_bitrate_below_the_output_recommendation_is_warned() {
        let (mut ui, _d) = ui("bitrate-warn");
        ui.canvas.width = 2560;
        ui.canvas.height = 1440;
        ui.canvas.fps = 30;
        let stream = ui.encoder_data(output::STREAM);
        assert_eq!(
            (
                stream.video_kbps,
                stream.recommended_kbps,
                stream.out_width,
                stream.out_height,
                stream.out_fps
            ),
            (6_000, 9_000, 2560, 1440, 30)
        );
        // The warning follows the encoded picture, not the canvas.
        ui.stream.output_size = Some(OutputSize {
            width: 1920,
            height: 1080,
        });
        let down = ui.encoder_data(output::STREAM);
        assert_eq!(down.recommended_kbps, 0);
        assert_eq!((down.out_width, down.out_height), (1920, 1080));

        ui.stream.output_size = None;
        ui.canvas.width = 3840;
        ui.canvas.height = 2160;
        assert_eq!(ui.encoder_data(output::STREAM).recommended_kbps, 20_000);

        let mut encoding = ui.profile.encoding();
        encoding.rate_control = RateMode::ConstantQuality;
        ui.profile.set_encoding(encoding);
        assert_eq!(ui.encoder_data(output::RECORDING).recommended_kbps, 0);
    }

    #[test]
    fn the_main_display_row_stays_in_pixels_and_shows_the_scale() {
        let (mut ui, d) = ui("display-scale");
        ui.display = Some(MainDisplay {
            size: (3840, 2160),
            scale: rbs_capture::DisplayScale::from_pixels_per_point(2, 1),
        });
        let tiles = ui.canvas_tiles();
        let four_k = tiles.iter().find(|t| t.title == "4K").unwrap();
        assert_eq!((four_k.width, four_k.height), (3840, 2160));
        assert_eq!(four_k.detail, "3840 × 2160 · this display, 200%");
        assert!(tiles.iter().all(|t| t.title != "This display"));
        assert_eq!(
            tiles.iter().find(|t| t.title == "1080p").unwrap().detail,
            "1920 × 1080"
        );
        assert!(tiles.iter().all(|t| (t.width, t.height) != (7680, 4320)));

        ui.display = Some(MainDisplay {
            size: (3440, 1440),
            scale: rbs_capture::DisplayScale::from_dpi(144),
        });
        let tiles = ui.canvas_tiles();
        let row = tiles.iter().find(|t| t.title == "This display").unwrap();
        assert_eq!((row.width, row.height), (3440, 1440));
        assert_eq!(row.detail, "3440 × 1440 · 150%");

        ui.display = Some(MainDisplay {
            size: (2560, 1440),
            scale: None,
        });
        let tiles = ui.canvas_tiles();
        assert!(tiles.iter().all(|t| t.title != "This display"));
        assert_eq!(
            tiles.iter().find(|t| t.title == "1440p").unwrap().detail,
            "2560 × 1440 · this display"
        );

        ui.canvas.width = 1080;
        ui.canvas.height = 1920;
        ui.display = Some(MainDisplay {
            size: (3840, 2160),
            scale: rbs_capture::DisplayScale::from_pixels_per_point(2, 1),
        });
        let tiles = ui.canvas_tiles();
        let portrait = tiles.iter().find(|t| t.title == "1080p").unwrap();
        assert_eq!((portrait.width, portrait.height), (1080, 1920));
        let display = tiles.iter().find(|t| t.title == "This display").unwrap();
        assert_eq!((display.width, display.height), (3840, 2160));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_typed_long_side_fits_the_canvas_and_an_odd_one_is_refused() {
        let (mut ui, d) = ui("long-side");
        ui.canvas.width = 1920;
        ui.canvas.height = 1080;
        ui.set_output_long_side(output::RECORDING, 1600.0);
        assert_eq!(
            ui.profile.output_size.map(|s| (s.width, s.height)),
            Some((1600, 900))
        );
        assert!(ui.profile_note.is_empty(), "{}", ui.profile_note);
        ui.set_output_long_side(output::RECORDING, 1920.0);
        assert_eq!(ui.profile.output_size, None);
        ui.set_output_long_side(output::RECORDING, 1921.0);
        assert!(ui.profile_note.contains("even"), "{}", ui.profile_note);
        assert_eq!(ui.profile.output_size, None);
        ui.canvas.width = 2560;
        ui.canvas.height = 1440;
        ui.profile.output_size = None;
        ui.set_output_same(output::RECORDING, false);
        assert_eq!(
            ui.profile.output_size.map(|s| (s.width, s.height)),
            Some((1920, 1080))
        );
        ui.set_output_same(output::RECORDING, true);
        assert_eq!(ui.profile.output_size, None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_refused_record_keeps_showing_the_recording_that_started_with_the_stream() {
        let (mut ui, d) = ui("auto-record");
        let path = d.join("auto.mkv");
        // Record was pressed while the stream's own recording was on its way.
        ui.rec_state = rec_state::STARTING;
        ui.apply(Update::Recording(RecordingUpdate::Started {
            path: path.clone(),
            video_encoder: "x".into(),
            audio_encoder: None,
        }));
        ui.apply(Update::Recording(RecordingUpdate::NotStarted(
            "A recording is already running.".into(),
        )));
        assert_eq!(ui.rec_state, rec_state::RECORDING);
        assert_eq!(ui.rec_file.as_ref().map(|(p, _)| p), Some(&path));
        assert!(!ui.rec_note_bad);
        // A refusal while Record waits for its own answer is shown.
        ui.rec_state = rec_state::OFF;
        ui.rec_file = None;
        ui.rec_state = rec_state::STARTING;
        ui.apply(Update::Recording(RecordingUpdate::NotStarted("no".into())));
        assert_eq!(ui.rec_state, rec_state::OFF);
        assert!(ui.rec_note_bad);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stream_settings_from_the_control_api_keep_the_server_and_the_key() {
        let (mut ui, d) = ui("api-stream");
        ui.set_server("rtmp://127.0.0.1/live");
        ui.stream.key_saved = true;
        ui.server_edited("rtmp://127.0.0.1/typing");
        let mut p = StreamProfile::new();
        p.server = "rtmp://elsewhere.example/app".into();
        p.reconnect = false;
        p.encoding.video_kbps = 9_000;
        ui.apply(Update::StreamProfile(p));
        assert_eq!(ui.stream.server, "rtmp://127.0.0.1/live");
        assert!(ui.stream.key_saved);
        assert!(!ui.stream.reconnect);
        assert_eq!(ui.stream.encoding.video_kbps, 9_000);
        let saved: StreamProfile = ui.user.section(STREAM).unwrap().unwrap();
        assert_eq!(saved, ui.stream);
        // The server being typed is still there to confirm.
        ui.settings_closed();
        assert_eq!(ui.stream.server, "rtmp://127.0.0.1/typing");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_control_api_profile_switch_is_answered_once_the_profile_is_loaded() {
        let (mut ui, d) = ui("api-switch");
        let id = ui.user.new_profile("Live", false).unwrap();
        let out = ui.apply(Update::SwitchRequested {
            seq: 7,
            collection: false,
            id: id.as_str().to_owned(),
        });
        assert_eq!(out.switch, Some((false, id.as_str().to_owned())));
        assert_eq!(ui.api_switch, Some(7));
        // Refused while live: still answered, and the one in use stays.
        ui.st_state = st_state::LIVE;
        assert!(ui.switch_profile(id.as_str()).is_err());
        ui.refresh_library();
        assert_eq!(ui.api_switch, None);
        assert_ne!(ui.user.profile_id, id);
        ui.st_state = st_state::OFF;
        ui.apply(Update::SwitchRequested {
            seq: 8,
            collection: false,
            id: id.as_str().to_owned(),
        });
        ui.switch_profile(id.as_str()).unwrap();
        assert_eq!(ui.api_switch, None);
        assert_eq!(ui.user.profile_id, id);
        assert!(
            ui.library_choices(false)
                .iter()
                .any(|(name, _, current)| name == "Live" && *current)
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_server_typed_but_not_confirmed_is_kept() {
        let (mut ui, d) = ui("server-draft");
        ui.server_edited("rtmp://127.0.0.1/li");
        ui.server_edited("rtmp://127.0.0.1/live");
        ui.settings_closed();
        assert_eq!(ui.stream.server, "rtmp://127.0.0.1/live");
        assert_eq!(
            ui.user
                .section::<StreamProfile>(STREAM)
                .unwrap()
                .unwrap()
                .server,
            "rtmp://127.0.0.1/live"
        );

        // Go live confirms it too; a bad one is refused and the one in use stays.
        ui.server_edited("http://example.com");
        ui.go_live();
        assert_eq!(ui.stream.server, "rtmp://127.0.0.1/live");
        // Confirmed with Return: closing Settings later changes nothing.
        ui.server_edited("srt://127.0.0.1:9000");
        ui.set_server("srt://127.0.0.1:9000");
        ui.settings_closed();
        assert_eq!(ui.stream.server, "srt://127.0.0.1:9000");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_note_asking_for_a_setting_goes_once_it_is_set() {
        let (mut ui, d) = ui("setup-note");
        ui.go_live();
        assert_eq!(ui.st_state, st_state::OFF);
        assert!(ui.stream_text().contains("Enter the server"));
        ui.set_server("rtmp://127.0.0.1/live");
        assert!(
            ui.stream_text().contains("Enter the stream key"),
            "still missing: the key"
        );
        ui.stream.key_saved = true;
        assert!(ui.setup_note_stale());
        assert_eq!(ui.stream_text(), "");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// ADR-0112 decision 12: the main window's frame row fits the 1280 x 720 minimum
    /// (U11) in every density and built-in theme: with the default toolbar and the fixed
    /// Go live and Record buttons, the row's minimum width is at most 1280 px. The
    /// docked areas' minimums are fixed sizes (`rbs_ui::frame`),
    /// which density does not change.
    #[test]
    fn the_frame_row_fits_1280_by_720_in_every_density() {
        use crate::look::{apply_tokens, slot};
        use crate::menu_bar::{Shown, toolbar};
        use crate::ui::{MainWindow, Shell, Theme, Tokens, ToolbarData};
        use rbs_ui::tokens::{Base, Density, Tokens as TokenFile, apply_density};
        use slint::ComponentHandle;

        let (ui, d) = ui("frame-fits");
        let ws = rbs_ui::builtin::edit();
        let shown = Shown {
            workspace: &ws,
            workspaces: &[],
            studio: &ui,
            panels: Vec::new(),
            themes: Vec::new(),
            maximized: None,
            camera: Default::default(),
            replay: Default::default(),
            can_undo: true,
            can_redo: false,
        };
        let (row, _) = toolbar(&shown);
        let data = |items: &[crate::menu_bar::ToolItem]| -> Vec<ToolbarData> {
            items
                .iter()
                .map(|t| ToolbarData {
                    id: t.id.as_str().into(),
                    title: t.title.as_str().into(),
                    kind: t.kind as i32,
                    enabled: t.enabled,
                    checked: t.checked,
                    live: t.live,
                    icon: t.icon.as_str().into(),
                    icon_only: t.icon_only,
                })
                .collect()
        };
        let model = |v: Vec<ToolbarData>| slint::ModelRc::new(slint::VecModel::from(v));
        i_slint_backend_testing::init_no_event_loop();
        let file = TokenFile::built_in().unwrap();
        for density in [Density::Regular, Density::Compact, Density::Roomy] {
            for base in [Base::Dark, Base::Light] {
                let mut theme = file.built_in_theme(base);
                apply_density(&mut theme.length, density, 3);
                let w = MainWindow::new().unwrap();
                let s = slot(&theme, "").unwrap();
                apply_tokens(
                    &w.global::<Theme>(),
                    &w.global::<Tokens>(),
                    &file,
                    [s.clone(), s],
                )
                .unwrap();
                let shell = w.global::<Shell>();
                shell.set_toolbar(model(data(&row.lead)));
                shell.set_toolbar_end(model(data(&row.end)));
                shell.set_toolbar_fixed(model(data(&row.fixed)));
                shell.set_toolbar_visible(true);
                w.window().set_size(slint::LogicalSize::new(1280.0, 720.0));
                w.show().unwrap();
                let narrowest = w.get_frame_row_width();
                assert!(
                    narrowest <= 1280.0,
                    "{density:?} {base:?}: the frame row needs {narrowest} px, more than 1280"
                );
                w.hide().unwrap();
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_panel_commands_follow_the_stack_rules_and_run_layout_commands() {
        use crate::menu_bar::{Run, Shown, run};
        use rbs_ui::LayoutCommand as L;
        use rbs_ui::command::{CommandId, CommandState};
        use rbs_ui::panel::PanelId;
        let (ui, d) = ui("panel-commands");
        let ws = rbs_ui::builtin::edit();
        let shown = Shown {
            workspace: &ws,
            workspaces: &[],
            studio: &ui,
            panels: Vec::new(),
            themes: Vec::new(),
            maximized: None,
            camera: Default::default(),
            replay: Default::default(),
            can_undo: true,
            can_redo: false,
        };
        let mixer = PanelId::parse("rbs.mixer").unwrap();
        let cmd = |t: &str| CommandId::parse(t).unwrap();
        // The Mixer collapses, floats and closes; a closed panel gives the reason.
        assert!(
            shown
                .enabled(&cmd("ui.panel.collapse?id=rbs.mixer"))
                .is_ok()
        );
        assert!(
            shown
                .enabled(&cmd("ui.panel.collapse?id=rbs.nothing"))
                .is_err()
        );
        assert_eq!(
            run("ui.panel.collapse?id=rbs.mixer", &shown),
            Some(Run::Layout(L::SetCollapsed {
                panel: mixer.clone(),
                collapsed: true,
            }))
        );
        assert_eq!(
            run("ui.panel.float?id=rbs.mixer", &shown),
            Some(Run::Layout(L::Float(mixer.clone())))
        );
        assert_eq!(
            run("ui.panel.close?id=rbs.mixer", &shown),
            Some(Run::Layout(L::ClosePanel(mixer.clone())))
        );
        assert_eq!(
            run("ui.panel.move?id=rbs.mixer&to=left", &shown),
            Some(Run::MovePanel(mixer.clone(), "left".to_owned()))
        );
        assert_eq!(
            run("ui.panel.auto_hide?id=rbs.mixer", &shown),
            Some(Run::AutoHide(mixer))
        );
        assert_eq!(
            shown.title(&cmd("ui.panel.close?id=rbs.mixer")).as_deref(),
            Some("Close Mixer")
        );
        // The move choices name the edges; next tab stack only with a second stack.
        let choices = shown.choices(&cmd("ui.panel.move?id=rbs.mixer"));
        assert!(choices.iter().any(|(_, v, _)| v == "bottom"));
        // A choice of the panel's menu carries the panel, so it runs on it.
        let menu = crate::context_menu::items(
            rbs_ui::command::Context::Panel,
            &[("id".to_owned(), "rbs.mixer".to_owned())],
            &shown,
        );
        let left = menu
            .iter()
            .flat_map(|i| i.choices.iter())
            .find(|c| c.0 == "Left edge")
            .unwrap();
        assert!(
            matches!(run(&left.1, &shown), Some(Run::MovePanel(..))),
            "{}",
            left.1
        );
        // A locked layout refuses what rearranges; collapse and auto-hide stay.
        let mut locked = rbs_ui::builtin::edit();
        locked.locked = true;
        let shown = Shown {
            workspace: &locked,
            ..shown
        };
        for id in [
            "move?id=rbs.mixer&to=left",
            "float?id=rbs.mixer",
            "close?id=rbs.mixer",
        ] {
            assert!(
                shown.enabled(&cmd(&format!("ui.panel.{id}"))).is_err(),
                "{id}"
            );
        }
        assert!(
            shown
                .enabled(&cmd("ui.panel.collapse?id=rbs.mixer"))
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_vertical_canvas_running_at_start_opens_no_panel_but_a_later_add_does() {
        let (mut ui, d) = ui("vertical-at-start");
        let vertical = rbs_app::studio_host::VerticalState {
            canvas: rbs_scene::model::CanvasId::from_raw(2),
            size: rbs_core::media::video::Size::new(1080, 1920),
            follow: true,
        };
        // The saved canvas runs again: the panel keeps the place the workspace saved.
        let out = ui.apply(Update::Vertical(Some(vertical)));
        assert!(!out.vertical_added);
        assert_eq!(ui.vertical, Some(vertical));
        // Removed, then added again by the user: its panel opens.
        assert!(!ui.apply(Update::Vertical(None)).vertical_added);
        assert!(ui.apply(Update::Vertical(Some(vertical))).vertical_added);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn the_vertical_preview_toggle_opens_and_closes_its_panel_while_the_canvas_exists() {
        use crate::menu_bar::{Run, Shown, run};
        use rbs_ui::LayoutCommand as L;
        use rbs_ui::command::{CommandId, CommandState};
        use rbs_ui::panel::PanelId;
        let (mut ui, d) = ui("vertical-preview-toggle");
        ui.state = 1;
        let panel = PanelId::parse(VERTICAL_PANEL).unwrap();
        let toggle = CommandId::parse("canvas.vertical_preview.toggle").unwrap();
        let shown =
            |ui: &StudioUi, ws: &rbs_ui::workspace::Workspace| -> (bool, bool, bool, Option<Run>) {
                let s = Shown {
                    workspace: ws,
                    workspaces: &[],
                    studio: ui,
                    panels: Vec::new(),
                    themes: Vec::new(),
                    maximized: None,
                    camera: Default::default(),
                    replay: Default::default(),
                    can_undo: false,
                    can_redo: false,
                };
                (
                    s.applies(&toggle),
                    s.enabled(&toggle).is_ok(),
                    s.checked(&toggle),
                    run("canvas.vertical_preview.toggle", &s),
                )
            };
        let mut ws = rbs_ui::builtin::edit();
        // No vertical canvas: the toggle is not offered.
        assert_eq!(shown(&ui, &ws), (false, false, false, None));
        // The host's first word is the state at start.
        assert!(!ui.apply(Update::Vertical(None)).vertical_added);
        // Adding one asks for its panel to open.
        let out = ui.apply(Update::Vertical(Some(
            rbs_app::studio_host::VerticalState {
                canvas: rbs_scene::model::CanvasId::from_raw(2),
                size: rbs_core::media::video::Size::new(1080, 1920),
                follow: true,
            },
        )));
        assert!(out.vertical_added);
        assert_eq!(
            shown(&ui, &ws),
            (
                true,
                true,
                false,
                Some(Run::Layout(L::OpenPanel(panel.clone())))
            )
        );
        ws.open(panel.clone()).unwrap();
        assert_eq!(
            shown(&ui, &ws),
            (
                true,
                true,
                true,
                Some(Run::Layout(L::ClosePanel(panel.clone())))
            )
        );
        // Removed while its panel is open: the panel can still be closed.
        let out = ui.apply(Update::Vertical(None));
        assert!(!out.vertical_added);
        assert_eq!(
            shown(&ui, &ws),
            (true, true, true, Some(Run::Layout(L::ClosePanel(panel))))
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn add_source_opens_the_picker_for_a_kind_and_offers_existing_only_when_there_are_some() {
        use crate::menu_bar::{AddWhat, Run, Shown, run};
        use rbs_ui::command::{Context, MenuItem, context_menu};
        let (mut ui, d) = ui("add-source-picker");
        ui.state = 1;
        // The kinds the host reported at start; a kind's code is its place here.
        ui.kind_list = vec![SourceKind::Screen, SourceKind::Camera];
        let ws = rbs_ui::builtin::edit();
        let run_of = |ui: &StudioUi, text: &str| {
            let s = Shown {
                workspace: &ws,
                workspaces: &[],
                studio: ui,
                panels: Vec::new(),
                themes: Vec::new(),
                maximized: None,
                camera: Default::default(),
                replay: Default::default(),
                can_undo: false,
                can_redo: false,
            };
            run(text, &s)
        };
        assert_eq!(
            run_of(&ui, "source.add?kind=screen"),
            Some(Run::AddSource(AddWhat::Kind(0)))
        );
        assert_eq!(run_of(&ui, "source.add?kind=bogus"), None);
        // Nothing runs without a kind: the buttons open the menu.
        assert_eq!(run_of(&ui, "source.add"), None);
        // No other scene has a source: "existing" is not offered and does not run.
        assert_eq!(run_of(&ui, "source.add?kind=existing"), None);
        ui.existing = vec![(SourceId::from_raw(7), "Mic".to_owned())];
        assert_eq!(
            run_of(&ui, "source.add?kind=existing"),
            Some(Run::AddSource(AddWhat::Existing))
        );
        // The type menu lists "Existing source" with the types, and only then.
        let ids = |ui: &StudioUi| -> Vec<String> {
            let s = Shown {
                workspace: &ws,
                workspaces: &[],
                studio: ui,
                panels: Vec::new(),
                themes: Vec::new(),
                maximized: None,
                camera: Default::default(),
                replay: Default::default(),
                can_undo: false,
                can_redo: false,
            };
            context_menu(Context::AddSource, &[], &s)
                .into_iter()
                .filter_map(|i| match i {
                    MenuItem::Command { id, .. } => Some(id.to_string()),
                    _ => None,
                })
                .collect()
        };
        assert!(ids(&ui).contains(&"source.add?kind=existing".to_owned()));
        ui.existing.clear();
        assert!(!ids(&ui).iter().any(|i| i.ends_with("existing")));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn free_collection_name_numbers_when_default_is_taken() {
        let (ui, d) = ui("free-col-name");
        assert_eq!(
            free_collection_name(&ui.user, DEFAULT_NAME),
            "Default 2",
            "a fresh library already has Default"
        );
        ui.user
            .new_collection("Default 2", &ui.canvas, false)
            .unwrap();
        assert_eq!(free_collection_name(&ui.user, DEFAULT_NAME), "Default 3");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn clearing_the_collection_view_drops_scenes_and_sources() {
        let (mut ui, d) = ui("clear-col-view");
        ui.sources.set_vec(vec![SourceData {
            id: 1,
            name: "Cam".into(),
            ..SourceData::default()
        }]);
        ui.scenes.set_vec(vec![SceneData {
            id: 1,
            name: "Scene".into(),
            live: true,
            preview: false,
            number: 1,
        }]);
        ui.selected = 1;
        ui.clear_collection_view();
        assert_eq!(ui.sources.row_count(), 0);
        assert_eq!(ui.scenes.row_count(), 0);
        assert_eq!(ui.selected, -1);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn the_profile_and_collection_commands_run_and_say_why_they_cannot() {
        use crate::menu_bar::{EntryKind, EntryOp, Run, Shown, run};
        use rbs_ui::command::{CommandId, CommandState, Context};
        let (ui, d) = ui("entry-commands");
        let ws = rbs_ui::builtin::edit();
        let s = Shown {
            workspace: &ws,
            workspaces: &[],
            studio: &ui,
            panels: Vec::new(),
            themes: Vec::new(),
            maximized: None,
            camera: Default::default(),
            replay: Default::default(),
            can_undo: false,
            can_redo: false,
        };
        let cmd = |t: &str| CommandId::parse(t).unwrap();
        assert_eq!(
            run("ui.profile.new?name=Live", &s),
            Some(Run::Entry(
                EntryKind::Profile,
                EntryOp::New,
                Some("Live".to_owned())
            ))
        );
        // Without a name the window asks.
        assert_eq!(
            run("ui.collection.rename", &s),
            Some(Run::Entry(EntryKind::Collection, EntryOp::Rename, None))
        );
        assert_eq!(
            run("ui.collection.export", &s),
            Some(Run::Entry(EntryKind::Collection, EntryOp::Export, None))
        );
        // A fresh library holds one output profile: the only one cannot be deleted.
        // The only scene collection can be deleted once the studio runs (a Default
        // replacement takes its place); without a studio the switch is refused.
        let why = s.enabled(&cmd("ui.profile.delete")).unwrap_err();
        assert!(why.starts_with("The only"), "ui.profile.delete: {why}");
        assert_eq!(run("ui.profile.delete", &s), None);
        let why = s.enabled(&cmd("ui.collection.delete")).unwrap_err();
        assert!(
            why.contains("studio"),
            "ui.collection.delete without a studio: {why}"
        );
        assert!(s.enabled(&cmd("ui.profile.rename")).is_ok());
        // The two status-bar menus offer the picker's submenu and the commands.
        for c in [Context::Profiles, Context::Collections] {
            let items = crate::context_menu::items(c, &[], &s);
            assert!(items.iter().any(|i| i.title.starts_with("New ")), "{c:?}");
            assert!(
                items.iter().any(|i| i.title.starts_with("Delete ")),
                "{c:?}"
            );
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_vertical_output_commands_follow_the_vertical_canvas_and_its_outputs() {
        use crate::menu_bar::{Run, Shown, run};
        use rbs_ui::command::{CommandId, CommandState, Context};
        let (mut ui, d) = ui("vertical-outputs");
        ui.state = 1;
        let ws = rbs_ui::builtin::edit();
        let cmd = |t: &str| CommandId::parse(t).unwrap();
        let titles = |ui: &StudioUi| {
            let s = Shown {
                workspace: &ws,
                workspaces: &[],
                studio: ui,
                panels: Vec::new(),
                themes: Vec::new(),
                maximized: None,
                camera: Default::default(),
                replay: Default::default(),
                can_undo: false,
                can_redo: false,
            };
            crate::context_menu::items(Context::VerticalOutputs, &[], &s)
                .into_iter()
                .map(|i| (i.title, i.enabled, i.checked))
                .collect::<Vec<_>>()
        };
        // No vertical canvas: the ▾ menu has nothing.
        assert!(titles(&ui).is_empty());
        ui.vertical = Some(rbs_app::studio_host::VerticalState {
            canvas: rbs_scene::model::CanvasId::from_raw(2),
            size: rbs_core::media::video::Size::new(1080, 1920),
            follow: true,
        });
        assert_eq!(
            titles(&ui),
            [
                ("Go live vertical".to_owned(), true, false),
                ("Record vertical".to_owned(), true, false),
            ]
        );
        // Without a vertical server Go live vertical says where to set it up.
        let why = ui.go_live_vertical().unwrap_err();
        assert_eq!(why, format!("Vertical canvas: {VERTICAL_SETUP}"));
        assert_eq!(ui.vertical_st, VerticalOut::Off);
        // Running outputs read as what the item does now, and stop or ask first.
        ui.vertical_st = VerticalOut::On;
        ui.apply(Update::VerticalRecording(RecordingUpdate::Started {
            path: d.join("v.mkv"),
            video_encoder: "x".into(),
            audio_encoder: None,
        }));
        assert_eq!(ui.vertical_rec, VerticalOut::On);
        assert_eq!(
            titles(&ui),
            [
                ("End vertical stream".to_owned(), true, true),
                ("Stop vertical recording".to_owned(), true, true),
            ]
        );
        {
            let s = Shown {
                workspace: &ws,
                workspaces: &[],
                studio: &ui,
                panels: Vec::new(),
                themes: Vec::new(),
                maximized: None,
                camera: Default::default(),
                replay: Default::default(),
                can_undo: false,
                can_redo: false,
            };
            assert_eq!(
                run("stream.vertical.toggle", &s),
                Some(Run::AskEndVerticalStream)
            );
            assert_eq!(
                run("record.vertical.toggle", &s),
                Some(Run::StopVerticalRecording)
            );
            // The main outputs are not held back by the vertical ones.
            assert!(s.enabled(&cmd("stream.toggle")).is_ok());
            assert!(s.enabled(&cmd("record.toggle")).is_ok());
        }
        // A refused start of another recording leaves the running one running.
        ui.apply(Update::VerticalRecording(RecordingUpdate::NotStarted(
            "busy".into(),
        )));
        assert_eq!(ui.vertical_rec, VerticalOut::On);
        // Stopping: the item waits for the host.
        ui.stop_vertical_recording();
        assert_eq!(ui.vertical_rec, VerticalOut::Ending);
        assert!(!titles(&ui)[1].1);
        ui.apply(Update::VerticalRecording(RecordingUpdate::Stopped {
            path: d.join("v.mkv"),
            failure: None,
            frames: 1,
            dropped: 0,
            flush_lost: 0,
        }));
        ui.apply(Update::VerticalStream(StreamUpdate::Ended {
            failure: None,
            frames: 1,
            dropped: 0,
            connects: 1,
        }));
        assert!(!ui.vertical_rec.busy() && !ui.vertical_st.busy());
        // A start the host refuses ends the wait.
        ui.vertical_st = VerticalOut::Starting;
        let out = ui.apply(Update::VerticalStream(StreamUpdate::NoKey));
        assert_eq!(ui.vertical_st, VerticalOut::Off);
        assert!(out.error.unwrap().starts_with("Vertical canvas: "));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_workspace_commands_run_and_say_why_they_cannot() {
        use crate::menu_bar::{Run, Shown, run};
        use rbs_ui::LayoutCommand as L;
        use rbs_ui::command::{CommandId, CommandState};
        let (ui, d) = ui("workspace-commands");
        let mut ws = rbs_ui::builtin::edit();
        fn shown<'a>(
            ws: &'a rbs_ui::workspace::Workspace,
            studio: &'a StudioUi,
            can_undo: bool,
            can_redo: bool,
        ) -> Shown<'a> {
            Shown {
                workspace: ws,
                workspaces: &[],
                studio,
                panels: Vec::new(),
                themes: Vec::new(),
                maximized: None,
                camera: Default::default(),
                replay: Default::default(),
                can_undo,
                can_redo,
            }
        }
        let cmd = |t: &str| CommandId::parse(t).unwrap();
        let s = shown(&ws, &ui, true, false);
        assert_eq!(run("ui.workspace.save", &s), Some(Run::SaveWorkspace));
        assert_eq!(
            run("ui.workspace.save_as?name=Show", &s),
            Some(Run::SaveWorkspaceAs(Some("Show".to_owned())))
        );
        assert_eq!(
            run("ui.workspace.save_as", &s),
            Some(Run::SaveWorkspaceAs(None))
        );
        assert_eq!(run("ui.layout.undo", &s), Some(Run::LayoutHistory(true)));
        assert!(s.enabled(&cmd("ui.layout.redo")).is_err());
        assert_eq!(run("ui.layout.redo", &s), None);
        // Lock toggles with the workspace's state and the title follows it.
        assert_eq!(
            run("ui.layout.lock", &s),
            Some(Run::Layout(L::SetLocked(true)))
        );
        assert_eq!(
            s.title(&cmd("ui.layout.lock")).as_deref(),
            Some("Lock layout")
        );
        ws.locked = true;
        let s = shown(&ws, &ui, false, true);
        assert!(s.checked(&cmd("ui.layout.lock")));
        assert_eq!(
            run("ui.layout.lock", &s),
            Some(Run::Layout(L::SetLocked(false)))
        );
        assert_eq!(
            s.title(&cmd("ui.layout.lock")).as_deref(),
            Some("Unlock layout")
        );
        assert!(s.enabled(&cmd("ui.layout.undo")).is_err());
        let _ = std::fs::remove_dir_all(d);
    }
}
