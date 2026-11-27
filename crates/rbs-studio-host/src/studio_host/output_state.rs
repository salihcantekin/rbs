//! What the outputs keep while they run: recording and stream plans, holds and destinations.

use super::*;

/// The separate files of the main recording (ADR-0086).
#[derive(Debug, Clone)]
pub(super) struct IsoFiles {
    /// The tracks, as `record.status` lists them.
    pub(super) tracks: Vec<api::RecordTrack>,
    /// Marked sources left out, as `record.status` lists them.
    pub(super) skipped: Vec<api::SkippedTrack>,
    /// The manifest written at start, and its folder; markers are added to it at stop
    /// (ADR-0089 decision 5).
    pub(super) manifest: rbs_manifest::RecordingManifest,
    /// The recording's folder.
    pub(super) folder: PathBuf,
    /// The image tracks, whose pictures go into the manifest at stop (ADR-0117
    /// decision 6).
    pub(super) images: Vec<rbs_record::IsoImageTrack>,
    /// The start gate's decision for the picture files, used once when they open.
    pub(super) gate: Option<PictureGate>,
    /// Video files skipped because their encoder would have fallen to a software one
    /// (ADR-0087 decision 5): `software_fallback` in the start answer's estimate.
    pub(super) software_stopped: Vec<SourceId>,
    /// The pause window the sound files follow; the master is started with it
    /// (ADR-0124).
    pub(super) pause: rbs_record::SharedPause,
}

/// How a `record.start` asks for its separate video files (control API 1.54,
/// ADR-0087 decisions 5 and 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartGate {
    /// Neither `acknowledge_heavy` nor `on_hardware_limit` (older clients, the
    /// desktop buttons, hotkeys): never refused because of separate video files.
    Older,
    /// With either parameter.
    Asked {
        /// The caller accepts a `heavy` plan.
        acknowledge_heavy: bool,
        /// What to do with files past their encoder's capacity.
        policy: LimitPolicy,
    },
}

/// Why a recording did not start.
#[derive(Debug)]
pub(super) enum StartError {
    /// In words.
    Message(String),
    /// The start gate refused it (control API 1.54): `failed` with `data`.
    Refused(String, Box<api::StartRefusal>),
    /// The destination carries fewer audio tracks than the profile names (ADR-0170
    /// decision 7): `failed` with `data.reason: "tracks_unsupported"` and
    /// `data.allowed`.
    Tracks(String, usize),
}

impl From<String> for StartError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<Refusal> for StartError {
    fn from(e: Refusal) -> Self {
        Self::Message(e.into_text())
    }
}

impl StartError {
    pub(super) fn into_message(self) -> String {
        match self {
            Self::Message(m) | Self::Refused(m, _) | Self::Tracks(m, _) => m,
        }
    }

    pub(super) fn into_api(self) -> ApiError {
        match self {
            Self::Message(m) => failed(m),
            Self::Refused(m, r) => r.into_error(m),
            Self::Tracks(m, allowed) => {
                let mut e = failed_because("tracks_unsupported", m);
                if let Some(data) = e.data.as_mut() {
                    data.insert("allowed".into(), allowed.into());
                }
                e
            }
        }
    }
}

/// The start gate's decision for the separate video files, applied once the master
/// runs (pictures resolve only then).
#[derive(Debug, Clone)]
pub(super) struct PictureGate {
    /// The only sources that may open a video file (a request with the 1.54
    /// parameters); `None`: every marked source in source id order, a later one taking
    /// the slot of one without a picture (1.52's behaviour).
    pub(super) keep: Option<Vec<SourceId>>,
    /// Most video files to open.
    pub(super) count: usize,
    /// No encoder can take a separate video file: the gate's skips are `open_failed`.
    pub(super) no_encoder: bool,
    /// `on_hardware_limit: "ask"`: a file that cannot open refuses the start.
    pub(super) ask: bool,
    /// The plan the gate judged, for a refusal found while files open.
    pub(super) judged: rbs_record::estimate::Estimate,
}

/// Separate video files a take may open while the encoder session limit of this
/// computer is not known (neither measured nor declared): the owner's choice of
/// 2026-11-14 in ADR-0087's Review. With a known limit the estimate decides
/// ([`rbs_record::estimate::separate_video_limit`]). Marked sources past the limit are
/// skipped, in source id order, and listed with the reason `limit`. Image and sound
/// files are not counted.
pub const SEPARATE_VIDEO_LIMIT: usize = rbs_record::estimate::UNKNOWN_SESSIONS_VIDEO_LIMIT;

/// Each separate file's media when the recording stopped, by file name.
pub(super) type SeparateMedia = Vec<(String, Option<rbs_record::MediaSpan>)>;

/// The canvas an output records or streams (ADR-0093 decision 6).
pub use calibration::{CalibrationSetup, ExportRigMaker};
pub use rbs_api::types::OutputCanvas;
pub use sync_measure::{SyncMeasurePhase, SyncMeasureRefusal, SyncMeasureView};

/// `c`'s place in the host's per-canvas state.
/// A refused pause or resume in words, for its `conflict` (control API 1.61).
pub(super) fn pause_refusal(e: rbs_record::PauseRefused) -> String {
    use rbs_record::PauseRefused as R;
    match e {
        R::KeyframeUnsupported => "the recording shares its video encoder with another \
            output, and that encoder cannot make a keyframe on request, so the file \
            could not go on cleanly after the pause"
            .to_owned(),
        R::NotPausable => "this recording cannot be paused".to_owned(),
        R::SeparateFiles => "separate files run that do not belong to this recording, so they \
             would go on while it is paused"
            .to_owned(),
        e => e.to_string(),
    }
}

/// How long a marker waits before the manifest is rewritten with it (ADR-0089
/// decision 5).
pub(super) const MARKER_FLUSH_NS: u64 = 5_000_000_000;

/// `Ok` when an entry changed; else the first refusal, or `conflict` with `none`.
pub(super) fn changed_or_first_refusal(
    outputs: &[api::OutputEntry],
    refusals: Vec<ApiError>,
    none: &str,
) -> Result<(), ApiError> {
    if outputs.iter().any(|o| {
        matches!(
            o.outcome,
            api::OutputOutcome::Paused | api::OutputOutcome::Resumed | api::OutputOutcome::Held
        )
    }) {
        return Ok(());
    }
    Err(refusals
        .into_iter()
        .next()
        .unwrap_or_else(|| ApiError::new(ErrorKind::Conflict, none)))
}

pub(super) fn hold_refusal(e: rbs_record::HoldRefused) -> String {
    use rbs_record::HoldRefused as R;
    match e {
        R::NotHoldable => "this stream cannot be held".to_owned(),
        R::PictureUnsupported => {
            "the hold picture cannot be made for this stream's video format".to_owned()
        }
        e => e.to_string(),
    }
}

/// The engine's hold picture for the control API's.
pub(super) fn hold_picture(p: rbs_api::request::HoldPicture) -> HoldPicture {
    match p {
        rbs_api::request::HoldPicture::Freeze => HoldPicture::Freeze,
        rbs_api::request::HoldPicture::Black => HoldPicture::Black,
        rbs_api::request::HoldPicture::Scene => HoldPicture::Scene,
    }
}

/// The control API's hold picture for the engine's (`freeze` for one it does not
/// know).
pub(super) fn api_hold_picture(p: HoldPicture) -> rbs_api::request::HoldPicture {
    match p {
        HoldPicture::Black => rbs_api::request::HoldPicture::Black,
        HoldPicture::Scene => rbs_api::request::HoldPicture::Scene,
        _ => rbs_api::request::HoldPicture::Freeze,
    }
}

/// The studio's hold sound for the control API's.
pub(super) fn hold_sound(s: rbs_api::request::HoldSound) -> rbs_record::HoldSound {
    match s {
        rbs_api::request::HoldSound::Silence => rbs_record::HoldSound::Silence,
        rbs_api::request::HoldSound::Scene => rbs_record::HoldSound::Scene,
    }
}

/// The control API's hold sound for the studio's.
pub(super) fn api_hold_sound(s: rbs_record::HoldSound) -> rbs_api::request::HoldSound {
    match s {
        rbs_record::HoldSound::Scene => rbs_api::request::HoldSound::Scene,
        _ => rbs_api::request::HoldSound::Silence,
    }
}

/// A refused hold as a control API error (protocol 1.67): a scene not in the
/// collection is `not_found`, one the canvas cannot show `invalid_params`, the rest
/// `conflict`.
pub(super) fn hold_error(e: rbs_record::HoldRefused) -> ApiError {
    use rbs_record::HoldRefused as R;
    let kind = match e {
        R::UnknownScene => ErrorKind::NotFound,
        R::SceneNotShowable => ErrorKind::InvalidParams,
        _ => ErrorKind::Conflict,
    };
    let message = match e {
        R::OtherSceneHeld => {
            "the stream is already on hold with another scene or sound: resume it first".to_owned()
        }
        e => hold_refusal(e),
    };
    ApiError::new(kind, message)
}

pub(super) fn slot(c: OutputCanvas) -> usize {
    match c {
        OutputCanvas::Main => 0,
        OutputCanvas::Vertical => 1,
    }
}

/// `u` as the update of `c`'s recording.
pub(super) fn recording_update(c: OutputCanvas, u: RecordingUpdate) -> Update {
    match c {
        OutputCanvas::Main => Update::Recording(u),
        OutputCanvas::Vertical => Update::VerticalRecording(u),
    }
}

/// `u` as the update of `c`'s stream.
pub(super) fn stream_update(c: OutputCanvas, u: StreamUpdate) -> Update {
    match c {
        OutputCanvas::Main => Update::Stream(u),
        OutputCanvas::Vertical => Update::VerticalStream(u),
    }
}

/// The state key of `c`'s outputs on a per-canvas topic (protocol 1.29).
pub(super) fn canvas_key(topic: Topic, c: OutputCanvas) -> StateKey {
    StateKey {
        topic,
        object: Some(c.object()),
    }
}

/// The recording and stream of one canvas, as the host tracks them.
#[derive(Debug)]
pub(super) struct CanvasOutputs {
    /// The running recording: its file and output (ADR-0047: it runs alongside a
    /// stream).
    pub(super) recording: Option<(PathBuf, OutputId)>,
    /// Stop was asked for: the recording ended its video and stops once its tail is
    /// in (ADR-0045).
    pub(super) ending: bool,
    /// The running recording was started with the stream ("Record when the stream
    /// starts"): ending the stream stops it too. Cleared when the recording stops or
    /// the stream ends.
    pub(super) auto_recording: bool,
    /// The running stream: its URL, the connection last reported and its output. With
    /// several destinations (ADR-0127) this is one destination that still runs, the
    /// stream's anchor: its output carries the stream's hold and counters.
    pub(super) streaming: Option<(String, Link, OutputId)>,
    /// The destinations of the running stream, the anchor's among them; empty while
    /// nothing streams.
    pub(super) destinations: Vec<LiveDestination>,
    /// The rows last sent with [`Update::LiveDestinations`], without their time on air.
    pub(super) destinations_shown: Vec<api::LiveDestination>,
    /// Recording protection ended the recording (ADR-0090 decision 3): the reason for
    /// its summary once the tail is in.
    pub(super) protection_stop: Option<String>,
    /// The recording, as the control API reports it.
    pub(super) record_state: api::RecordState,
    /// The stream, as the control API reports it.
    pub(super) stream_state: api::StreamState,
    /// The running recording's markers (ADR-0089); `None` while nothing records.
    pub(super) markers: Option<RecordingMarkers>,
    /// The stream's hold as the host keeps it (ADR-0124 decision 14).
    pub(super) hold: HoldPlan,
    /// The stats summary in the log (ADR-0158), on the main canvas only.
    pub(super) log_summary: log_summary::LogSummary,
}

/// A destination Go live is about to start: its request and key account.
pub(super) struct Planned {
    /// What it becomes once started.
    pub(super) live: LiveDestination,
    /// Where and how to stream to it.
    pub(super) request: StreamRequest,
    /// Its key's account in the key store.
    pub(super) account: String,
    /// It is its canvas's first destination.
    pub(super) first: bool,
}

impl Planned {
    pub(super) fn of(d: &Resolved, request: StreamRequest, first: bool) -> Self {
        Self {
            live: LiveDestination {
                id: d.destination.id,
                name: d.destination.name.clone(),
                service: d.destination.service.clone(),
                url: request.url.clone(),
                own: d.destination.encoding.is_own(),
                output: None,
                state: DestinationState::Connecting,
                link: Link::default(),
                error: None,
                on_air_since: None,
            },
            request,
            account: d.account.clone(),
            first,
        }
    }

    /// It started as `output`.
    pub(super) fn started(self, output: OutputId) -> LiveDestination {
        LiveDestination {
            output: Some(output),
            ..self.live
        }
    }

    /// It did not start, because of `why`.
    pub(super) fn failed(self, why: String) -> LiveDestination {
        LiveDestination {
            state: DestinationState::Failed,
            error: Some(why),
            ..self.live
        }
    }
}

/// The studio output of a stream to `r`'s server with `key`, from `canvas`.
pub(super) fn stream_output(
    r: &StreamRequest,
    key: Option<Secret>,
    canvas: Option<CanvasId>,
    adapters: &RecordAdapters,
) -> OutputSettings {
    let mut out = match r.input {
        TransportInput::Container(c) => OutputSettings::new(r.url.clone(), c),
        _ => OutputSettings::packets(r.url.clone()),
    };
    // Never in a crash report (ADR-0055 decision 4).
    if let Some(key) = &key {
        rbs_crash::add_secret(key.expose());
    }
    rbs_crash::add_secret(&r.url);
    out.secret = key;
    out.canvas = canvas;
    out.reconnect = r.reconnect;
    out.transport_options = r.transport_options.clone();
    out.video = Some(r.video.clone());
    out.audio = adapters.audio_encoders.ids().next().map(|_| {
        let mut a = AudioSettings::default();
        a.bitrate_bps = r.audio_bitrate_bps;
        a
    });
    // The size this stream starts with. A later settings change waits for the next
    // start (ADR-0102). `None` stays the canvas.
    out.size = r.size;
    out.audio_bus = Some(r.audio_bus.clone());
    // Every stream can be held (ADR-0124 decision 12).
    out.holdable = true;
    out
}

/// Where one destination of a running stream stands (ADR-0127 decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationState {
    /// Started; not connected yet.
    Connecting,
    /// Connected and sending.
    Live,
    /// The connection dropped; the transport tries again.
    Reconnecting,
    /// Refused at start, gave up reconnecting, or its output failed.
    Failed,
    /// Stopped by the user while the others run.
    Stopped,
}

/// One destination of a running stream, as the host tracks it.
#[derive(Debug, Clone)]
pub(super) struct LiveDestination {
    /// The destination's id in the profile.
    pub(super) id: u32,
    /// Its name, for notices.
    pub(super) name: String,
    /// Its service preset's id, or `custom` (control API only).
    pub(super) service: String,
    /// Its server URL (never with the key).
    pub(super) url: String,
    /// It has its own encoder chain.
    pub(super) own: bool,
    /// Its output while it runs.
    pub(super) output: Option<OutputId>,
    /// Where it stands.
    pub(super) state: DestinationState,
    /// The connection last read.
    pub(super) link: Link,
    /// Why it failed, the URL and key never in it.
    pub(super) error: Option<String>,
    /// When it first connected (master clock).
    pub(super) on_air_since: Option<Timestamp>,
}

impl LiveDestination {
    /// Its row in the stream's state (control API 1.77), as of `now`.
    pub(super) fn api(&self, now: Timestamp) -> api::LiveDestination {
        api::LiveDestination {
            id: self.id,
            name: self.name.clone(),
            service: self.service.clone(),
            state: match self.state {
                DestinationState::Connecting => api::DestinationState::Connecting,
                DestinationState::Live => api::DestinationState::Live,
                DestinationState::Reconnecting => api::DestinationState::Reconnecting,
                DestinationState::Failed => api::DestinationState::Failed,
                DestinationState::Stopped => api::DestinationState::Stopped,
            },
            connects: self.link.connects,
            reconnect_attempts: self.link.reconnect_attempts,
            dropped: self.link.transport_drops,
            on_air_ms: self.on_air_since.map_or(0, |t| {
                u64::try_from(now.saturating_since(t).as_millis()).unwrap_or(u64::MAX)
            }),
            error: self.error.clone(),
        }
    }

    /// The name to show: its own, or "Destination <id>".
    pub(super) fn label(&self) -> String {
        if self.name.is_empty() {
            format!("Destination {}", self.id)
        } else {
            self.name.clone()
        }
    }
}

/// What the host remembers about a stream's hold (ADR-0124 decision 14).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct HoldPlan {
    /// A hold asked for that waits until the recording sharing the stream's encoder
    /// is paused: the studio holds a shared chain only then, so the hold picture
    /// never reaches the file.
    pub(super) pending: Option<HoldAsk>,
    /// The hold paused the recording: Resume stream resumes it, and never a
    /// recording the user paused.
    pub(super) paused_recording: bool,
}

/// A hold as asked for, with the stream settings' defaults filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HoldAsk {
    pub(super) picture: HoldPicture,
    /// The scene, with [`HoldPicture::Scene`] only (phase 2b).
    pub(super) scene: Option<SceneId>,
    pub(super) sound: rbs_record::HoldSound,
}

impl Default for CanvasOutputs {
    fn default() -> Self {
        Self {
            recording: None,
            ending: false,
            auto_recording: false,
            streaming: None,
            destinations: Vec::new(),
            destinations_shown: Vec::new(),
            protection_stop: None,
            record_state: api::RecordState::Idle { last: None },
            stream_state: api::StreamState::idle(),
            markers: None,
            hold: HoldPlan::default(),
            log_summary: Default::default(),
        }
    }
}

impl CanvasOutputs {
    /// A recording or a stream runs.
    pub(super) fn busy(&self) -> bool {
        self.recording.is_some() || self.streaming.is_some()
    }
}

/// Why Go live and `stream.start` are refused while no destination of the canvas is
/// on (ADR-0148 decision 8).
pub const STREAMING_OFF: &str =
    "Streaming is off. Add a destination or turn one on in Settings, Stream.";

/// Where and how to stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRequest {
    /// The server URL (`rtmp://host/app`, `srt://host:port`); never holds the key.
    pub url: String,
    /// Packets (RTMP) or the chunks of a container (SRT: MPEG-TS).
    pub input: TransportInput,
    /// The server needs a key (RTMP): without a saved one nothing opens.
    pub needs_key: bool,
    /// What the transport does when the connection drops.
    pub reconnect: ReconnectPolicy,
    /// Opaque transport options (SRT `latency_ms`).
    pub transport_options: Vec<(String, String)>,
    /// The video encoder's adapter id: only this one is used. `None`: the best one.
    pub encoder: Option<String>,
    /// Video encoding.
    pub video: VideoSettings,
    /// Audio bitrate in bit/s.
    pub audio_bitrate_bps: u64,
    /// Encoded picture size (ADR-0102). `None`: this output's canvas.
    pub size: Option<(u32, u32)>,
    /// The bus the stream sends, by name (ADR-0170).
    pub audio_bus: String,
}

impl StreamRequest {
    /// The request for the saved stream settings, with `encoder` as the video encoder
    /// (`None`: the best one). Says why when they cannot be used: no server, a server
    /// that is not valid, or settings out of range. The key is not checked here: the
    /// host reads it from the key store when the stream starts.
    pub fn from_profile(p: &StreamProfile, encoder: Option<String>) -> Result<Self, Refusal> {
        if let Some(why) = p.problem() {
            return Err(Refusal::Invalid(why));
        }
        if p.server.is_empty() {
            return Err(Refusal::Invalid(
                "Enter the server in Settings, Stream.".to_owned(),
            ));
        }
        let server = parse_server(&p.server).map_err(|e| Refusal::Invalid(e.to_string()))?;
        Ok(Self {
            url: p.server.clone(),
            input: server.protocol.input(),
            needs_key: server.protocol.needs_key(),
            reconnect: p.reconnect_policy(),
            transport_options: p.transport_options(),
            ..Self::from_settings(p, encoder)?
        })
    }

    /// The stream's shared settings (encoding, output size) without a server: the
    /// base each destination's request is built on when the canvas's first slot does
    /// not exist (ADR-0148 decision 6). Its own URL is empty and never used.
    pub fn from_settings(p: &StreamProfile, encoder: Option<String>) -> Result<Self, Refusal> {
        if let Some(why) = p.problem() {
            return Err(Refusal::Invalid(why));
        }
        let e = &p.encoding;
        Ok(Self {
            url: String::new(),
            input: Protocol::Rtmp.input(),
            needs_key: false,
            reconnect: p.reconnect_policy(),
            transport_options: Vec::new(),
            encoder,
            video: e.video_settings(),
            audio_bitrate_bps: e.audio_bps(),
            size: p.output_size.map(|s| s.pair()),
            audio_bus: p.audio_bus.clone(),
        })
    }
}
