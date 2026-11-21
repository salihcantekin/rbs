//! What a composition root hands the host (HostConfig) and the faults tests inject.

use super::*;

/// How the studio starts.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Canvas size.
    pub size: Size,
    /// Canvas rate.
    pub fps: FrameRate,
    /// The preview's size, or `None` for no preview. The studio sets the ring
    /// (`surfaces`) from the canvas frame rate when it starts and whenever the
    /// canvas changes (ADR-0034, ADR-0036); a count stored here is replaced.
    pub preview: Option<PreviewDesc>,
    /// Where the stream key is kept: read when a stream starts, written when it is
    /// saved, on the control thread only. Memory by default.
    pub keys: Arc<dyn KeyStore>,
    /// The key store account of the output profile in use ([`stream_key::account`]);
    /// [`stream_key::ACCOUNT`] by default.
    pub key_account: String,
    /// The scene collection to run (ADR-0051). Its first canvas is the studio's; that
    /// canvas's program scene is the one shown and edited (a scene is added when it
    /// has none). `None`: a new collection with one canvas of `size` at `fps`.
    pub collection: Option<SceneCollection>,
    /// Saves the collection after every change (on the user data writer thread);
    /// `None`: changes are not saved.
    pub autosave: Option<CollectionSaver>,
    /// The platform's virtual camera, `None` where there is none: the studio sends
    /// to it on [`Command::StartVirtualCamera`].
    pub virtual_camera: Option<CameraFactory>,
    /// The config folder where the recent targets ([`crate::recent_targets`]) and
    /// `calibration.toml` are kept; `None`: they are kept in memory only.
    pub config_dir: Option<PathBuf>,
    /// A transition also puts the old program scene in preview (ADR-0098); false by
    /// default.
    pub swap_after_transition: bool,
    /// How long a paused or cued media file keeps its decoder (`pause_release_s`,
    /// ADR-0108 decision 10); 30 s by default.
    pub media_pause_release: Duration,
    /// The monitor output's device and volume (ADR-0097; `monitor.toml`). The device
    /// is opened only while a source is monitored or soloed.
    pub monitor: MonitorSettings,
    /// The loudness meter's saved settings (ADR-0092; `loudness.toml`): Show loudness,
    /// the target and the true-peak limit.
    pub loudness: crate::loudness_settings::LoudnessSettings,
    /// Reads free disk space and memory for recording protection (ADR-0090); `None`:
    /// disk and memory are reported unavailable and only lag is watched.
    pub system_probe: Option<protection::SystemProbeFactory>,
    /// The last calibration of this computer (`calibration.toml`, ADR-0087 decision
    /// 4), read with `app.toml` before the control thread starts; empty by default.
    pub calibration: crate::calibration_file::CalibrationFile,
    /// Reads a recording folder's free space for the estimate's disk probe (ADR-0087
    /// decision 1), on the probe's own short-lived thread; `None`: free space is
    /// unknown.
    pub read_free_space: Option<FreeSpaceReader>,
    /// Writes markers into a finished Matroska master as chapters (ADR-0159 decision 4);
    /// `None`: the markers stay in the manifest only.
    pub chapters: Option<Arc<dyn ChapterWriter>>,
    /// The OS name and version in the calibration key; always pass it (ADR-0159).
    pub os_version: Option<String>,
    /// The main display for the Video settings row at start (ADR-0102 decision 8);
    /// `None`: unknown. Off macOS, roots pass [`display_from_default_screen`].
    pub main_display: Option<MainDisplayProbe>,
    /// The calibration run's clock, pacer, file system and wall time (ADR-0087 step
    /// D1); `None`: the real ones. Tests inject a test clock and a fake sink.
    pub calibration_setup: Option<calibration::CalibrationSetup>,
    /// The add-on catalog, where add-ons are kept and how they are fetched (ADR-0145;
    /// `rbs_app::add_ons::system_setup` in the app). `None`: `addon.list`
    /// shows the built-in catalog as not installed and installs are refused.
    pub add_ons: Option<crate::add_ons::AddOnsSetup>,
    /// Makes the browser tree at each start (ADR-0159 decision 4); `None`: no helper,
    /// so browser sources are Unavailable with `helper_missing`.
    pub browser: Option<Arc<dyn BrowserTrees>>,
    /// A step to panic at once (tests only, ADR-0055).
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fault: Option<Fault>,
}

impl HostConfig {
    /// 1920 x 1080 at 30 frames per second with a 1280 x 720 preview where the platform
    /// has one (macOS, Windows).
    pub fn desktop() -> Self {
        Self::canvas(Size::new(1920, 1080), FrameRate::FPS_30)
    }

    /// A canvas of `size` at `fps`, with a preview of its aspect ratio where the
    /// platform has one (macOS, Windows).
    pub fn canvas(size: Size, fps: FrameRate) -> Self {
        let preview = cfg!(any(target_os = "macos", windows)).then(|| preview_desc(size, fps));
        Self {
            size,
            fps,
            preview,
            keys: Arc::new(MemoryKeyStore::new()),
            key_account: stream_key::ACCOUNT.to_owned(),
            collection: None,
            autosave: None,
            virtual_camera: None,
            config_dir: None,
            swap_after_transition: false,
            media_pause_release: rbs_record::MEDIA_PAUSE_RELEASE,
            monitor: MonitorSettings::default(),
            loudness: crate::loudness_settings::LoudnessSettings::default(),
            system_probe: None,
            calibration: crate::calibration_file::CalibrationFile::default(),
            read_free_space: None,
            chapters: None,
            os_version: None,
            main_display: None,
            calibration_setup: None,
            add_ons: None,
            browser: None,
            #[cfg(feature = "fault-injection")]
            fault: None,
        }
    }
}

/// A panic the host raises once at a chosen step, for the crash-resilience tests
/// (ADR-0055). Only with the `fault-injection` feature, which release builds never use.
#[cfg(feature = "fault-injection")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// In a recording start, after the output started and before the host tracks it.
    AfterOutputStart,
    /// In a recording stop, after the output stopped and before the host forgets it.
    InRecordingStop,
    /// In a stream start, after the output started and before the host tracks it.
    AfterStreamStart,
    /// In a stream end, after the output stopped and before the host forgets it.
    InStreamEnd,
    /// While a canvas change is restarting the studio, before that restart is over.
    DuringCanvasRestart,
}
