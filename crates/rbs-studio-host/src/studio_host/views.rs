//! What the host sends the window: scene and source lists, canvases, projectors and notes.

use super::*;

/// A scene the canvas may show, as the Scenes panel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneRow {
    /// Its id.
    pub id: SceneId,
    /// Its name.
    pub name: String,
    /// The scene keeps its own levels (ADR-0091).
    pub own_audio: bool,
    /// The transition into this scene (ADR-0135); `None`: the collection's.
    pub transition_in: Option<rbs_scene::SceneTransition>,
    /// Next, previous and slot navigation reach it (ADR-0195).
    pub active: bool,
}

/// A source of the collection, for "Add existing source".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    /// Its id.
    pub id: SourceId,
    /// Its name.
    pub name: String,
    /// Its kind; `None` for a type this build does not know.
    pub kind: Option<SourceKind>,
    /// A global source of the studio's canvas (ADR-0065).
    pub global: bool,
    /// Per-source slide override (ADR-0100).
    pub glide: Option<rbs_scene::SourceGlide>,
    /// Which feeds are marked to record as files of their own (ADR-0086).
    pub record_separately: RecordSeparately,
}

/// The scenes and sources of the collection, as the Scenes panel and the source
/// pickers show them (ADR-0059).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneList {
    /// The scenes the canvas may show, in panel order.
    pub scenes: Vec<SceneRow>,
    /// The scenes a stream of each canvas (main, vertical) may hold with (ADR-0124
    /// phase 2b), in panel order; the vertical one empty while it does not exist.
    pub hold_scenes: [Vec<SceneRow>; 2],
    /// The one on air.
    pub program: SceneId,
    /// The preview scene in studio mode (ADR-0061); `None` outside it.
    pub preview: Option<SceneId>,
    /// The canvas's master volume and mute (ADR-0081).
    pub master: MasterAudio,
    /// Collection morph defaults (ADR-0100).
    pub glide: rbs_scene::CollectionGlide,
    /// The collection's transition (ADR-0128).
    pub transition: rbs_scene::CollectionTransition,
    /// The stinger's found cut point in ms (ADR-0140 decision 3); `None` until found.
    pub stinger_cut_found_ms: Option<u32>,
    /// The length of the stinger's play range in ms, once its file was looked at.
    pub stinger_length_ms: Option<u32>,
    /// The stinger's file is being looked at.
    pub stinger_looking: bool,
    /// The stinger's file has transparency no adapter here keeps (decision 9).
    pub stinger_alpha_unsupported: bool,
    /// Every source of the collection, by name.
    pub sources: Vec<SourceEntry>,
}

/// Smallest and largest side of a vertical canvas the control API accepts: even (the
/// 4:2:0 formats of the encoders need it) and within what the hardware encoders take.
pub(super) const VERTICAL_MIN_SIDE: u32 = 64;
pub(super) const VERTICAL_MAX_SIDE: u32 = 8192;

/// What `canvas.add_vertical` makes (ADR-0093 decision 8).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalCanvas {
    /// The picture size, for example 1080x1920 or 720x1280. It runs at the main canvas's
    /// frame rate.
    pub size: Size,
    /// Shows the main canvas's program scene and switches with it (the default for a
    /// vertical canvas); else it keeps the scene on air now and is switched on its own.
    pub follow: bool,
    /// How to arrange the program scene's sources for the new shape; `None`: they keep
    /// the layout of the main canvas.
    pub layout: Option<AutoLayout>,
}

/// The second canvas as the host runs it ([`Update::Vertical`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerticalState {
    /// Its canvas.
    pub canvas: CanvasId,
    /// Its picture size.
    pub size: Size,
    /// It shows the main canvas's program scene.
    pub follow: bool,
}

/// What became of a projector (ADR-0125 decisions 3 and 11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectorState {
    /// Open: its picture runs or is being prepared.
    Open,
    /// Kept with no picture until its canvas comes back (the vertical canvas was
    /// removed, or the collection has none).
    Suspended,
    /// Closed by the studio, with the reason to tell the user: a limit was reached or
    /// its picture failed. The UI closes the window.
    Closed(String),
}

/// At most this many projectors are open at a time (ADR-0125 decision 4).
pub const MAX_PROJECTORS: usize = 4;
/// Surface memory all open projector pictures may hold together, counted at
/// [`MAX_PREVIEW_SURFACES`] surfaces of 4 bytes per pixel each (ADR-0125 decision 4).
pub const MAX_PROJECTOR_SURFACE_BYTES: u64 = 512 * 1024 * 1024;

/// The surface memory a projector of a `size` canvas counts toward
/// [`MAX_PROJECTOR_SURFACE_BYTES`].
pub fn projector_surface_bytes(size: Size) -> u64 {
    u64::from(size.width) * u64::from(size.height) * 4 * MAX_PREVIEW_SURFACES as u64
}

/// Why a projector of `size` may not open next to projectors counting `open` bytes
/// in `count` projectors; `None` when it may.
pub fn projector_refusal(count: usize, open: u64, size: Size) -> Option<&'static str> {
    if count >= MAX_PROJECTORS || open + projector_surface_bytes(size) > MAX_PROJECTOR_SURFACE_BYTES
    {
        return Some("Too many projectors are open. Close one first.");
    }
    None
}

/// One projector as the host keeps it (ADR-0125 decision 3).
pub(super) struct ProjectorRun {
    pub(super) id: u32,
    pub(super) canvas: OutputCanvas,
    /// Its picture in the running studio; `None` while suspended or not yet added.
    pub(super) picture: Option<PictureId>,
    pub(super) closing: bool,
    /// Counted toward the limits (its canvas's size) while it has a picture.
    pub(super) size: Size,
    /// What was last sent to the UI.
    pub(super) sent: Option<(Option<PreviewEndpoint>, ProjectorState)>,
}

/// The main display's framebuffer, for the Video settings row (ADR-0102 decision 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainDisplay {
    /// Physical pixels. Not multiplied by [`Self::scale`].
    pub size: (u32, u32),
    /// OS pixels per logical point, when the adapter read it. `None` when it did not
    /// (the label then has no percent).
    pub scale: Option<rbs_capture::DisplayScale>,
}

/// What the Mixer and Settings › Audio show of the loudness meter ([`Update::Loudness`]).
#[derive(Clone, Debug)]
pub struct LoudnessView {
    /// Show loudness is on (`loudness.set_enabled`, saved in `loudness.toml`).
    pub shown: bool,
    /// The running meter's values; `None` while it is off or the studio has no audio.
    pub readout: Option<Arc<rbs_audio::LoudnessReadout>>,
    /// The target, LUFS.
    pub target: f32,
    /// The true-peak limit, dBTP.
    pub true_peak_limit: f32,
}

impl PartialEq for LoudnessView {
    fn eq(&self, o: &Self) -> bool {
        let same = match (&self.readout, &o.readout) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        same && self.shown == o.shown
            && self.target.to_bits() == o.target.to_bits()
            && self.true_peak_limit.to_bits() == o.true_peak_limit.to_bits()
    }
}

/// A live stream's hold state for the window (ADR-0124), as of the moment it was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamHold {
    /// It is on hold now (or the hold waits for a sharing recording's pause).
    pub held: bool,
    /// A hold scene fell back (ADR-0124 phase 2b), while it does.
    pub note: Option<HoldNote>,
}

/// How a scene hold fell back, for the Stream card's notice (ADR-0124 phase 2b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldNote {
    /// The scene could not open or went away: the last frame and silence go out.
    Picture,
    /// The scene's sound failed: its picture goes out with silence.
    Sound,
}

/// A running recording's pause state for the window (ADR-0124), as of the moment it
/// was sent: the window moves its clocks on from there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordPause {
    /// It is paused now.
    pub paused: bool,
    /// What the files hold: the time recorded, pauses left out.
    pub recorded_ns: u64,
}

/// A marker just added, for the window's notice (ADR-0089 decision 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerNote {
    /// Its label ("Marker 3").
    pub label: String,
    /// Its place in the recording, from the start, after the lead.
    pub t_ns: u64,
    /// Each recording's canvas and the marker's index there, to undo it.
    pub placed: Vec<(OutputCanvas, u32)>,
}
