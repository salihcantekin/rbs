//! The output cost estimate (ADR-0087 decisions 1 to 3, spec
//! `docs/specs/output-estimate.md`): levels, unknown limits and "at most N".

use std::time::Duration;

use rbs_record::IsoRole;
use rbs_record::estimate::{
    AudioEncode, Calibration, DeclaredAdapter, EncoderMeasure, EstimatePlan, FpsClass, Level,
    Limits, RateClass, ReasonCode, Resource, Row, SizeClass, TrackRef, Use, VideoEncode, estimate,
};
use rbs_scene::model::SourceId;

const HW: &str = "hw";
const SW: &str = "sw";

fn track(id: u32, role: IsoRole) -> TrackRef {
    TrackRef {
        source: SourceId::from_raw(id),
        role,
    }
}

fn video(used_by: Use, adapter: &str, worker: Option<usize>) -> VideoEncode {
    VideoEncode {
        used_by,
        adapter: adapter.to_owned(),
        hardware: adapter == HW,
        codec: "h264".to_owned(),
        width: 1920,
        height: 1080,
        fps: 60.0,
        rate: RateClass::Cbr,
        bytes_per_second: 1_000_000,
        falls_to_cpu: false,
        worker,
        export_buffers: 3,
    }
}

/// The master on `adapter` and `iso` separate video tracks of the same shape.
fn plan(adapter: &str, iso: u32) -> EstimatePlan {
    let mut v = vec![video(Use::Recording, adapter, None)];
    for i in 0..iso {
        v.push(video(
            Use::Iso(track(i + 1, IsoRole::Video)),
            adapter,
            Some(if i % 2 == 0 { 0 } else { 1 }),
        ));
    }
    EstimatePlan {
        video: v,
        audio: vec![AudioEncode {
            used_by: Use::Recording,
            bytes_per_second: 20_000,
        }],
        images: Vec::new(),
        active_sources: iso,
        inactive: Vec::new(),
        video_template: Some(video(Use::Iso(track(100, IsoRole::Video)), adapter, None)),
        audio_template: Some(AudioEncode {
            used_by: Use::Iso(track(100, IsoRole::Audio)),
            bytes_per_second: 300_000,
        }),
        iso_workers: 2,
        tick: Duration::from_micros(16_667),
        give_way_share: 0.75,
        planned_duration: Duration::from_secs(3600),
    }
}

fn declared(sessions: Option<u32>) -> Limits {
    Limits {
        adapters: vec![
            DeclaredAdapter {
                id: HW.to_owned(),
                hardware: true,
                sessions_max: sessions,
                pixel_rate_max: None,
            },
            DeclaredAdapter {
                id: SW.to_owned(),
                hardware: false,
                sessions_max: None,
                pixel_rate_max: None,
            },
        ],
        ..Limits::default()
    }
}

fn row(rows: &[Row], resource: Resource) -> &Row {
    rows.iter()
        .find(|r| r.resource == resource)
        .unwrap_or_else(|| panic!("no {} row", resource.as_str()))
}

fn measure(sessions: u32, pixels_per_second: u64, frame_time: Duration) -> EncoderMeasure {
    EncoderMeasure {
        adapter: HW.to_owned(),
        codec: "h264".to_owned(),
        size: SizeClass::L1080,
        fps: FpsClass::F60,
        rate: RateClass::Cbr,
        sessions,
        pixels_per_second,
        frame_time,
    }
}

#[test]
fn classes_take_the_smallest_class_at_or_above() {
    assert_eq!(SizeClass::of(720), SizeClass::L720);
    assert_eq!(SizeClass::of(721), SizeClass::L1080);
    assert_eq!(SizeClass::of(1080), SizeClass::L1080);
    assert_eq!(SizeClass::of(2160), SizeClass::L2160);
    assert_eq!(SizeClass::of(2161), SizeClass::Above2160);
    assert_eq!(FpsClass::of(30.0), FpsClass::F30);
    assert_eq!(FpsClass::of(59.94), FpsClass::F60);
    assert_eq!(FpsClass::of(60.0), FpsClass::F60);
    assert_eq!(FpsClass::of(120.0), FpsClass::Above60);
}

#[test]
fn sessions_are_a_count_and_using_all_of_them_fits() {
    let e = estimate(&plan(HW, 3), &declared(Some(4)));
    let s = row(&e.resources, Resource::EncoderSessions);
    assert_eq!((s.need, s.limit), (4.0, Some(4.0)));
    assert_eq!(s.level, Some(Level::Fits));
    assert!(!s.measured);

    let e = estimate(&plan(HW, 4), &declared(Some(4)));
    assert_eq!(
        row(&e.resources, Resource::EncoderSessions).level,
        Some(Level::Over)
    );
    assert_eq!(e.level, Level::Over);
    assert!(
        e.reasons.iter().any(
            |r| r.code == ReasonCode::Sessions && r.resource == Some(Resource::EncoderSessions)
        )
    );
}

#[test]
fn an_absent_session_limit_is_unknown_on_hardware_and_unlimited_in_software() {
    let e = estimate(&plan(HW, 2), &declared(None));
    let s = row(&e.resources, Resource::EncoderSessions);
    assert_eq!((s.limit, s.level), (None, Some(Level::Unknown)));
    assert!(e.unknown.contains(&Resource::EncoderSessions));
    assert!(
        e.reasons
            .iter()
            .any(|r| r.code == ReasonCode::SessionsUnknown)
    );
    // Unknown never makes the plan `over`.
    assert_eq!(e.level, Level::Fits);

    let e = estimate(&plan(SW, 2), &declared(None));
    let s = row(&e.resources, Resource::EncoderSessions);
    assert_eq!((s.limit, s.level), (None, Some(Level::Fits)));
    assert!(!e.unknown.contains(&Resource::EncoderSessions));
}

#[test]
fn rate_levels_turn_heavy_at_seven_tenths_and_over_past_the_limit() {
    let mut limits = declared(Some(16));
    let p = plan(HW, 1); // two 1080p60 sessions: 248_832_000 pixels per second
    let need = 2.0 * 1920.0 * 1080.0 * 60.0;
    for (limit, level) in [
        (need / 0.69, Level::Fits),
        (need / 0.7, Level::Heavy),
        (need, Level::Heavy),
        (need - 1.0, Level::Over),
    ] {
        limits.adapters[0].pixel_rate_max = Some(limit as u64);
        let e = estimate(&p, &limits);
        let t = row(&e.resources, Resource::EncoderThroughput);
        assert_eq!(t.level, Some(level), "limit {limit}");
        assert!(!t.measured);
    }
}

#[test]
fn a_calibration_overrides_the_declared_limits_and_says_measured() {
    let mut limits = declared(Some(8));
    limits.calibration = Some(Calibration {
        encoders: vec![measure(3, 1_000_000_000, Duration::from_millis(2))],
        exports: vec![(SizeClass::L1080, Duration::from_millis(1))],
        write_speed: Some(100_000_000),
    });
    limits.previous_tick = Some(Duration::from_millis(5));
    let e = estimate(&plan(HW, 3), &limits);
    let s = row(&e.resources, Resource::EncoderSessions);
    assert_eq!(
        (s.limit, s.level, s.measured),
        (Some(3.0), Some(Level::Over), true)
    );
    let t = row(&e.resources, Resource::EncoderThroughput);
    assert!(t.measured);
    assert_eq!(t.limit, Some(1_000_000_000.0));
    // Two workers, tracks 1 and 3 on worker 0: 2 × 2 ms × 60 = 0.24 of a second.
    let w = row(&e.resources, Resource::IsoWorkers);
    assert!((w.need - 0.24).abs() < 1e-9, "{}", w.need);
    assert_eq!(w.level, Some(Level::Fits));
    // (5 ms + 3 × 1 ms) / 16.667 ms against the 0.75 give-way share.
    let r = row(&e.resources, Resource::IsoRenderShare);
    assert!((r.need - 8.0 / 16.667).abs() < 1e-3, "{}", r.need);
    assert_eq!((r.limit, r.level), (Some(0.75), Some(Level::Fits)));
    assert!(row(&e.resources, Resource::DiskSpeed).measured);
    // Count limit 3 measured: the master and two separate tracks.
    assert_eq!(e.max_video.count, Some(2));
    assert_eq!(e.max_video.limit, Some(Resource::EncoderSessions));
    assert!(!e.max_video.partial);
}

#[test]
fn the_render_share_is_unknown_until_the_previous_tick_time_is_published() {
    let mut limits = declared(Some(8));
    limits.calibration = Some(Calibration {
        exports: vec![(SizeClass::L1080, Duration::from_millis(1))],
        ..Calibration::default()
    });
    let e = estimate(&plan(HW, 1), &limits);
    let r = row(&e.resources, Resource::IsoRenderShare);
    assert_eq!((r.limit, r.level), (None, Some(Level::Unknown)));
    assert!(
        e.reasons
            .iter()
            .any(|x| x.code == ReasonCode::Unknown && x.resource == Some(Resource::IsoRenderShare))
    );
    assert!(!Resource::IsoRenderShare.calibration_measures());
}

#[test]
fn disk_space_is_never_over_and_unknown_without_a_probe() {
    let mut limits = declared(Some(8));
    let e = estimate(&plan(HW, 0), &limits);
    let d = row(&e.resources, Resource::DiskSpace);
    assert_eq!(d.level, Some(Level::Unknown));
    assert!(
        e.reasons
            .iter()
            .any(|r| r.code == ReasonCode::DiskUnknown && r.resource == Some(Resource::DiskSpace))
    );

    limits.free_space = Some(1);
    let e = estimate(&plan(HW, 0), &limits);
    let d = row(&e.resources, Resource::DiskSpace);
    assert_eq!(d.level, Some(Level::Heavy));
    assert_eq!(d.need, 1_020_000.0 * 3600.0);
    assert!(e.reasons.iter().any(|r| r.code == ReasonCode::DiskFull));
    assert_eq!(e.level, Level::Heavy);
}

#[test]
fn a_fall_to_a_cpu_encoder_makes_the_plan_heavy() {
    let mut p = plan(HW, 1);
    p.video[1].falls_to_cpu = true;
    let mut limits = declared(Some(8));
    limits.free_space = Some(u64::MAX);
    let e = estimate(&p, &limits);
    assert_eq!(e.level, Level::Heavy);
    assert!(e.reasons.iter().any(
        |r| r.code == ReasonCode::SoftwareFallback && r.track == Some(track(1, IsoRole::Video))
    ));
}

#[test]
fn a_source_that_cannot_be_kept_active_is_a_hard_limit() {
    let mut p = plan(HW, 1);
    p.inactive.push(track(1, IsoRole::Video));
    let e = estimate(&p, &declared(Some(8)));
    assert_eq!(e.level, Level::Over);
    assert!(
        e.reasons
            .iter()
            .any(|r| r.code == ReasonCode::SourceInactive)
    );
}

#[test]
fn at_most_n_is_partial_when_a_growing_resource_is_unknown() {
    // Declared sessions 4, nothing measured: the count is an upper bound.
    let e = estimate(&plan(HW, 0), &declared(Some(4)));
    assert_eq!(e.max_video.count, Some(3));
    assert_eq!(e.max_video.limit, Some(Resource::EncoderSessions));
    assert!(e.max_video.partial);
    assert!(e.max_video.unknown.contains(&Resource::EncoderThroughput));
    assert!(e.max_video.unknown.contains(&Resource::DiskSpeed));
}

#[test]
fn at_most_n_is_null_when_nothing_that_grows_is_known() {
    let e = estimate(&plan(HW, 1), &declared(None));
    assert_eq!(e.max_video.count, None);
    assert_eq!(e.max_video.limit, None);
    assert!(e.max_video.partial);
    assert!(e.max_video.unknown.contains(&Resource::EncoderSessions));
}

#[test]
fn at_most_n_stops_at_the_ceiling_and_counts_audio_against_the_disk() {
    // Software, unlimited sessions, measured disk: the video ceiling sets the count.
    let mut limits = declared(None);
    limits.calibration = Some(Calibration {
        write_speed: Some(10_000_000_000),
        ..Calibration::default()
    });
    let e = estimate(&plan(SW, 0), &limits);
    assert_eq!(e.max_video.count, Some(16));
    assert_eq!(e.max_video.limit, Some(Resource::IsoCeiling));
    assert!(e.max_video.partial, "CPU load is unknown");

    // 1.02 MB/s for the master, 0.3 MB/s per audio track, 3 MB/s disk: 6 tracks.
    limits.calibration = Some(Calibration {
        write_speed: Some(3_000_000),
        ..Calibration::default()
    });
    let e = estimate(&plan(SW, 0), &limits);
    assert_eq!(e.max_audio.count, Some(6));
    assert_eq!(e.max_audio.limit, Some(Resource::DiskSpeed));
    assert!(!e.max_audio.partial);
}

#[test]
fn every_separate_track_lists_what_it_adds() {
    let mut p = plan(HW, 1);
    p.audio.push(AudioEncode {
        used_by: Use::Iso(track(1, IsoRole::Audio)),
        bytes_per_second: 300_000,
    });
    let e = estimate(&p, &declared(Some(8)));
    assert_eq!(e.tracks.len(), 2);
    let v = &e.tracks[0];
    assert_eq!(v.track, track(1, IsoRole::Video));
    assert!(v.cost.contains(&(Resource::EncoderSessions, 1.0)));
    assert!(
        v.cost
            .contains(&(Resource::Memory, 1920.0 * 1080.0 * 4.0 * 3.0))
    );
    assert_eq!(e.tracks[1].cost, vec![(Resource::DiskSpeed, 300_000.0)]);
    let m = row(&e.resources, Resource::Memory);
    assert_eq!(m.level, None);
    assert_eq!(m.limit, None);
}
