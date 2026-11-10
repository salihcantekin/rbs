//! The window's side of the studio (ADR-0041 decision 7): state shown in the `Studio`
//! global, commands to the control thread, the meters' 30 Hz refresh while the Mixer is
//! visible, the Program panel's preview layer, recording and streaming with their
//! output settings.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_app::canvas_profile::{self, CanvasProfile, RATES, SIZES};
use rbs_app::edit_history::{EditKind, StepInfo};
use rbs_app::encoder_form::{Choices, EncoderForm};
use rbs_app::network_access::{self, Source};
use rbs_app::record_profile::{AUDIO_KBPS, Encoding, Format, RateMode, RecordProfile, VIDEO_KBPS};
use rbs_app::remote_control::{NetworkOptions, RemoteOptions, RemoteServer};
use rbs_app::stream_key::{self, KeyStore};
use rbs_app::stream_profile::{self, SRT_DEFAULT_LATENCY_MS, SRT_LATENCIES_MS, StreamProfile};
use rbs_app::studio_host::{
    Command, CropRegion, EncoderInfo, EncoderOptions, RecordRequest, RecordingUpdate, SceneList,
    SourceKind, SourceRow, SourceSettingsView, SourceStatus, Stats, StreamRequest, StreamUpdate,
    StudioHost, Target, TransformOp, Update,
};
use rbs_app::user_session::{RECORDING, STREAM, UserSession};
use rbs_audio::LevelMeter;
use rbs_core::fault::SourceFault;
use rbs_record::Link;
use rbs_remote_settings::{self as remote_profile, Access, RemoteProfile};
use rbs_scene::model::{Border, ItemStyle, SceneId, Shape, SourceId};
use rbs_transport::Secret;
use rbs_ui::fader;
use rbs_ui::meter::{self, Meter, SCALE_MARKS_DB};
use rbs_ui::transform::{DragKeys, MAX_MARKS, Mark, MarkKind, SourceBox};
use rbs_user_data::EntryId;
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::preview_edit::{Effect, Placed, PreviewEdit};
use crate::ui::{
    EncoderData, EntryRow, ExistingData, KindData, LookData, MarkData, MeterRow, ModeData,
    SampleData, ScaleMark, SceneData, SourceData, SourceSettingsData, SourceTargetData, StackData,
    StatRow, Studio, TargetData, TokenRow,
};

/// Meter refresh while the Mixer is visible (U16: 30 Hz).
pub const METER_PERIOD: Duration = Duration::from_millis(33);
/// Accessible meter values change at most this often (U16).
const REPORT_EVERY: u32 = 30;

/// Codes of `Studio.rec-state`.
mod filters;

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
}

/// Codes of `Studio.preview-state`.
pub mod preview_state {
    pub const SHOWN: i32 = 0;
    pub const STARTING: i32 = 1;
    pub const NO_PLATFORM: i32 = 2;
    pub const NO_SESSION: i32 = 3;
    pub const OTHER_WINDOW: i32 = 4;
    pub const STOPPED: i32 = 5;
}

struct MeterSlot {
    source: SourceId,
    level: Arc<LevelMeter>,
    ballistics: Meter,
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
    existing: Rc<VecModel<ExistingData>>,
    /// The Library's looks (set once) and sample pictures (made when the panel first
    /// shows, ADR-0075).
    library_looks: Rc<VecModel<LookData>>,
    library_samples: Rc<VecModel<SampleData>>,
    /// The selected source's id (-1 none), shown in every window's Properties.
    pub selected: i32,
    /// The Filters tab's selection and the drag in progress (ADR-0062).
    filter_ui: filters::FilterUi,
    /// An Add source is on its way: the next new source is selected.
    adding: bool,
    /// Moving and resizing on the picture panel (ADR-0048).
    edit: PreviewEdit,
    /// The selection as marks, panel coordinates; and as the toolkit draws them.
    mark_buf: Vec<Mark>,
    marks: Rc<VecModel<MarkData>>,
    kind_list: Vec<SourceKind>,
    kinds: Rc<VecModel<KindData>>,
    target_list: Vec<Target>,
    targets: Rc<VecModel<TargetData>>,
    /// The Source tab's list (ADR-0077): what the selected source's kind can capture,
    /// filtered by the search field, the source's current target first.
    source_targets: Rc<VecModel<SourceTargetData>>,
    source_target_list: Vec<Target>,
    /// The Source settings window's content (ADR-0077): empty until the host answers,
    /// then one entry; and what its choices mean.
    source_settings: Rc<VecModel<SourceSettingsData>>,
    source_settings_view: Option<SettingsChoices>,
    /// The source the list is for, and what the search field holds.
    source_targets_for: Option<SourceId>,
    source_query: String,
    pub targets_kind: i32,
    slots: Vec<MeterSlot>,
    /// The master strip (ADR-0081): the mixer's master meter and its ballistics, and
    /// the model of its one row (empty while the studio has no mixer).
    master_slot: Option<(Arc<LevelMeter>, Meter)>,
    master_meter: Rc<VecModel<MeterRow>>,
    master_audio: rbs_scene::model::MasterAudio,
    /// What the Mixer shows or hides, for its menus (ADR-0080): the heard sources.
    heard: Vec<HeardFact>,
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
    /// The preview scene's picture layer exists (studio mode).
    pub scene_preview_ready: bool,
    /// The preview scene's picture layer could not be made in this session.
    pub scene_preview_failed: bool,
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
    /// The main display's size, when known.
    display: Option<(u32, u32)>,
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
            existing: Rc::new(VecModel::default()),
            library_looks: Rc::new(VecModel::from(library_looks())),
            library_samples: Rc::new(VecModel::default()),
            filter_ui: filters::FilterUi::default(),
            selected: -1,
            adding: false,
            edit: PreviewEdit::default(),
            mark_buf: Vec::with_capacity(MAX_MARKS),
            marks: Rc::new(VecModel::default()),
            kind_list: Vec::new(),
            kinds: Rc::new(VecModel::default()),
            target_list: Vec::new(),
            targets: Rc::new(VecModel::default()),
            source_targets: Rc::new(VecModel::default()),
            source_settings: Rc::new(VecModel::default()),
            source_settings_view: None,
            source_target_list: Vec::new(),
            source_targets_for: None,
            source_query: String::new(),
            targets_kind: -1,
            slots: Vec::new(),
            master_slot: None,
            master_meter: Rc::new(VecModel::default()),
            master_audio: rbs_scene::model::MasterAudio::default(),
            heard: Vec::new(),
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
            scene_preview_ready: false,
            scene_preview_failed: false,
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
        g.set_existing(ModelRc::from(self.existing.clone()));
        g.set_library_looks(ModelRc::from(self.library_looks.clone()));
        g.set_library_samples(ModelRc::from(self.library_samples.clone()));
        g.set_filter_types(strings(Self::filter_type_names()));
        g.set_audio_filter_types(strings(Self::audio_filter_type_names()));
        g.set_marks(ModelRc::from(self.marks.clone()));
        g.set_kinds(ModelRc::from(self.kinds.clone()));
        g.set_targets(ModelRc::from(self.targets.clone()));
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

    /// Pushes the scalar state into a window's `Studio` global.
    pub fn push(
        &self,
        g: &Studio<'_>,
        preview_state: i32,
        scene_preview_state: i32,
        picture_panel: &str,
    ) {
        g.set_studio_mode(self.studio_mode);
        g.set_scene_preview_state(scene_preview_state);
        g.set_selected_source(self.selected);
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
        let p = &self.profile;
        g.set_out_format(Format::ALL.iter().position(|f| *f == p.format).unwrap_or(0) as i32);
        g.set_out_formats(strings(Format::ALL.iter().map(|f| f.label().to_owned())));
        g.set_out_enc(self.encoder_data(output::RECORDING));
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
        if g.get_out_folder_serial() != self.folder_serial as i32 {
            g.set_out_folder(self.profile.folder.display().to_string().into());
            g.set_out_folder_serial(self.folder_serial as i32);
        }
        let sizes = self.canvas_sizes();
        g.set_canvas_sizes(strings(sizes.iter().map(|(_, label)| label.clone())));
        g.set_canvas_size(
            sizes
                .iter()
                .position(|(s, _)| *s == (self.canvas.width, self.canvas.height))
                .unwrap_or(0) as i32,
        );
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
        g.set_picture_panel(picture_panel.into());
        g.set_state(self.state);
        g.set_failure(self.failure.as_str().into());
        g.set_targets_kind(self.targets_kind);
        g.set_preview_state(preview_state);
        g.set_deleted(self.deleted.as_str().into());
        g.set_deleted_from_scene(self.deleted_from_scene);
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
    /// `SourceKind::ALL` (the Add source menu's).
    pub fn kind_codes(&self) -> Vec<(i32, &'static str)> {
        self.kind_list
            .iter()
            .filter_map(|k| {
                let code = SourceKind::ALL.iter().position(|a| a == k)?;
                Some((code as i32, k.label()))
            })
            .collect()
    }

    pub fn list_targets(&mut self, code: i32) {
        let Some(kind) = SourceKind::ALL.get(code as usize).copied() else {
            return;
        };
        self.targets_kind = -1;
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
        let rows = source_target_rows(&self.source_target_list, &current, &self.source_query)
            .into_iter()
            .map(|r| SourceTargetData {
                name: self.source_target_list[r.index].name.as_str().into(),
                current: r.current,
                index: r.index as i32,
                heading: r.heading.into(),
            })
            .collect::<Vec<_>>();
        self.source_targets.set_vec(rows);
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

    pub fn add_source(&mut self, code: i32, index: i32) {
        let kind = SourceKind::ALL.get(code as usize).copied();
        let target = self.target_list.get(index as usize).cloned();
        if let (Some(kind), Some(target)) = (kind, target) {
            self.adding = true;
            self.deleted.clear();
            self.send(Command::AddSource { kind, target });
        }
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
        let (code, _) = self.kind_codes().get(self.global_kind as usize).copied()?;
        SourceKind::ALL.get(code as usize).copied()
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

    /// Places a source of another scene into the scene on air (Add source, Existing
    /// source); it is selected once it is there.
    pub fn add_existing_source(&mut self, id: i32) {
        self.deleted.clear();
        self.adding = true;
        self.send(Command::AddExistingSource(SourceId::from_raw(id as u32)));
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
        let data: Vec<ExistingData> = list
            .sources
            .iter()
            .filter(|s| !s.global && !self.rows.iter().any(|r| r.id == s.id))
            .map(|s| ExistingData {
                id: s.id.get() as i32,
                name: s.name.as_str().into(),
                icon: source_icon(s.kind).into(),
            })
            .collect();
        set_rows(&self.existing, data);
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

    pub fn move_source(&mut self, id: i32, up: bool) {
        self.deleted.clear();
        self.send(Command::MoveSource {
            id: SourceId::from_raw(id as u32),
            up,
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

    /// Shape and border of a video source's picture (the Properties fields). `shape`:
    /// 0 rectangle, 1 circle, 2 ellipse. A colour that is not `#rrggbb` or `#rrggbbaa`
    /// changes nothing.
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
            self.stream.encoding.clone()
        } else {
            self.profile.encoding()
        }
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
        EncoderData {
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
            _ => false,
        };
        if !changed {
            return;
        }
        if out == output::STREAM {
            self.stream.encoding = e;
            self.save_stream();
        } else {
            self.profile.set_encoding(e);
            self.save_profile();
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
            self.profile.folder = folder;
            self.save_profile();
        } else {
            self.profile_note = "The folder must be a full path, for example C:\\Videos.".into();
        }
        // The field shows the folder in use (also after a refused entry).
        self.folder_serial += 1;
    }

    pub fn set_format(&mut self, i: i32) {
        if let Some(f) = Format::ALL.get(i as usize) {
            self.profile.format = *f;
            self.save_profile();
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

    /// The server field was confirmed.
    pub fn set_server(&mut self, text: &str) {
        self.server_draft = None;
        let url = text.trim();
        if url == self.stream.server {
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
        self.stream.server = url.to_owned();
        self.server_serial += 1;
        self.save_stream();
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
        if key.is_empty() || self.st_state != st_state::OFF {
            return;
        }
        self.send(Command::SaveStreamKey(Some(Secret::new(key))));
    }

    /// "Remove key".
    pub fn remove_key(&mut self) {
        if self.st_state == st_state::OFF {
            self.send(Command::SaveStreamKey(None));
        }
    }

    /// "Record when the stream starts".
    pub fn set_auto_record(&mut self, on: bool) {
        self.stream.auto_record = on;
        self.save_stream();
    }

    pub fn set_reconnect(&mut self, on: bool) {
        self.stream.reconnect = on;
        self.save_stream();
    }

    /// The SRT latencies offered: the default, the presets and a saved other one.
    fn latencies(&self) -> Vec<Option<u16>> {
        let mut v: Vec<Option<u16>> = std::iter::once(None)
            .chain(SRT_LATENCIES_MS.iter().map(|ms| Some(*ms)))
            .collect();
        if !v.contains(&self.stream.srt_latency_ms) {
            v.push(self.stream.srt_latency_ms);
        }
        v
    }

    pub fn set_latency(&mut self, i: i32) {
        if let Some(ms) = self.latencies().get(i as usize) {
            self.stream.srt_latency_ms = *ms;
            self.save_stream();
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
        let st = &self.stream;
        g.set_st_state(self.st_state);
        g.set_st_connected(self.st_state == st_state::LIVE && self.st_link.connected);
        g.set_st_text(self.stream_text().into());
        g.set_st_bad(
            self.st_state == st_state::OFF
                && self.rec_state == rec_state::OFF
                && self.st_bad
                && !self.setup_note_stale(),
        );
        g.set_st_needs_setup(st.missing().is_some());
        if g.get_st_server_serial() != self.server_serial as i32 {
            g.set_st_server(st.server.as_str().into());
            g.set_st_server_serial(self.server_serial as i32);
        }
        g.set_st_key_saved(st.key_saved);
        g.set_st_key_place(self.key_hint.as_str().into());
        g.set_st_key_serial(self.key_serial as i32);
        g.set_st_reconnect(st.reconnect);
        g.set_st_auto_record(st.auto_record);
        let record = self.profile.encoding();
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

    /// The canvas sizes offered, with their labels: the presets, the main display's
    /// size, and the saved size if it is none of them.
    fn canvas_sizes(&self) -> Vec<((u32, u32), String)> {
        let label = |(w, h): (u32, u32)| {
            if h > w {
                format!("{w} x {h} (vertical)")
            } else {
                format!("{w} x {h}")
            }
        };
        let mut v: Vec<_> = SIZES.iter().map(|s| (*s, label(*s))).collect();
        let display = self
            .display
            .and_then(|(w, h)| canvas_profile::canvas_for_display(w, h));
        if let Some(d) = display {
            match v.iter_mut().find(|(s, _)| *s == d) {
                Some(entry) => entry.1 = format!("{} (main display)", label(d)),
                None => v.push((d, format!("{} (main display)", label(d)))),
            }
        }
        let saved = (self.canvas.width, self.canvas.height);
        if !v.iter().any(|(s, _)| *s == saved) {
            v.push((saved, label(saved)));
        }
        v
    }

    /// A canvas size was picked. `close_preview` runs before the studio restarts:
    /// the old studio's preview ends with it.
    pub fn set_canvas_size(&mut self, i: i32, close_preview: impl FnOnce()) {
        if let Some(((w, h), _)) = self.canvas_sizes().get(i as usize) {
            let next = CanvasProfile {
                width: *w,
                height: *h,
                ..self.canvas
            };
            self.change_canvas(next, close_preview);
        }
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

    /// Adds a sample picture to the scene being edited (ADR-0075).
    pub fn add_sample(&mut self, sample: &str) {
        self.deleted.clear();
        self.send(Command::AddSample {
            sample: sample.to_owned(),
        });
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
                            locked: false,
                            muted: h.muted,
                            mixer_hidden: h.hidden,
                            picture: false,
                            audio: true,
                            sized: false,
                            flip: rbs_scene::FlipState::default(),
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
        Some(SourceFacts {
            id: row.id.get() as i32,
            item: row.item.get(),
            visible: row.visible,
            locked: row.locked,
            muted: row.audio.muted,
            mixer_hidden: row.audio.mixer_hidden,
            picture: row.layout.is_some(),
            audio: row.kind.is_some_and(SourceKind::is_audio),
            sized: row.picture.is_some(),
            flip: row.layout.map(|l| l.flip_state()).unwrap_or_default(),
        })
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
        // ADR-0051 decision 5: the key goes with the profile.
        self.send(Command::RemoveStreamKey(stream_key::account(id.as_str())));
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
        self.ask_switch(id, name, collection, None, close_preview);
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

    fn ask_switch(
        &mut self,
        id: EntryId,
        name: String,
        collection: rbs_scene::SceneCollection,
        delete_after: Option<EntryId>,
        close_preview: impl FnOnce(),
    ) {
        self.switch_seq += 1;
        let autosave = self.user.saver_for(&id, &name);
        close_preview();
        self.preview_ready = false;
        self.preview_failed = false;
        self.scene_preview_ready = false;
        self.scene_preview_failed = false;
        self.canvas_restarting = true;
        self.switching = Some(Switching {
            seq: self.switch_seq,
            id,
            name,
            delete_after,
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
                s.delete_after
                    .and_then(|old| self.user.delete_collection(&old).err())
                    .map(|e| format!("The scene collection was not deleted: {e}"))
            }
            Err(why) => Some(why),
        };
        self.refresh_library();
        told
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
        self.ask_switch(id, name, collection, None, close_preview);
        Ok(())
    }

    /// Scene collection menu: renames the collection in use.
    pub fn rename_collection(&mut self, name: &str) -> Result<(), String> {
        let result = self.user.rename_collection(name);
        self.refresh_library();
        result
    }

    /// Scene collection menu: deletes the collection `id`. Deleting the one in use
    /// first runs another (refused while live, and for the only one).
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
        let (other, _) = self
            .user
            .collections()
            .into_iter()
            .find(|(other, _)| *other != id)
            .ok_or("The only scene collection cannot be deleted. Make another first.")?;
        self.can_switch_collection()?;
        let (name, collection) = self.user.load_collection(&other)?;
        self.ask_switch(other, name, collection, Some(id), close_preview);
        Ok(())
    }

    /// The recording's duration so far, while one runs.
    pub fn recording_for(&self) -> Option<(Duration, String)> {
        self.rec_file
            .as_ref()
            .map(|(p, since)| (since.elapsed(), file_name(p)))
    }

    pub fn reset_clip(&mut self, i: i32) {
        if let Some(s) = self.slots.get_mut(i as usize) {
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
        match u {
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
                self.refresh_library();
                self.kind_list = kinds;
                let rows: Vec<KindData> = self
                    .kind_list
                    .iter()
                    .map(|k| KindData {
                        code: SourceKind::ALL.iter().position(|a| a == k).unwrap_or(0) as i32,
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
            Update::Sources(rows) => {
                self.set_rows(rows);
                self.refresh_existing();
                self.refresh_marks();
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
            Update::Targets(kind, list) => {
                self.targets_kind = SourceKind::ALL
                    .iter()
                    .position(|a| *a == kind)
                    .map_or(-1, |i| i as i32);
                self.targets.set_vec(
                    list.iter()
                        .map(|t| TargetData {
                            name: t.name.as_str().into(),
                            is_default: t.is_default,
                        })
                        .collect::<Vec<_>>(),
                );
                let picked = self
                    .kind_codes()
                    .get(self.global_kind as usize)
                    .and_then(|(code, _)| SourceKind::ALL.get(*code as usize).copied());
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
                self.target_list = list;
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
            Update::CollectionSwitched { seq, result } => {
                if let Some(e) = self.collection_switched(seq, result) {
                    out.error = Some(e);
                }
            }
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
                    Shape::Rectangle { radius } => (0, radius),
                    _ => (0, 0.0),
                };
                // Turning a border on starts from these.
                let border = style
                    .border
                    .unwrap_or(Border::new(DEFAULT_BORDER_WIDTH, [255, 255, 255, 255]));
                let [cr, cg, cb, ca] = border.color;
                let (filters, selected_filter) = self.filter_ui.data(r);
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
            })
            .collect();
        let mut old: Vec<MeterSlot> = std::mem::take(&mut self.slots);
        for r in rows.iter().filter(|r| !r.audio.mixer_hidden) {
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
            .filter(|r| r.meter.is_some() && !r.audio.mixer_hidden)
            .map(|r| MeterRow {
                id: r.id.get() as i32,
                muted: r.audio.muted,
                volume: fader::position_of(r.audio.volume),
                volume_text: fader::volume_text(r.audio.volume).into(),
                name: r.name.as_str().into(),
                icon: source_icon(r.kind).into(),
                value: "silent".into(),
                global: r.global,
                ..MeterRow::default()
            })
            .collect();
        set_rows(&self.meters, meters);
        // Hidden sources are heard but not metered: names only (ADR-0080).
        let hidden: Vec<MeterRow> = rows
            .iter()
            .filter(|r| r.meter.is_some() && r.audio.mixer_hidden)
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

    /// One meter refresh: takes each source's peaks since the last call.
    pub fn tick_meters(&mut self) {
        let now = self.meter_start.elapsed();
        self.meter_ticks = self.meter_ticks.wrapping_add(1);
        let report = self.meter_ticks.is_multiple_of(REPORT_EVERY);
        for (i, s) in self.slots.iter_mut().enumerate() {
            let levels = s.level.take();
            let db = (0..levels.channels())
                .map(|ch| levels.peak_db(ch))
                .fold(f32::NEG_INFINITY, f32::max);
            s.ballistics.update(db, now);
            let view = s.ballistics.view(now);
            let Some(mut row) = self.meters.row_data(i) else {
                continue;
            };
            paint(&mut row, &view);
            if report {
                row.value = s.ballistics.take_report().english().into();
            }
            self.meters.set_row_data(i, row);
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
#[derive(Default)]
pub struct Applied {
    pub error: Option<String>,
    pub preview: Option<rbs_gpu::share::PreviewEndpoint>,
    /// The preview scene's picture endpoint changed (studio mode, ADR-0061): connect to
    /// it, or close that picture for `None`.
    pub scene_preview: Option<Option<rbs_gpu::share::PreviewEndpoint>>,
    /// Studio mode just started: a closed Preview panel opens (UI spec, Studio mode).
    pub studio_mode_started: bool,
    /// A control API client asked to switch: scene collections (`true`) or output
    /// profiles to the entry with this id. The caller switches as the File menu
    /// does and then calls [`StudioUi::refresh_library`], which answers the host.
    pub switch: Option<(bool, String)>,
    /// The virtual camera started or stopped sending (`Some(None)`: it does not send).
    pub camera: Option<Option<rbs_app::studio_host::CameraStatus>>,
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
    pub locked: bool,
    pub muted: bool,
    /// Kept out of the Mixer (ADR-0080).
    pub mixer_hidden: bool,
    /// It has a picture (it can be locked on the picture panel).
    pub picture: bool,
    /// It makes sound only (it can be muted in the Mixer).
    pub audio: bool,
    /// Its picture size is known, so it can be fitted or reset (ADR-0084).
    pub sized: bool,
    /// Its picture is mirrored left to right or top to bottom as seen on the canvas.
    pub flip: rbs_scene::FlipState,
}

/// One heard source as the Mixer's menus ask about it (ADR-0080).
#[derive(Clone, Copy, Debug)]
struct HeardFact {
    id: u32,
    muted: bool,
    hidden: bool,
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

/// The sample pictures with their thumbnails (made once, when the Library first shows).
fn library_samples() -> Vec<SampleData> {
    rbs_sources::still::SAMPLES
        .iter()
        .filter_map(|s| {
            let px = rbs_sources::still::sample_picture(s.id, THUMB.0, THUMB.1)?;
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &px, THUMB.0, THUMB.1,
            );
            Some(SampleData {
                id: s.id.into(),
                name: s.name.into(),
                frame: s.kind == rbs_sources::still::SampleKind::Frame,
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
        None => "window",
    }
}

/// The Studio global's `undo-kind` / `redo-kind` code and the source's name for a
/// step: 0 nothing, 1 add, 2 delete, 3 layer order, 4 move or resize, 5 shape or
/// border, 6 crop, 7 shown or hidden, 8 lock, 9 source rename, 10 scene added, 11
/// scene removed, 12 scene renamed (the name is the scene's, or the old name), 13
/// filters changed, 14 removed from one scene, 15 made global or moved back into a
/// scene, 16 look applied, 17 source pointed at another target, 18 source settings
/// changed, 19 flipped, turned, fitted or centred.
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
        }
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
}
