//! Separate video files (ADR-0086 step 3.2, ADR-0119): one file per source marked for
//! video, from the source's own picture (or its chain tail when filters are asked
//! for), each encoded on a partition of its own and written on the ISO output
//! partition. Reference adapters on a `TestClock`, as in
//! `studio.rs`. Waits are for packet counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_core::media::{CodecId, ContainerId};
use rbs_graph::node::NodeTypeId;
use rbs_record::{
    ISO_OUTPUT, ISO_VIDEO_ENCODERS, IsoError, IsoRole, IsoSkip, IsoVideoStart, OutputSettings,
    PlannedOutput, RecordError, StartPlanRequest, Studio, StudioSettings, VideoSettings,
};
use rbs_scene::model::{ItemContent, Layout, RecordSeparately, SettingValue, Settings, VideoAs};
use rbs_sources::types::{self, keys};
use support::*;

fn h264(log: &Log) -> usize {
    log.packets
        .lock()
        .unwrap()
        .iter()
        .filter(|packet| packet.codec == CodecId::H264)
        .count()
}

fn start_request() -> IsoVideoStart {
    IsoVideoStart::new("mem://iso", ContainerId::Matroska, VideoSettings::default())
}

/// A folder of its own under the system temp folder, removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rbs-record-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The PNG files in `dir`, sorted.
fn pngs(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png"))
        .collect();
    names.sort();
    names
}

#[test]
fn separate_video_files_follow_each_marked_source_and_are_not_outputs() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    // Nothing marked: nothing is added and no worker starts.
    let idle = studio
        .start_iso_video(&r.adapters, &start_request())
        .unwrap();
    assert!(idle.tracks.is_empty());
    assert!(!studio.iso_video_running());
    for part in ISO_VIDEO_ENCODERS {
        assert!(!studio.engine().has_worker(part));
    }
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 0);

    // The master runs first: the sources draw only while something takes the canvas.
    let master = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    let master_sessions = r.video_sessions.load(Ordering::Relaxed);

    // A still picture: an image track (ADR-0117), no encoder.
    let scene = col.scenes().next().unwrap().id;
    let settings: Settings = [
        (
            keys::COLOR.to_owned(),
            SettingValue::Text("#30a050".to_owned()),
        ),
        (keys::WIDTH.to_owned(), SettingValue::Int(32)),
        (keys::HEIGHT.to_owned(), SettingValue::Int(18)),
    ]
    .into_iter()
    .collect();
    let (card, _) = col
        .add_source(
            "Card",
            NodeTypeId::new_static(types::COLOR_SOURCE),
            settings,
        )
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(card),
        Layout::new([0.0, 0.0], [8.0, 4.5]),
        None,
    )
    .unwrap();
    for name in ["Display", "Camera", "Card"] {
        let id = col.sources().find(|s| s.name == name).unwrap().id;
        col.set_source_record_separately(id, RecordSeparately::new(true, false))
            .unwrap();
    }
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    // A file takes its size from the source's picture, known once capture opened.
    let marked: Vec<_> = col
        .sources()
        .filter(|s| s.record_separately.video)
        .map(|s| s.id)
        .collect();
    let reached = run_until(&mut studio, &clock, |s| {
        marked.iter().all(|id| s.picture_size(*id).is_some())
    });
    let sizes: Vec<_> = marked.iter().map(|id| studio.picture_size(*id)).collect();
    assert!(reached, "{sizes:?} {:?}", studio.progress());
    // A still source needs a folder for its images.
    let err = studio
        .start_iso_video(&r.adapters, &start_request())
        .unwrap_err();
    assert!(
        matches!(err, RecordError::Iso(IsoError::ImageFolder)),
        "{err:?}"
    );
    let images = TempDir::new("iso-images");
    let mut request = start_request();
    request.image_dir = Some(images.0.clone());
    let started = studio.start_iso_video(&r.adapters, &request).unwrap();
    assert_eq!(started.tracks.len(), 2);
    assert_eq!(started.images.len(), 1);
    assert_eq!(started.images[0].file_name, "Card.png");
    assert_eq!(started.images[0].size, (32, 18));
    for track in &started.tracks {
        assert!(track.file_name.ends_with(".mkv"), "{}", track.file_name);
        assert_eq!(track.size.0 % 2, 0);
        assert_eq!(track.size.1 % 2, 0);
    }
    assert!(studio.iso_video_running());
    assert_eq!(studio.outputs(), [master], "the files are not outputs");
    assert_eq!(
        r.video_sessions.load(Ordering::Relaxed),
        master_sessions * 3,
        "each video file opens its encoder as the master did; an image track none"
    );
    let err = studio.start_iso_video(&r.adapters, &request).unwrap_err();
    assert!(matches!(err, RecordError::IsoRunning));

    let reached = run_until(&mut studio, &clock, |_| {
        started
            .tracks
            .iter()
            .all(|t| t.stats.frames.load(Ordering::Relaxed) >= 10)
            && r.log.headers.load(Ordering::Relaxed) >= 3
            && started.images[0].files.entries().len() == 1
    });
    assert!(
        reached,
        "h264 {} headers {}",
        h264(&r.log),
        r.log.headers.load(Ordering::Relaxed)
    );
    // The still picture did not change: one image, taken on the first tick.
    let image = &started.images[0];
    assert_eq!(image.stats.taken.load(Ordering::Relaxed), 1);
    let entries = image.files.entries();
    assert_eq!(entries[0].file_name, "Card.png");
    assert_eq!((entries[0].width, entries[0].height), (32, 18));
    assert_eq!(pngs(&images.0), ["Card.png"]);
    // Files on the master's engine take turns on two encoder partitions, and none beyond.
    for (i, part) in ISO_VIDEO_ENCODERS.into_iter().enumerate() {
        assert_eq!(studio.engine().has_worker(part), i < 2, "{part:?}");
    }
    assert!(studio.engine().has_worker(ISO_OUTPUT));

    for track in &started.tracks {
        assert_eq!(track.stats.lost.load(Ordering::Relaxed), 0, "{track:?}");
        assert!(track.queue.is_some(), "{track:?}");
    }

    assert!(studio.stop_iso_video());
    assert!(!studio.iso_video_running());
    assert!(!studio.stop_iso_video());
    for part in ISO_VIDEO_ENCODERS {
        assert!(!studio.engine().has_worker(part), "{part:?}");
    }
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    assert!(r.log.trailers.load(Ordering::Relaxed) >= 2);
    let report = studio.stop_output(master).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_marked_source_with_no_picture_yet_adds_nothing() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let display = col.sources().find(|s| s.name == "Display").unwrap().id;
    col.set_source_record_separately(display, RecordSeparately::new(true, false))
        .unwrap();
    let (engine, _clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    // No tick ran, so the capture has not reported a picture.
    let err = studio
        .start_iso_video(&r.adapters, &start_request())
        .unwrap_err();
    assert!(
        matches!(err, RecordError::Iso(IsoError::NoPicture { source, .. }) if source == display),
        "{err:?}"
    );
    assert!(!studio.iso_video_running());
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 0);
    for part in ISO_VIDEO_ENCODERS {
        assert!(!studio.engine().has_worker(part));
    }
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn files_past_the_limit_and_sources_without_a_picture_are_skipped_when_asked() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    // Not on any scene: never drawn, so it has no picture.
    let (spare, _) = col
        .add_source(
            "Spare",
            NodeTypeId::new_static(types::COLOR_SOURCE),
            Settings::new(),
        )
        .unwrap();
    col.set_source_record_separately(
        spare,
        RecordSeparately::new(true, false).with_video_as(VideoAs::Video),
    )
    .unwrap();
    let mut marked = Vec::new();
    for name in ["Display", "Camera"] {
        let id = col.sources().find(|s| s.name == name).unwrap().id;
        col.set_source_record_separately(id, RecordSeparately::new(true, false))
            .unwrap();
        marked.push(id);
    }
    marked.sort();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let master = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    let reached = run_until(&mut studio, &clock, |s| {
        marked.iter().all(|id| s.picture_size(*id).is_some())
    });
    assert!(reached, "{:?}", studio.progress());

    // The estimate's plan (ADR-0087): the master's encoder and three marked pictures,
    // all on the one test encoder, which declares no session limit.
    let (plan, declared) = studio.separate_video_plan(&r.adapters, &col, &start_request());
    let count = |iso: bool| {
        plan.video
            .iter()
            .filter(|e| matches!(e.used_by, rbs_record::estimate::Use::Iso(_)) == iso)
            .count()
    };
    assert_eq!((count(false), count(true)), (1, 3), "{:#?}", plan.video);
    assert!(plan.video_template.is_some());
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0].sessions_max, None);

    let mut request = start_request();
    request.max_video = 1;
    let err = studio.start_iso_video(&r.adapters, &request).unwrap_err();
    assert!(
        matches!(err, RecordError::Iso(IsoError::NoPicture { source, .. }) if source == spare),
        "{err:?}"
    );
    assert!(!studio.iso_video_running());

    request.skip_no_picture = true;
    let started = studio.start_iso_video(&r.adapters, &request).unwrap();
    assert_eq!(started.tracks.len(), 1);
    assert_eq!(started.tracks[0].source, marked[0]);
    let mut expected = vec![
        (marked[1], IsoRole::Video, IsoSkip::Limit),
        (spare, IsoRole::Video, IsoSkip::NoPicture),
    ];
    expected.sort_by_key(|e| e.0);
    let skipped: Vec<_> = started
        .skipped
        .iter()
        .map(|s| (s.source, s.role, s.reason))
        .collect();
    assert_eq!(skipped, expected);

    // The plan of a whole start (ADR-0087 step C2): what runs counts, the running
    // separate file included; an added recording with the master's settings shares
    // its encoder; one that replaces the running recording leaves its encoder and
    // files out and opens its own.
    let split = |plan: &rbs_record::estimate::EstimatePlan| {
        let iso = plan
            .video
            .iter()
            .filter(|e| matches!(e.used_by, rbs_record::estimate::Use::Iso(_)))
            .count();
        (plan.video.len() - iso, iso)
    };
    let mut whole = StartPlanRequest::default();
    let (plan, _) = studio.start_plan(&r.adapters, &col, &whole);
    assert_eq!(split(&plan), (1, 1), "{:#?}", plan.video);
    assert!(plan.video_template.is_none());
    let same = OutputSettings::new("mem://again", ContainerId::Matroska);
    whole.outputs = vec![PlannedOutput::new(&same, None)];
    let (plan, _) = studio.start_plan(&r.adapters, &col, &whole);
    assert_eq!(split(&plan), (1, 1), "{:#?}", plan.video);
    let mut other = OutputSettings::new("mem://other", ContainerId::Matroska);
    other.size = Some((640, 360));
    whole.outputs = vec![PlannedOutput::new(&other, None)];
    let (plan, _) = studio.start_plan(&r.adapters, &col, &whole);
    assert_eq!(split(&plan), (2, 1), "{:#?}", plan.video);
    whole.outputs = vec![PlannedOutput::new(&same, None)];
    whole.replacing = vec![master];
    whole.replacing_separate = true;
    let (plan, _) = studio.start_plan(&r.adapters, &col, &whole);
    assert_eq!(split(&plan), (1, 0), "{:#?}", plan.video);

    assert!(studio.stop_iso_video());
    studio.stop_output(master).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_filtered_source_is_recorded_without_its_filters_unless_asked() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let display = col.sources().find(|s| s.name == "Display").unwrap().id;
    col.add_filter(
        display,
        NodeTypeId::new_static(rbs_filters::catalogue::BLUR),
        Settings::new(),
    )
    .unwrap();
    col.set_source_record_separately(display, RecordSeparately::new(true, false))
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let master = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| s
        .picture_size(display)
        .is_some()));

    // The default reads the source's own picture; `include_filters` reads its chain
    // tail. Both give a file at the canvas frame rate.
    for include_filters in [false, true] {
        let mut request = start_request();
        request.include_filters = include_filters;
        let started = studio.start_iso_video(&r.adapters, &request).unwrap();
        assert_eq!(started.tracks.len(), 1);
        let track = &started.tracks[0];
        let reached = run_until(&mut studio, &clock, |_| {
            track.stats.frames.load(Ordering::Relaxed) >= 5
        });
        assert!(reached, "include_filters {include_filters}: {track:?}");
        assert_eq!(track.stats.lost.load(Ordering::Relaxed), 0, "{track:?}");
        assert!(studio.stop_iso_video());
    }

    let report = studio.stop_output(master).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// The centre pixel of a PNG file, decoded.
fn png_centre(path: &std::path::Path) -> [u8; 4] {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgba);
    let (w, h) = (info.width as usize, info.height as usize);
    let at = ((h / 2) * w + w / 2) * 4;
    [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]
}

fn colour(hex: &str) -> Settings {
    [
        (keys::COLOR.to_owned(), SettingValue::Text(hex.to_owned())),
        (keys::WIDTH.to_owned(), SettingValue::Int(32)),
        (keys::HEIGHT.to_owned(), SettingValue::Int(18)),
    ]
    .into_iter()
    .collect()
}

#[test]
fn a_still_source_writes_a_new_image_when_its_picture_changes() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let scene = col.scenes().next().unwrap().id;
    let (card, _) = col
        .add_source(
            "Card",
            NodeTypeId::new_static(types::COLOR_SOURCE),
            colour("#30a050"),
        )
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(card),
        Layout::new([0.0, 0.0], [8.0, 4.5]),
        None,
    )
    .unwrap();
    col.set_source_record_separately(card, RecordSeparately::new(true, false))
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let master = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| s
        .picture_size(card)
        .is_some()));
    let sessions = r.video_sessions.load(Ordering::Relaxed);

    let images = TempDir::new("iso-still");
    let mut request = start_request();
    request.image_dir = Some(images.0.clone());
    let started = studio.start_iso_video(&r.adapters, &request).unwrap();
    assert!(started.tracks.is_empty());
    assert_eq!(started.images.len(), 1);
    // No encoder and no encoder partition for an image track.
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), sessions);
    for part in ISO_VIDEO_ENCODERS {
        assert!(!studio.engine().has_worker(part), "{part:?}");
    }
    let track = started.images[0].clone();
    assert!(run_until(&mut studio, &clock, move |_| track
        .files
        .entries()
        .len()
        == 1));
    assert_eq!(
        png_centre(&images.0.join("Card.png")),
        [0x30, 0xa0, 0x50, 255]
    );

    // A new colour: a new image, numbered on.
    col.set_source_settings(card, colour("#ff0000")).unwrap();
    assert!(
        studio
            .update(&col, &r.adapters.sources)
            .unwrap()
            .failed
            .is_empty()
    );
    let track = started.images[0].clone();
    assert!(run_until(&mut studio, &clock, move |_| track
        .files
        .entries()
        .len()
        == 2));
    assert_eq!(png_centre(&images.0.join("Card-2.png")), [255, 0, 0, 255]);

    // Two changes before the next tick: the image written shows the picture as it is
    // when taken (the newer one).
    col.set_source_settings(card, colour("#0000ff")).unwrap();
    assert!(
        studio
            .update(&col, &r.adapters.sources)
            .unwrap()
            .failed
            .is_empty()
    );
    col.set_source_settings(card, colour("#00ff00")).unwrap();
    assert!(
        studio
            .update(&col, &r.adapters.sources)
            .unwrap()
            .failed
            .is_empty()
    );
    let track = started.images[0].clone();
    assert!(run_until(&mut studio, &clock, move |_| track
        .files
        .entries()
        .len()
        >= 3));
    let entries = started.images[0].files.entries();
    let last = entries.last().unwrap();
    assert_eq!(
        png_centre(&images.0.join(&last.file_name)),
        [0, 255, 0, 255],
        "{entries:?}"
    );
    assert!(
        entries.windows(2).all(|w| w[1].at_ns > w[0].at_ns),
        "{entries:?}"
    );
    let stats = &started.images[0].stats;
    assert_eq!(stats.failed.load(Ordering::Relaxed), 0);
    assert_eq!(started.images[0].files.failed.load(Ordering::Relaxed), 0);

    assert!(studio.stop_iso_video());
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    let report = studio.stop_output(master).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
