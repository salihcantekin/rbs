//! The stinger file look (ADR-0140 decisions 2 and 3) on the scripted decoder.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use rbs_media_decode::MediaDecoderFactory;
use rbs_media_decode_test::{ScriptClip, ScriptFactory, ScriptFrame};
use rbs_sources::stinger::{LookError, StingerAlpha, look};

const STEP: Duration = Duration::from_millis(100);

/// Frames at 0, 100, ... ms; the frames from `cover` on are opaque, the others clear.
fn clip(frames: u32, cover: u32) -> ScriptClip {
    let mut c = ScriptClip::silent_frames();
    c.frames = (0..frames)
        .map(|i| {
            let mut f = ScriptFrame::rgba(16, 9, STEP * i, STEP, 1);
            if i < cover {
                for px in f.bytes.chunks_exact_mut(4) {
                    px[3] = 0;
                }
            }
            f
        })
        .collect();
    c.duration = Some(STEP * frames);
    c
}

fn run(
    factories: Vec<Arc<dyn MediaDecoderFactory>>,
    range: (Duration, Option<Duration>),
) -> Result<rbs_sources::stinger::StingerLook, LookError> {
    look(
        &factories,
        Path::new("/sting.mov"),
        range,
        (1920, 1080),
        &AtomicBool::new(false),
    )
}

#[test]
fn the_cut_is_the_first_covering_picture_from_the_range_start() {
    let keeps: Arc<dyn MediaDecoderFactory> =
        Arc::new(ScriptFactory::new(clip(10, 4)).with_alpha(true));
    let found = run(vec![Arc::clone(&keeps)], (Duration::ZERO, None)).unwrap();
    assert_eq!(found.alpha, StingerAlpha::Kept);
    assert_eq!(found.cut, Some(STEP * 4));
    assert_eq!(found.duration, Some(STEP * 10));
    // Measured from the start of the play range.
    let later = run(vec![keeps], (STEP * 2, None)).unwrap();
    assert_eq!(later.cut, Some(STEP * 2));
}

#[test]
fn no_cut_without_alpha_or_without_a_covering_picture() {
    let opaque: Arc<dyn MediaDecoderFactory> = Arc::new(ScriptFactory::new(clip(10, 4)));
    let plain = run(vec![opaque], (Duration::ZERO, None)).unwrap();
    assert_eq!(plain.alpha, StingerAlpha::None);
    assert_eq!(plain.cut, None);
    let never: Arc<dyn MediaDecoderFactory> =
        Arc::new(ScriptFactory::new(clip(10, 10)).with_alpha(true));
    assert_eq!(run(vec![never], (Duration::ZERO, None)).unwrap().cut, None);
    // A play range that ends before the covering picture finds none.
    let short: Arc<dyn MediaDecoderFactory> =
        Arc::new(ScriptFactory::new(clip(10, 4)).with_alpha(true));
    assert_eq!(
        run(vec![short], (Duration::ZERO, Some(STEP * 3)))
            .unwrap()
            .cut,
        None
    );
}

#[test]
fn an_adapter_that_drops_alpha_is_passed_over_for_one_that_keeps_it() {
    let drops: Arc<dyn MediaDecoderFactory> =
        Arc::new(ScriptFactory::new(clip(10, 4)).with_alpha(false));
    let keeps: Arc<dyn MediaDecoderFactory> =
        Arc::new(ScriptFactory::new(clip(10, 4)).with_alpha(true));
    let found = run(vec![Arc::clone(&drops), keeps], (Duration::ZERO, None)).unwrap();
    assert_eq!(found.alpha, StingerAlpha::Kept);
    assert_eq!(found.cut, Some(STEP * 4));
    // Alone, it reports that the alpha cannot be kept here.
    let alone = run(vec![drops], (Duration::ZERO, None)).unwrap();
    assert_eq!(alone.alpha, StingerAlpha::Unsupported);
    assert_eq!(alone.duration, Some(STEP * 10));
}

#[test]
fn a_cancelled_or_unreadable_look_finds_nothing() {
    let keeps: Arc<dyn MediaDecoderFactory> =
        Arc::new(ScriptFactory::new(clip(10, 4)).with_alpha(true));
    let cancelled = look(
        &[keeps],
        Path::new("/sting.mov"),
        (Duration::ZERO, None),
        (1920, 1080),
        &AtomicBool::new(true),
    );
    assert_eq!(cancelled, Err(LookError::Cancelled));
    let mut broken = clip(1, 0);
    broken.fail_open = Some("broken".into());
    let failed: Arc<dyn MediaDecoderFactory> = Arc::new(ScriptFactory::new(broken));
    assert_eq!(
        run(vec![failed], (Duration::ZERO, None)),
        Err(LookError::Failed("broken".into()))
    );
}

#[test]
fn alpha_here_answers_from_opens_only() {
    use rbs_sources::stinger::alpha_here;
    let path = Path::new("/sting.mov");
    let keeps: Arc<dyn MediaDecoderFactory> =
        Arc::new(ScriptFactory::new(clip(10, 4)).with_alpha(true));
    let drops: Arc<dyn MediaDecoderFactory> =
        Arc::new(ScriptFactory::new(clip(10, 4)).with_alpha(false));
    let plain: Arc<dyn MediaDecoderFactory> = Arc::new(ScriptFactory::new(clip(10, 4)));
    assert_eq!(
        alpha_here(&[Arc::clone(&drops)], path),
        Some(StingerAlpha::Unsupported)
    );
    assert_eq!(alpha_here(&[drops, keeps], path), Some(StingerAlpha::Kept));
    assert_eq!(alpha_here(&[plain], path), Some(StingerAlpha::None));
    assert_eq!(alpha_here(&[], path), None);
}
