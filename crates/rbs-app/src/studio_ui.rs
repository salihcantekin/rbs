//! The window's side of the studio (ADR-0041 decision 7): state shown in the `Studio`
//! global, commands to the control thread, the meters' 30 Hz refresh while the Mixer is
//! visible, the Program panel's preview layer, and recording with its output settings.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_app::canvas_profile::{self, CanvasProfile, RATES, SIZES};
use rbs_app::encoder_form::{Choices, EncoderForm};
use rbs_app::record_profile::{self, AUDIO_KBPS, Format, RateMode, RecordProfile, VIDEO_KBPS};
use rbs_app::remote_control::{RemoteOptions, RemoteServer};
use rbs_app::studio_host::{
    Command, EncoderInfo, EncoderOptions, RecordRequest, RecordingUpdate, SourceKind, SourceRow,
    SourceStatus, StudioHost, Target, Update,
};
use rbs_audio::LevelMeter;
use rbs_remote_settings::{self as remote_profile, Access, RemoteProfile};
use rbs_scene::model::SourceId;
use rbs_ui::meter::{self, Meter, SCALE_MARKS_DB};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::ui::{
    KindData, MeterRow, ScaleMark, SourceData, StackData, Studio, TargetData, TokenRow,
};

/// Meter refresh while the Mixer is visible (U16: 30 Hz).
pub const METER_PERIOD: Duration = Duration::from_millis(33);
/// Accessible meter values change at most this often (U16).
const REPORT_EVERY: u32 = 30;

/// Codes of `Studio.rec-state`.
pub mod rec_state {
    pub const OFF: i32 = 0;
    pub const STARTING: i32 = 1;
    pub const RECORDING: i32 = 2;
    pub const STOPPING: i32 = 3;
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
    kind_list: Vec<SourceKind>,
    kinds: Rc<VecModel<KindData>>,
    target_list: Vec<Target>,
    targets: Rc<VecModel<TargetData>>,
    pub targets_kind: i32,
    slots: Vec<MeterSlot>,
    meters: Rc<VecModel<MeterRow>>,
    scale: Rc<VecModel<ScaleMark>>,
    meter_start: Instant,
    meter_ticks: u32,
    /// The preview endpoint arrived; the platform has a preview.
    pub preview_offered: bool,
    /// The preview layer could not be made in this session.
    pub preview_failed: bool,
    /// The layer exists.
    pub preview_ready: bool,
    pub deleted: String,
    pub deleted_serial: u64,
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
}

/// The remote control settings are locked by another program.
struct Busy;

impl StudioUi {
    pub fn new(config: &Path) -> Self {
        let (profile, why) = record_profile::load(config);
        let (canvas, canvas_why) = canvas_profile::load(config);
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
        let ui = Self {
            host: None,
            state: 0,
            failure: String::new(),
            rows: Vec::new(),
            sources: Rc::new(VecModel::default()),
            kind_list: Vec::new(),
            kinds: Rc::new(VecModel::default()),
            target_list: Vec::new(),
            targets: Rc::new(VecModel::default()),
            targets_kind: -1,
            slots: Vec::new(),
            meters: Rc::new(VecModel::default()),
            scale: Rc::new(VecModel::from(scale)),
            meter_start: Instant::now(),
            meter_ticks: 0,
            preview_offered: false,
            preview_failed: false,
            preview_ready: false,
            deleted: String::new(),
            deleted_serial: 0,
            rec_state: rec_state::OFF,
            rec_file: None,
            rec_note: String::new(),
            rec_note_bad: false,
            cannot_record: Some("The studio is starting.".into()),
            encoders: Vec::new(),
            profile,
            config: config.to_owned(),
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
        };
        ui.refresh_tokens();
        ui
    }

    /// Gives a window's `Studio` global the shared models (once per window).
    pub fn attach_models(&self, g: &Studio<'_>) {
        g.set_sources(ModelRc::from(self.sources.clone()));
        g.set_kinds(ModelRc::from(self.kinds.clone()));
        g.set_targets(ModelRc::from(self.targets.clone()));
        g.set_meters(ModelRc::from(self.meters.clone()));
        g.set_scale(ModelRc::from(self.scale.clone()));
        g.set_rc_tokens(ModelRc::from(self.tokens.clone()));
    }

    /// Pushes the scalar state into a window's `Studio` global.
    pub fn push(&self, g: &Studio<'_>, preview_state: i32, picture_panel: &str) {
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
        let mut encoders = vec!["Automatic (best available)".to_owned()];
        encoders.extend(self.encoders.iter().map(|e| e.name.clone()));
        g.set_out_encoders(strings(encoders));
        g.set_out_encoder(self.encoder_index() as i32);
        let video = self.video_options();
        g.set_out_video_rates(strings(video.iter().map(|k| kbps(*k))));
        g.set_out_video_rate(video.iter().position(|k| *k == p.video_kbps).unwrap_or(0) as i32);
        g.set_out_audio_rates(strings(AUDIO_KBPS.iter().map(|k| kbps(*k))));
        g.set_out_audio_rate(
            AUDIO_KBPS
                .iter()
                .position(|k| *k == p.audio_kbps)
                .unwrap_or(0) as i32,
        );
        let form = self.form();
        g.set_out_rate_kind(match p.rate_control {
            RateMode::Cbr => 0,
            RateMode::Vbr => 1,
            RateMode::ConstantQuality => 2,
        });
        let (o, i) = choices(&form.rate_modes);
        g.set_out_rate_modes(o);
        g.set_out_rate_mode(i);
        g.set_out_bitrate_label(form.bitrate_label.into());
        let (o, i) = choices(&form.peaks);
        g.set_out_peaks(o);
        g.set_out_peak(i);
        let (o, i) = choices(&form.qualities);
        g.set_out_qualities(o);
        g.set_out_quality(i);
        let (o, i) = choices(&form.keyframes);
        g.set_out_keyframes(o);
        g.set_out_keyframe(i);
        let (o, i) = choices(&form.presets);
        g.set_out_presets(o);
        g.set_out_preset(i);
        let (o, i) = choices(&form.profiles);
        g.set_out_profiles(o);
        g.set_out_profile(i);
        let (o, i) = choices(&form.b_frames);
        g.set_out_b_frames(o);
        g.set_out_b_frame(i);
        g.set_out_b_frames_note(form.b_frames_note.unwrap_or_default().into());
        g.set_out_padding(p.cbr_padding);
        g.set_out_padding_available(form.padding_available);
        g.set_out_note(self.profile_note.as_str().into());
        let encoder = match self.encoder_index() {
            0 => "best encoder".to_owned(),
            i => self
                .encoders
                .get(i - 1)
                .map_or_else(String::new, |e| e.name.clone()),
        };
        let video = match p.rate_control {
            RateMode::Cbr if p.cbr_padding => format!("video {} kbit/s padded", p.video_kbps),
            RateMode::Cbr => format!("video {} kbit/s", p.video_kbps),
            RateMode::Vbr => format!("video VBR {} kbit/s, up to {}", p.video_kbps, p.peak_kbps()),
            RateMode::ConstantQuality => format!("video quality {}", p.quality),
        };
        g.set_out_summary(
            format!(
                "{} · {} · {} · audio {} kbit/s · {}",
                p.format.label(),
                encoder,
                video,
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
        g.set_picture_panel(picture_panel.into());
        g.set_state(self.state);
        g.set_failure(self.failure.as_str().into());
        g.set_targets_kind(self.targets_kind);
        g.set_preview_state(preview_state);
        g.set_deleted(self.deleted.as_str().into());
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
                    "Remote control is not running: {why}. Choose another port, or turn remote control off."
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
                "On. Apps on this computer connect at {}. Other computers cannot connect.",
                self.remote.address()
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
        match RemoteServer::start_on_port(self.remote.port, self.remote.store(), options) {
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
        let Ok(_lock) = self.lock_remote() else {
            return;
        };
        self.reload_tokens();
        self.remote.enabled = on;
        self.save_remote();
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
                    // Listen on the new port: connected apps reconnect there.
                    self.stop_remote();
                    self.remote_failure = None;
                    self.sync_remote();
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

    fn send(&self, c: Command) {
        if let Some(h) = &self.host {
            h.send(c);
        }
    }

    pub fn list_targets(&mut self, code: i32) {
        let Some(kind) = SourceKind::ALL.get(code as usize).copied() else {
            return;
        };
        self.targets_kind = -1;
        self.send(Command::ListTargets(kind));
    }

    pub fn add_source(&mut self, code: i32, index: i32) {
        let kind = SourceKind::ALL.get(code as usize).copied();
        let target = self.target_list.get(index as usize).cloned();
        if let (Some(kind), Some(target)) = (kind, target) {
            self.send(Command::AddSource { kind, target });
        }
    }

    pub fn delete_source(&mut self, id: i32) {
        self.deleted.clear();
        self.send(Command::DeleteSource(SourceId::from_raw(id as u32)));
    }

    pub fn undo_delete(&mut self) {
        self.deleted.clear();
        self.send(Command::UndoDelete);
    }

    /// The picked encoder's place in the list (0: automatic, also when the saved one is
    /// not on this system).
    fn encoder_index(&self) -> usize {
        self.profile
            .encoder
            .as_ref()
            .and_then(|id| self.encoders.iter().position(|e| e.id == *id))
            .map_or(0, |i| i + 1)
    }

    /// What the picked encoder can do; with automatic choice, what any encoder can do.
    fn options(&self) -> EncoderOptions {
        match self.encoder_index() {
            0 => EncoderOptions::union(self.encoders.iter().map(|e| &e.options)),
            i => self
                .encoders
                .get(i - 1)
                .map(|e| e.options.clone())
                .unwrap_or_default(),
        }
    }

    /// The encoder fields of the form, for the profile and the picked encoder.
    fn form(&self) -> EncoderForm {
        EncoderForm::new(&self.profile, &self.options())
    }

    /// The video bitrates offered: the presets, and the saved one if it is not one.
    fn video_options(&self) -> Vec<u32> {
        let mut v = VIDEO_KBPS.to_vec();
        if !v.contains(&self.profile.video_kbps) {
            v.push(self.profile.video_kbps);
            v.sort_unstable();
        }
        v
    }

    /// Starts recording with the output settings.
    pub fn record(&mut self) {
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
        let encoder = match self.encoder_index() {
            0 => None,
            i => self.encoders.get(i - 1).map(|e| e.id.clone()),
        };
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

    pub fn set_encoder(&mut self, i: i32) {
        self.profile.encoder = match i {
            0 => None,
            i => match self.encoders.get(i as usize - 1) {
                Some(e) => Some(e.id.clone()),
                None => return,
            },
        };
        self.save_profile();
    }

    pub fn set_video_rate(&mut self, i: i32) {
        if let Some(k) = self.video_options().get(i as usize) {
            self.profile.video_kbps = *k;
            self.save_profile();
        }
    }

    pub fn set_audio_rate(&mut self, i: i32) {
        if let Some(k) = AUDIO_KBPS.get(i as usize) {
            self.profile.audio_kbps = *k;
            self.save_profile();
        }
    }

    pub fn set_rate_mode(&mut self, i: i32) {
        if let Some(m) = self.form().rate_modes.get(i) {
            self.profile.rate_control = m;
            self.save_profile();
        }
    }

    pub fn set_peak(&mut self, i: i32) {
        if let Some(k) = self.form().peaks.get(i) {
            self.profile.max_video_kbps = k;
            self.save_profile();
        }
    }

    pub fn set_quality(&mut self, i: i32) {
        if let Some(q) = self.form().qualities.get(i) {
            self.profile.quality = q;
            self.save_profile();
        }
    }

    pub fn set_padding(&mut self, on: bool) {
        self.profile.cbr_padding = on;
        self.save_profile();
    }

    pub fn set_keyframe(&mut self, i: i32) {
        if let Some(s) = self.form().keyframes.get(i) {
            self.profile.keyframe_seconds = s;
            self.save_profile();
        }
    }

    pub fn set_preset(&mut self, i: i32) {
        if let Some(p) = self.form().presets.get(i) {
            self.profile.preset = p;
            self.save_profile();
        }
    }

    pub fn set_profile(&mut self, i: i32) {
        if let Some(p) = self.form().profiles.get(i) {
            self.profile.profile = p;
            self.save_profile();
        }
    }

    pub fn set_b_frames(&mut self, i: i32) {
        if let Some(n) = self.form().b_frames.get(i) {
            self.profile.b_frames = n;
            self.save_profile();
        }
    }

    fn save_profile(&mut self) {
        self.profile_note = match record_profile::save(&self.config, &self.profile) {
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
        if let Some(why) = next.problem() {
            self.canvas_note = why;
            return;
        }
        self.canvas = next;
        self.save_canvas();
        if self.state != 1 || self.host.is_none() {
            // Not running: the next start uses it.
            return;
        }
        close_preview();
        self.preview_ready = false;
        self.preview_failed = false;
        self.canvas_restarting = true;
        self.send(Command::SetCanvas {
            size: next.size(),
            fps: next.frame_rate(),
        });
    }

    fn save_canvas(&mut self) {
        if let Err(e) = canvas_profile::save(&self.config, &self.canvas) {
            self.canvas_note = format!("The canvas could not be saved: {e}");
        } else {
            self.canvas_note.clear();
        }
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
        match u {
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
                    let note = std::mem::take(&mut self.canvas_note);
                    self.save_canvas();
                    if self.canvas_note.is_empty() {
                        self.canvas_note = note;
                    }
                }
                self.preview_offered = preview.is_some();
                out.preview = preview;
            }
            Update::Failed(why) => {
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
                self.failure = why.clone();
                out.error = Some(format!("The studio stopped: {why}"));
            }
            Update::Sources(rows) => self.set_rows(rows),
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
                self.target_list = list;
            }
            Update::Deleted(name) => {
                self.deleted = name;
                self.deleted_serial += 1;
            }
            Update::Error(e) => out.error = Some(e),
            Update::Recording(r) => self.recording(r, &mut out),
            Update::Profile(p) => {
                // A control API client changed the settings: show and save them.
                self.profile = p;
                self.folder_serial += 1;
                self.save_profile();
            }
            // `rbs-cli token` over local IPC made or revoked a token; the control
            // thread gave it to the server already. Only the list shown changes here.
            Update::TokensChanged => self.read_tokens(false),
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

    fn set_rows(&mut self, rows: Vec<SourceRow>) {
        let data: Vec<SourceData> = rows
            .iter()
            .map(|r| {
                let (status, ok) = match &r.status {
                    SourceStatus::Running => ("Running".to_owned(), true),
                    SourceStatus::Idle => ("Not shown, closed".to_owned(), true),
                    SourceStatus::Failed(why) => (format!("Not running: {why}"), false),
                };
                SourceData {
                    id: r.id.get() as i32,
                    name: r.name.as_str().into(),
                    kind: r.kind.map_or("Unknown type", SourceKind::label).into(),
                    status: status.into(),
                    ok,
                }
            })
            .collect();
        set_rows(&self.sources, data);
        // Meters: one per playing audio source, in list order; ballistics survive for
        // sources that stay.
        let mut old: Vec<MeterSlot> = std::mem::take(&mut self.slots);
        for r in &rows {
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
        let names: Vec<SharedString> = rows
            .iter()
            .filter(|r| r.meter.is_some())
            .map(|r| r.name.as_str().into())
            .collect();
        let meters: Vec<MeterRow> = names
            .into_iter()
            .map(|name| MeterRow {
                name,
                value: "silent".into(),
                ..MeterRow::default()
            })
            .collect();
        set_rows(&self.meters, meters);
        self.rows = rows;
    }

    /// True while meters need refreshing.
    pub fn has_meters(&self) -> bool {
        !self.slots.is_empty()
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
            if report {
                row.value = s.ballistics.take_report().english().into();
            }
            self.meters.set_row_data(i, row);
        }
    }
}

/// What [`StudioUi::apply`] asks of the shell.
#[derive(Default)]
pub struct Applied {
    pub error: Option<String>,
    pub preview: Option<rbs_gpu::share::PreviewEndpoint>,
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
