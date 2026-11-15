//! The output cost estimate (ADR-0087 decisions 1 to 3, spec
//! `docs/specs/output-estimate.md`): levels, unknown limits and "at most N".

#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use std::time::Duration;

use rbs_record::IsoRole;
use rbs_record::estimate::{
    AudioEncode, Calibration, DeclaredAdapter, EncoderMeasure, EstimatePlan, ExportFormat,
    FpsClass, Level, Limits, RateClass, ReasonCode, Resource, Row, SizeClass, StopReason, TrackRef,
    Use, VideoEncode, estimate, separate_video_limit,
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
        format: ExportFormat::Nv12,
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
        stopped_by: StopReason::Late,
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
        // A refused open: a session cap of 3 beats the declared 8.
        encoders: vec![EncoderMeasure {
            stopped_by: StopReason::Refused,
            ..measure(3, 1_000_000_000, Duration::from_millis(2))
        }],
        exports: vec![(
            SizeClass::L1080,
            ExportFormat::Nv12,
            Duration::from_millis(1),
        )],
        write_speed: Some(100_000_000),
        exports_unmeasurable: false,
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
        exports: vec![(
            SizeClass::L1080,
            ExportFormat::Nv12,
            Duration::from_millis(1),
        )],
        ..Calibration::default()
    });
    let e = estimate(&plan(HW, 1), &limits);
    let r = row(&e.resources, Resource::IsoRenderShare);
    assert_eq!((r.limit, r.level), (None, Some(Level::Unknown)));
    // Every export is measured: only the previous tick is missing, which no test
    // measures (step E).
    assert!(!r.calibrate);
    assert!(
        e.reasons
            .iter()
            .any(|x| x.code == ReasonCode::Unknown && x.resource == Some(Resource::IsoRenderShare))
    );
    assert!(!Resource::IsoRenderShare.calibration_measures());
}

#[test]
fn a_missing_export_of_the_tracks_format_advises_a_test_once_it_can_help() {
    let mut p = plan(HW, 1);
    for e in &mut p.video {
        if matches!(e.used_by, Use::Iso(_)) {
            e.format = ExportFormat::P010;
        }
    }
    // Only NV12 measured: the P010 track's export is missing, and a test measures it.
    let mut limits = declared(Some(8));
    limits.calibration = Some(Calibration {
        exports: vec![(
            SizeClass::L1080,
            ExportFormat::Nv12,
            Duration::from_millis(1),
        )],
        ..Calibration::default()
    });
    let e = estimate(&p, &limits);
    let r = row(&e.resources, Resource::IsoRenderShare);
    assert_eq!(r.level, Some(Level::Unknown));
    assert!(r.calibrate);
    assert!(
        e.reasons
            .iter()
            .any(|x| x.code == ReasonCode::Unknown && x.resource == Some(Resource::IsoRenderShare))
    );
    assert!(
        e.tracks
            .iter()
            .all(|t| t.cost.iter().all(|(r, _)| *r != Resource::IsoRenderShare))
    );
    // The last test could not measure exports: no advice.
    limits.calibration = Some(Calibration {
        exports_unmeasurable: true,
        ..Calibration::default()
    });
    let e = estimate(&p, &limits);
    assert!(!row(&e.resources, Resource::IsoRenderShare).calibrate);
    // Known and informational rows never advise; an unknown session count does.
    let e = estimate(&plan(HW, 1), &declared(None));
    assert!(row(&e.resources, Resource::EncoderSessions).calibrate);
    assert!(!row(&e.resources, Resource::IsoCeiling).calibrate);
    assert!(!row(&e.resources, Resource::Memory).calibrate);
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

#[test]
fn the_separate_video_limit_is_three_until_the_encoder_is_known() {
    use rbs_record::estimate::UNKNOWN_SESSIONS_VIDEO_LIMIT;
    // Hardware, nothing declared or measured.
    let e = estimate(&plan(HW, 0), &declared(None));
    assert_eq!(separate_video_limit(&e), UNKNOWN_SESSIONS_VIDEO_LIMIT);
    // Declared: the declared count decides, above or below three.
    let e = estimate(&plan(HW, 0), &declared(Some(8)));
    assert_eq!(separate_video_limit(&e), 7);
    let e = estimate(&plan(HW, 0), &declared(Some(2)));
    assert_eq!(separate_video_limit(&e), 1);
    // Software: unlimited sessions, but its load is not measured.
    let e = estimate(&plan(SW, 0), &declared(None));
    assert_eq!(separate_video_limit(&e), UNKNOWN_SESSIONS_VIDEO_LIMIT);
    // Measured: the measurement decides.
    let mut limits = declared(None);
    limits.calibration = Some(Calibration {
        encoders: vec![measure(6, 10_000_000_000, Duration::from_micros(100))],
        ..Calibration::default()
    });
    let e = estimate(&plan(HW, 0), &limits);
    assert_eq!(separate_video_limit(&e), 5);
}

// ---- The start gate (ADR-0087 decisions 5 and 7, control API 1.54) ---------------

use rbs_record::estimate::{GatePolicy, GateRefusal, GateRequest, LimitCause, gate};

fn ids(range: std::ops::RangeInclusive<u32>) -> Vec<SourceId> {
    range.map(SourceId::from_raw).collect()
}

fn asked(acknowledge_heavy: bool, policy: GatePolicy) -> GateRequest {
    GateRequest::Asked {
        acknowledge_heavy,
        policy,
    }
}

#[test]
fn the_gate_skips_or_refuses_files_past_the_encoder_cap() {
    // Nothing declared, nothing measured: 3 files, the cause is the encoder.
    let p = plan(HW, 5);
    let limits = declared(None);
    let marked = ids(1..=5);
    let older = gate(&p, &limits, &marked, GateRequest::Older);
    assert_eq!(
        (older.keep.clone(), older.limit.clone()),
        (ids(1..=3), ids(4..=5))
    );
    assert_eq!(older.count, 3);
    assert!(older.refusal.is_none());

    let skip = gate(&p, &limits, &marked, asked(false, GatePolicy::Skip));
    assert_eq!((skip.keep, skip.limit), (ids(1..=3), ids(4..=5)));
    assert!(skip.refusal.is_none());

    let ask = gate(&p, &limits, &marked, asked(true, GatePolicy::Ask));
    let (why, _, tracks) = ask.refusal.unwrap();
    assert_eq!(why, GateRefusal::HardwareLimit);
    assert_eq!(
        tracks.iter().map(|t| t.source).collect::<Vec<_>>(),
        ids(4..=5)
    );

    // A declared session limit decides: 3 sessions, the master takes one.
    let declared3 = declared(Some(3));
    let e = estimate(&p, &declared3);
    assert_eq!(
        rbs_record::estimate::separate_video_bound(&p, &e),
        (2, LimitCause::Encoder)
    );
    let older = gate(&p, &declared3, &marked, GateRequest::Older);
    assert_eq!(older.keep, ids(1..=2));
}

#[test]
fn disk_space_never_gates_but_a_slow_disk_trims_older_requests() {
    // A nearly full disk is heavy for the plan, never for the gate.
    let p = plan(HW, 2);
    let mut limits = declared(Some(8));
    limits.free_space = Some(1);
    let marked = ids(1..=2);
    let d = gate(&p, &limits, &marked, asked(false, GatePolicy::Skip));
    assert_eq!(d.estimate.level, Level::Heavy);
    assert_eq!(d.estimate.gate_level, Level::Fits);
    assert!(d.refusal.is_none());
    assert_eq!(d.keep, marked);
    assert_eq!(gate(&p, &limits, &marked, GateRequest::Older).keep, marked);

    // A disk of 4 MB/s: master and audio 1.02 MB/s fit, each file adds 1 MB/s, so
    // two files are heavy (3.02 ≥ 2.8) and one fits.
    let mut limits = declared(Some(8));
    limits.calibration = Some(Calibration {
        write_speed: Some(4_000_000),
        ..Calibration::default()
    });
    let older = gate(&p, &limits, &marked, GateRequest::Older);
    assert_eq!((older.keep, older.limit), (ids(1..=1), ids(2..=2)));
    let (why, judged, _) = gate(&p, &limits, &marked, asked(false, GatePolicy::Skip))
        .refusal
        .unwrap();
    assert_eq!(why, GateRefusal::Heavy);
    assert_eq!(judged.gate_level, Level::Heavy);
    let acknowledged = gate(&p, &limits, &marked, asked(true, GatePolicy::Ask));
    assert!(acknowledged.refusal.is_none());
    assert_eq!(acknowledged.keep, marked);
    // Three files would be over: a resource limit, refused under `ask`.
    let three = plan(HW, 3);
    let (why, _, tracks) = gate(&three, &limits, &ids(1..=3), asked(true, GatePolicy::Ask))
        .refusal
        .unwrap();
    assert_eq!((why, tracks.len()), (GateRefusal::OverLimit, 1));
}

#[test]
fn with_no_encoder_for_the_files_older_requests_skip_them_and_new_ones_are_refused() {
    let mut p = plan(HW, 0);
    p.video_template = None;
    let marked = ids(1..=2);
    let older = gate(&p, &declared(None), &marked, GateRequest::Older);
    assert_eq!(older.no_encoder, marked);
    assert!(older.keep.is_empty() && older.refusal.is_none());
    let asked = gate(&p, &declared(None), &marked, asked(true, GatePolicy::Skip));
    assert_eq!(asked.refusal.unwrap().0, GateRefusal::OverLimit);
}

// ADR-0087 step D1: what a calibration entry's stop reason means
// (`output-estimate.md`, "What a stop reason means").

fn measured(size: SizeClass, pixels: u64, sessions: u32, stopped_by: StopReason) -> EncoderMeasure {
    EncoderMeasure {
        size,
        pixels_per_second: u64::from(sessions) * pixels * 60,
        stopped_by,
        ..measure(sessions, 0, Duration::from_millis(2))
    }
}

fn with_calibration(encoders: Vec<EncoderMeasure>) -> Limits {
    let mut limits = declared(None);
    limits.calibration = Some(Calibration {
        encoders,
        ..Calibration::default()
    });
    limits
}

fn sized(mut e: VideoEncode, width: u32, height: u32) -> VideoEncode {
    e.width = width;
    e.height = height;
    e
}

#[test]
fn mixed_classes_combine_by_share_and_a_late_count_is_not_a_session_cap() {
    // Master 1080p, three 720p files and one 2160p file on one adapter.
    let mut p = plan(HW, 0);
    for i in 0..3 {
        p.video.push(sized(
            video(Use::Iso(track(i + 1, IsoRole::Video)), HW, Some(0)),
            1280,
            720,
        ));
    }
    p.video.push(sized(
        video(Use::Iso(track(4, IsoRole::Video)), HW, Some(1)),
        3840,
        2160,
    ));
    let limits = with_calibration(vec![
        measured(SizeClass::L1080, 1920 * 1080, 12, StopReason::Late),
        measured(SizeClass::L720, 1280 * 720, 20, StopReason::Ceiling),
        measured(SizeClass::L2160, 3840 * 2160, 3, StopReason::Late),
    ]);
    let e = estimate(&p, &limits);
    let s = row(&e.resources, Resource::EncoderSessions);
    assert_eq!((s.level, s.measured), (Some(Level::Fits), true));
    // 1/12 + 3/20 + 1/3 = 0.57 of the adapter.
    let t = row(&e.resources, Resource::EncoderThroughput);
    assert_eq!((t.level, t.measured), (Some(Level::Fits), true));
    let share = t.need / t.limit.unwrap_or(f64::NAN);
    assert!(
        (share - (1.0 / 12.0 + 3.0 / 20.0 + 1.0 / 3.0)).abs() < 1e-9,
        "{share}"
    );
}

#[test]
fn a_late_count_limits_the_files_by_throughput_not_to_three() {
    let limits = with_calibration(vec![measured(
        SizeClass::L1080,
        1920 * 1080,
        12,
        StopReason::Late,
    )]);
    let e = estimate(&plan(HW, 1), &limits);
    assert_eq!(e.max_video.count, Some(11));
    assert_eq!(e.max_video.limit, Some(Resource::EncoderThroughput));
    assert!(!e.max_video.unknown.contains(&Resource::EncoderSessions));
    assert_eq!(separate_video_limit(&e), 11);
}

#[test]
fn a_lower_bound_raises_the_limit_to_what_was_measured() {
    // `iso_render_share` stays unknown (no export measured): it must not hold the
    // known count at 0.
    let limits = with_calibration(vec![measured(
        SizeClass::L1080,
        1920 * 1080,
        9,
        StopReason::Limit,
    )]);
    let e = estimate(&plan(HW, 1), &limits);
    assert!(e.max_video.unknown.contains(&Resource::EncoderSessions));
    assert!(e.max_video.unknown.contains(&Resource::IsoRenderShare));
    assert_eq!(e.max_video.known_count, Some(8));
    assert_eq!(separate_video_limit(&e), 8);
}

#[test]
fn a_small_lower_bound_keeps_the_cap_of_three() {
    let limits = with_calibration(vec![measured(
        SizeClass::L1080,
        1920 * 1080,
        2,
        StopReason::Cancelled,
    )]);
    let e = estimate(&plan(HW, 1), &limits);
    assert_eq!(e.max_video.known_count, Some(1));
    assert_eq!(separate_video_limit(&e), 3);
}

#[test]
fn every_class_at_the_ceiling_has_no_session_cap() {
    let limits = with_calibration(vec![measured(
        SizeClass::L1080,
        1920 * 1080,
        20,
        StopReason::Ceiling,
    )]);
    let e = estimate(&plan(HW, 15), &limits);
    let s = row(&e.resources, Resource::EncoderSessions);
    assert_eq!((s.level, s.measured), (Some(Level::Fits), true));
    assert!(!e.max_video.unknown.contains(&Resource::EncoderSessions));
    assert!(separate_video_limit(&e) > 3);
}

#[test]
fn a_refused_open_is_the_session_cap() {
    let limits = with_calibration(vec![measured(
        SizeClass::L1080,
        1920 * 1080,
        4,
        StopReason::Refused,
    )]);
    let e = estimate(&plan(HW, 4), &limits);
    let s = row(&e.resources, Resource::EncoderSessions);
    assert_eq!(
        (s.limit, s.level, s.measured),
        (Some(4.0), Some(Level::Over), true)
    );
    assert_eq!(e.max_video.count, Some(3));
    assert_eq!(e.max_video.limit, Some(Resource::EncoderSessions));
}

#[test]
fn a_software_adapter_keeps_unlimited_sessions_above_its_measure() {
    let limits = with_calibration(vec![EncoderMeasure {
        adapter: SW.to_owned(),
        ..measured(SizeClass::L1080, 1920 * 1080, 5, StopReason::Late)
    }]);
    let e = estimate(&plan(SW, 8), &limits);
    let s = row(&e.resources, Resource::EncoderSessions);
    assert_eq!(s.level, Some(Level::Fits));
    let c = row(&e.resources, Resource::CpuLoad);
    assert_eq!(c.level, Some(Level::Over));
    assert_eq!(e.max_video.count, Some(4));
    assert_eq!(e.max_video.limit, Some(Resource::CpuLoad));
}

#[test]
fn a_measured_zero_is_over_and_measured() {
    let limits = with_calibration(vec![measured(
        SizeClass::L1080,
        1920 * 1080,
        0,
        StopReason::Late,
    )]);
    let e = estimate(&plan(HW, 0), &limits);
    let t = row(&e.resources, Resource::EncoderThroughput);
    assert_eq!(
        (t.limit, t.level, t.measured),
        (Some(0.0), Some(Level::Over), true)
    );
}

#[test]
fn throughput_above_a_lower_bound_is_unknown_not_over() {
    let limits = with_calibration(vec![measured(
        SizeClass::L1080,
        1920 * 1080,
        3,
        StopReason::Limit,
    )]);
    let e = estimate(&plan(HW, 4), &limits);
    let t = row(&e.resources, Resource::EncoderThroughput);
    assert_eq!((t.limit, t.level), (None, Some(Level::Unknown)));
}
