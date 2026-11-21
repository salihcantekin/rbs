//! How the control thread starts: the studio, the browser tree and the host's state from its config (ADR-0159 decision 8).

use super::*;

pub(super) fn start(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl Fn() -> Engine + 'static,
    levels: Arc<crate::levels::LevelFeed>,
) -> Result<(Host, Update), String> {
    let keys = Arc::clone(&config.keys);
    #[cfg(feature = "fault-injection")]
    let config_fault = config.fault;
    let (mut adapters, mut notes) = adapters();
    adapters.probe_io();
    let can_monitor = adapters.sources.can_monitor();
    let gpu = gpu()?;
    notes.push(format!("gpu: {}", gpu.adapter_name()));
    let loaded = config.collection.as_ref().map(|c| (0, c.generation()));
    let mut collection = config.collection.unwrap_or_default();
    let (canvas, scene) = shown_scene(&mut collection, config.size, config.fps)?;
    // The collection's canvas; `config.size` only chose the preview's aspect ratio.
    let (run_size, run_fps) = collection
        .canvas(canvas)
        .map_or((config.size, config.fps), |c| (c.size, c.frame_rate));
    // The runner ships with the app; the engine it loads is an add-on, resolved now
    // only when the collection has a browser source (ADR-0145 decision 6).
    // Signed-in profiles sit beside the add-ons, in the local data directory.
    let profiles = config.add_ons.as_ref().and_then(profiles_dir);
    let add_ons = add_ons::AddOnsHost::new(config.add_ons);
    let engine_slot = engine_slot(&collection, &add_ons);
    let browser = seams::browser_tree(
        config.browser.as_ref(),
        Arc::clone(&engine_slot),
        profiles.clone(),
        &gpu,
    );
    let browser_tree: Option<Arc<dyn BrowserTree>> = Some(Arc::clone(&browser) as _);
    let settings = studio_settings(
        config.preview,
        config.media_pause_release,
        canvas,
        run_fps,
        &collection,
    );
    let held = held_copy(&collection, canvas, None);
    let run = held.as_ref().and_then(|h| run_copy(&collection, canvas, h));
    let first = engine();
    let clock = first.clock();
    let studio = Studio::start_with_tree(
        first,
        Arc::clone(&gpu),
        run.as_ref().unwrap_or(&collection),
        &adapters.sources,
        &settings,
        browser_tree.clone(),
    )
    .map_err(|e| e.to_string())?;
    let display = seams::probe_display(config.main_display, &adapters.sources);
    let kinds = installed_kinds(&adapters.sources);
    // Video and audio encoders are probed on first use (ADR-0154's neighbour, F-08):
    // output settings about to open, or a recording or stream about to start.
    let encoders = Vec::new();
    let cannot_record = None;
    let update = Update::Started {
        preview: studio.preview(),
        kinds: Vec::clone(&kinds),
        notes,
        encoders: encoders.clone(),
        cannot_record: cannot_record.clone(),
        size: run_size,
        fps: run_fps,
        display,
    };
    let saved = SavedFiles::load(config.config_dir.as_deref());
    let mut host = Host {
        studio,
        adapters,
        collection,
        collection_epoch: 0,
        list_stamp: None,
        cues_sent: Vec::new(),
        canvas,
        scene,
        rows: Vec::new(),
        heard: Vec::new(),
        globals_shown: Vec::new(),
        into: None,
        held,
        held_was: None,
        held_id_was: None,
        scene_preview_sent: None,
        vertical_picture: false,
        vertical_preview_sent: None,
        projectors: Vec::new(),
        interacts: Vec::new(),
        scenes_shown: None,
        scene_answer: scene_cache::SceneAnswerCache::default(),
        vertical: None,
        vertical_restore_error: None,
        vertical_flags: VerticalFlags::default(),
        separate_files: true,
        limit_policy: (LimitPolicy::Ask, true),
        vertical_shared: Vec::new(),
        vertical_sent: None,
        master_sent: None,
        sizes: BTreeMap::new(),
        attribute_clipboard: None,
        history: History::new(),
        history_shown: (None, None),
        waiting: false,
        held_switch: None,
        ran_program: None,
        pending_progress: None,
        tbar_manual: false,
        tbar_zeroed: false,
        taking: false,
        text_elapsed: text_live::TextElapsed::default(),
        text_timer_file: saved.text_timers,
        text_timers_restored: false,
        crisp_pending: None,
        fonts_wanted: false,
        outs: [CanvasOutputs::default(), CanvasOutputs::default()],
        iso: None,
        stats_due: None,
        clock,
        camera_factory: config.virtual_camera.map(|f| f.0),
        camera: None,
        camera_due: None,
        monitor: monitor::MonitorHost::new(config.monitor, can_monitor),
        loudness_on: config.loudness.shown,
        loudness_settings: config.loudness,
        loudness_sent: None,
        camera_setup: None,
        camera_published: None,
        replay: replay::ReplayHost::new(),
        protection: protection::ProtectionHost::new(config.system_probe),
        keys,
        key_account: config.key_account,
        engine: Box::new(engine),
        gpu,
        browser_tree,
        browser,
        engine_slot,
        profiles_dir: profiles,
        preview_surfaces: settings.preview.map(|p| p.surfaces),
        dead: None,
        restarting: false,
        caught: false,
        unanswered: None,
        #[cfg(feature = "fault-injection")]
        fault: config_fault,
        autosave: config.autosave,
        recent: saved.recent,
        device_sync: saved.device_sync,
        offered: None,
        measure: sync_measure::MeasureHost::default(),
        config_dir: config.config_dir,
        calibration: config.calibration,
        calibration_run: calibration::CalibrationHost::new(config.calibration_setup),
        disk_probe: config
            .read_free_space
            .map(|r| rbs_record::estimate::DiskProbe::new(r.0)),
        swap_after_transition: config.swap_after_transition,
        media_pause_release: config.media_pause_release,
        media_sent: None,
        thumbs: Arc::new(AtomicU64::new(0)),
        permission_asking: HashMap::new(),
        permission_asked: Vec::new(),
        permission_refused: Vec::new(),
        region_grabs: Arc::new(AtomicU64::new(0)),
        // A collection that had to be completed is saved at the first publish.
        saved: loaded,
        events: EventPublisher::new(),
        meters_ui: true,
        meters_applied: true,
        api: None,
        profile: RecordProfile::new(),
        kinds,
        encoders,
        encoders_probed: false,
        cannot_record,
        stream_profile: None,
        vertical_profile: None,
        vertical_stream_profile: None,
        destinations: Destinations::default(),
        destinations_unloaded: false,
        library: None,
        switch_pending: None,
        switch_seq: 0,
        inbox: Weak::new(),
        seams: seams::HostSeams::new(config.chapters, config.os_version),
        stinger: stinger::StingerFile::default(),
        add_ons,
        levels,
    };
    // A vertical canvas saved with the collection runs again (ADR-0093 decision 9).
    host.restore_vertical();
    // Show loudness is saved: the meter runs again from the start.
    host.restore_loudness();
    Ok((host, update))
}

/// Where signed-in browser profiles sit: beside the add-ons, in the local data
/// directory (`start`).
fn profiles_dir(setup: &crate::add_ons::AddOnsSetup) -> Option<PathBuf> {
    crate::browser_profiles::dir_beside(&setup.root)
}

/// The engine add-on slot, resolved now only when the collection has a browser source
/// (ADR-0145 decision 6; `start`).
pub(super) fn engine_slot(
    collection: &SceneCollection,
    add_ons: &add_ons::AddOnsHost,
) -> Arc<EngineSlot> {
    let engine_slot = Arc::new(EngineSlot::default());
    if collection
        .sources()
        .any(|s| s.type_id.as_str() == types::BROWSER_SOURCE)
    {
        engine_slot.set(add_ons.browser_engine());
    }
    engine_slot
}

/// The studio's start settings for `canvas` (`start`).
pub(super) fn studio_settings(
    preview: Option<PreviewDesc>,
    media_pause_release: Duration,
    canvas: CanvasId,
    run_fps: FrameRate,
    collection: &SceneCollection,
) -> StudioSettings {
    let mut settings = StudioSettings::new(canvas);
    // Width and height stay as configured. The ring follows the canvas that runs:
    // a saved collection can use another rate than the one the descriptor was built with.
    settings.preview = preview.map(|preview| PreviewDesc {
        surfaces: preview_surfaces(run_fps),
        ..preview
    });
    settings.scene_preview = SCENE_PICTURE;
    settings.audio_offsets = filters::audio_offsets(collection);
    settings.media_pause_release = media_pause_release;
    settings
}

/// The source kinds whose adapters are installed: capture kinds in their order, then
/// the kinds RBS draws itself (`start`).
pub(super) fn installed_kinds(sources: &rbs_sources::SourceTypes) -> Vec<SourceKind> {
    let installed = sources.installed();
    SourceKind::ALL
        .into_iter()
        .filter(|k| installed.contains(&k.type_id()))
        .chain(
            [
                SourceKind::Image,
                SourceKind::Color,
                SourceKind::Media,
                SourceKind::Browser,
                SourceKind::Text,
            ]
            .into_iter()
            .filter(|k| installed.contains(&k.type_id())),
        )
        .collect()
}

/// The files of the config directory that `start` reads; defaults without one.
#[derive(Default)]
pub(super) struct SavedFiles {
    pub(super) text_timers: Vec<crate::text_timers::Entry>,
    pub(super) recent: RecentTargets,
    pub(super) device_sync: crate::device_sync_file::DeviceSyncFile,
}

impl SavedFiles {
    pub(super) fn load(dir: Option<&Path>) -> Self {
        let Some(d) = dir else {
            return Self::default();
        };
        let (text_timers, note) = crate::text_timers::load(d);
        if let Some(why) = note {
            rbs_core::warn!(rbs_core::log::Area::App, "text timers were not read: {why}");
        }
        Self {
            text_timers,
            recent: crate::recent_targets::load(d).0,
            device_sync: crate::device_sync_file::load(d).0,
        }
    }
}

/// The vertical canvas a saved collection holds: the first canvas besides `main`
/// (a studio runs at most two, and only `canvas.add_vertical` adds the second).
pub(super) fn saved_vertical(
    collection: &SceneCollection,
    main: CanvasId,
) -> Option<VerticalState> {
    collection
        .canvases()
        .find(|c| c.id != main)
        .map(|c| VerticalState {
            canvas: c.id,
            size: c.size,
            follow: c.follow == Some(main),
        })
}
