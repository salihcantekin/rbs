//! The window's side of the studio (ADR-0041 decision 7): state shown in the `Studio`
//! global, commands to the control thread, the meters' 30 Hz refresh while the Mixer is
//! visible, the Program panel's preview layer, recording and streaming with their
//! output settings.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_app::canvas_profile::{self, CanvasProfile, RATES, SIZES};
use rbs_app::encoder_form::{Choices, EncoderForm};
use rbs_app::network_access::{self, Source};
use rbs_app::record_profile::{
    self, AUDIO_KBPS, Encoding, Format, RateMode, RecordProfile, VIDEO_KBPS,
};
use rbs_app::remote_control::{NetworkOptions, RemoteOptions, RemoteServer};
use rbs_app::stream_key::{self, KeyStore};
use rbs_app::stream_profile::{self, SRT_DEFAULT_LATENCY_MS, SRT_LATENCIES_MS, StreamProfile};
use rbs_app::studio_host::{
    Command, EncoderInfo, EncoderOptions, RecordRequest, RecordingUpdate, SourceKind, SourceRow,
    SourceStatus, StreamRequest, StreamUpdate, StudioHost, Target, Update,
};
use rbs_audio::LevelMeter;
use rbs_record::Link;
use rbs_remote_settings::{self as remote_profile, Access, RemoteProfile};
use rbs_scene::model::SourceId;
use rbs_transport::Secret;
use rbs_ui::meter::{self, Meter, SCALE_MARKS_DB};
use rbs_ui::transform::{DragKeys, MAX_MARKS, Mark, MarkKind, SourceBox};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::preview_edit::{Effect, Placed, PreviewEdit};
use crate::ui::{
    EncoderData, KindData, MarkData, MeterRow, ScaleMark, SourceData, StackData, Studio,
    TargetData, TokenRow,
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
    /// The selected source's id (-1 none), shown in every window's Properties.
    pub selected: i32,
    /// An Add source is on its way: the next new source is selected.
    adding: bool,
    /// Moving and resizing on the picture panel (ADR-0047).
    edit: PreviewEdit,
    /// The selection as marks, panel coordinates; and as the toolkit draws them.
    mark_buf: Vec<Mark>,
    marks: Rc<VecModel<MarkData>>,
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
    pub fn new(config: &Path, keys: &dyn KeyStore) -> Self {
        let (profile, why) = record_profile::load(config);
        let (mut stream, stream_why) = stream_profile::load(config);
        // A key kept only in memory is gone after a restart.
        stream.key_saved &= keys.persistent();
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
            selected: -1,
            adding: false,
            edit: PreviewEdit::default(),
            mark_buf: Vec::with_capacity(MAX_MARKS),
            marks: Rc::new(VecModel::default()),
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
            network: None,
            network_failure: None,
            network_note: String::new(),
            stream,
            stream_note: stream_why
                .map(|w| format!("The saved stream settings were not used: {w}"))
                .unwrap_or_default(),
            server_serial: 1,
            key_serial: 0,
            key_hint: stream_key::hint(keys),
            st_state: st_state::OFF,
            st_live: None,
            st_link: Link::default(),
            st_note: String::new(),
            st_bad: false,
        };
        ui.refresh_tokens();
        ui
    }

    /// Gives a window's `Studio` global the shared models (once per window).
    pub fn attach_models(&self, g: &Studio<'_>) {
        g.set_sources(ModelRc::from(self.sources.clone()));
        g.set_marks(ModelRc::from(self.marks.clone()));
        g.set_kinds(ModelRc::from(self.kinds.clone()));
        g.set_targets(ModelRc::from(self.targets.clone()));
        g.set_meters(ModelRc::from(self.meters.clone()));
        g.set_scale(ModelRc::from(self.scale.clone()));
        g.set_rc_tokens(ModelRc::from(self.tokens.clone()));
    }

    /// Pushes the scalar state into a window's `Studio` global.
    pub fn push(&self, g: &Studio<'_>, preview_state: i32, picture_panel: &str) {
        g.set_selected_source(self.selected);
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
            self.adding = true;
            self.send(Command::AddSource { kind, target });
        }
    }

    pub fn delete_source(&mut self, id: i32) {
        self.deleted.clear();
        self.send(Command::DeleteSource(SourceId::from_raw(id as u32)));
    }

    /// One layer up (drawn over the source above it in the list) or down.
    pub fn move_source(&mut self, id: i32, up: bool) {
        self.send(Command::MoveSource {
            id: SourceId::from_raw(id as u32),
            up,
        });
    }

    /// A video source's box on the canvas, in canvas pixels (the Properties fields;
    /// the picture panel's drags go through [`PreviewEdit`], numbered).
    pub fn set_source_box(&mut self, id: i32, x: f32, y: f32, w: f32, h: f32) {
        self.send_box(0, id, SourceBox::new([x, y], [w, h]));
    }

    fn send_box(&mut self, seq: u64, id: i32, b: SourceBox) {
        if self.host.is_none() {
            // Nothing will answer: forget what the picture panel has on its way.
            self.edit.reset();
            return;
        }
        let (position, size) = (b.position, b.size);
        self.send(Command::SetSourceBox {
            seq,
            id: SourceId::from_raw(id as u32),
            position,
            size,
        });
    }

    /// The video sources as the picture panel places them, bottom first.
    fn placed(&self) -> Vec<Placed> {
        let mut v: Vec<Placed> = self
            .sources
            .iter()
            .filter(|d| d.picture)
            .map(|d| Placed {
                id: d.id,
                at: SourceBox::new([d.x, d.y], [d.w, d.h]),
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
            Effect::Send(seq, id, b) => self.send_box(seq, id, b),
        }
        self.refresh_marks();
    }

    /// A press on the picture panel (panel coordinates, its size `size`).
    pub fn preview_press(&mut self, at: [f32; 2], size: [f32; 2], shift: bool, no_snap: bool) {
        self.edit.set_panel(size[0], size[1]);
        let keys = DragKeys {
            free: shift,
            no_snap,
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
        if self.rec_state != rec_state::OFF || self.st_state != st_state::OFF {
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
        if self.st_state != st_state::OFF || self.rec_state != rec_state::OFF {
            return;
        }
        let fail = |ui: &mut Self, why: String| {
            ui.st_note = why;
            ui.st_bad = true;
        };
        if self.cannot_record.is_some() {
            // Live controls already say why (the button is disabled).
            return;
        }
        if let Some(why) = self.stream.missing() {
            return fail(self, why.to_owned());
        }
        if let Some(why) = self.stream.problem() {
            return fail(self, why);
        }
        let Some(server) = self.stream.parsed() else {
            return fail(self, "Enter the server in Settings, Stream.".to_owned());
        };
        let e = &self.stream.encoding;
        let request = StreamRequest {
            url: self.stream.server.clone(),
            input: server.protocol.input(),
            needs_key: server.protocol.needs_key(),
            reconnect: self.stream.reconnect_policy(),
            transport_options: self.stream.transport_options(),
            encoder: self.encoder_id(e),
            video: e.video_settings(),
            audio_bitrate_bps: e.audio_bps(),
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
        self.stream_note = match stream_profile::save(&self.config, &self.stream) {
            Ok(()) => String::new(),
            Err(e) => format!("The stream settings could not be saved: {e}"),
        };
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
            _ if self.rec_state != rec_state::OFF => "Stop the recording to go live.".to_owned(),
            _ => self.st_note.clone(),
        }
    }

    fn push_stream(&self, g: &Studio<'_>) {
        let st = &self.stream;
        g.set_st_state(self.st_state);
        g.set_st_connected(self.st_state == st_state::LIVE && self.st_link.connected);
        g.set_st_text(self.stream_text().into());
        g.set_st_bad(
            self.st_state == st_state::OFF && self.rec_state == rec_state::OFF && self.st_bad,
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
                self.st_state = st_state::LIVE;
                if let Some((_, since)) = &mut self.st_live {
                    *since = Instant::now();
                }
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
                // The user ended it (not the server or a failure).
                let asked = self.st_state == st_state::ENDING;
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
        if self.st_state != st_state::OFF {
            self.canvas_note = "End the stream before changing the canvas.".into();
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
        // A studio that starts, stops or restarts answers nothing sent before.
        if matches!(
            u,
            Update::Started { .. } | Update::Failed(_) | Update::Canvas { .. }
        ) {
            self.edit.reset();
        }
        match u {
            Update::BoxPlaced { seq, ok } => {
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
            Update::Sources(rows) => {
                self.set_rows(rows);
                self.refresh_marks();
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
                self.target_list = list;
            }
            Update::Deleted(name) => {
                self.deleted = name;
                self.deleted_serial += 1;
            }
            Update::Error(e) => {
                self.adding = false;
                out.error = Some(e);
            }
            Update::Recording(r) => self.recording(r, &mut out),
            Update::Stream(u) => self.stream_update(u, &mut out),
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
        let pictures: Vec<usize> = (0..rows.len())
            .filter(|i| rows[*i].layout.is_some())
            .collect();
        let data: Vec<SourceData> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (status, ok) = match &r.status {
                    SourceStatus::Running => ("Running".to_owned(), true),
                    SourceStatus::Idle => ("Not shown, closed".to_owned(), true),
                    SourceStatus::Failed(why) => (format!("Not running: {why}"), false),
                };
                let (x, y, w, h) = r.layout.map_or((0.0, 0.0, 0.0, 0.0), |l| {
                    (l.position[0], l.position[1], l.size[0], l.size[1])
                });
                SourceData {
                    id: r.id.get() as i32,
                    name: r.name.as_str().into(),
                    kind: r.kind.map_or("Unknown type", SourceKind::label).into(),
                    status: status.into(),
                    ok,
                    picture: r.layout.is_some(),
                    x,
                    y,
                    w,
                    h,
                    // Rows are top first.
                    can_up: r.layout.is_some() && pictures.first().is_some_and(|f| *f < i),
                    can_down: r.layout.is_some() && pictures.last().is_some_and(|l| *l > i),
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
