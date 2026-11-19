//! Conformance of the animated-image decoder against small fixtures (ADR-0108).
//!
//! Every fixture is 8x8 with three frames: all red (100 ms), red with a green
//! top-left 4x4 (200 ms), all blue (stated 0 ms, read as 100 ms by decision 4).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use rbs_media_anim::{AnimDecoder, AnimFactory};
use rbs_media_decode::{MediaDecoder, MediaDecoderFactory, PictureData, PictureLayout};

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The pixel at (x, y), snapped to 0 or 255 per channel: lossy WebP frames come
/// back as 254 or 1, and the fixtures use only full channels.
fn pixel(bytes: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * 8 + x) * 4;
    let snap = |v: u8| {
        assert!(!(3..=252).contains(&v), "channel {v} is not near 0 or 255");
        if v > 127 { 255 } else { 0 }
    };
    [
        snap(bytes[i]),
        snap(bytes[i + 1]),
        snap(bytes[i + 2]),
        snap(bytes[i + 3]),
    ]
}

/// (pts, frame duration, pixel at (0,0), pixel at (7,7)) of one frame.
type Frame = (Duration, Option<Duration>, [u8; 4], [u8; 4]);

fn read_all(dec: &mut AnimDecoder) -> Vec<Frame> {
    let mut out = Vec::new();
    while let Some(sample) = dec.next_picture().expect("next picture") {
        assert_eq!((sample.picture.width, sample.picture.height), (8, 8));
        let PictureData::System { layout, bytes, .. } = sample.picture.data() else {
            panic!("a system picture");
        };
        assert_eq!(*layout, PictureLayout::Rgba8);
        assert_eq!(bytes.len(), 8 * 8 * 4);
        out.push((
            sample.pts.expect("pts"),
            sample.frame_duration,
            pixel(bytes, 0, 0),
            pixel(bytes, 7, 7),
        ));
        assert!(out.len() < 10, "the decoder must end");
    }
    out
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

fn check_animation(name: &str) {
    let mut dec = AnimDecoder::open(&fixture(name)).expect("open");
    assert!(!dec.has_audio(), "{name}");
    assert_eq!(dec.next_sound().expect("sound"), None, "{name}");
    assert_eq!(dec.duration(), Some(ms(400)), "{name}");
    let frames = read_all(&mut dec);
    assert_eq!(
        frames,
        vec![
            (ms(0), Some(ms(100)), RED, RED),
            (ms(100), Some(ms(200)), GREEN, RED),
            (ms(300), Some(ms(100)), BLUE, BLUE),
        ],
        "{name}"
    );

    // Seeking into the second frame starts there, composed over the first.
    dec.seek(ms(150)).expect("seek");
    let frames = read_all(&mut dec);
    assert_eq!(frames.len(), 2, "{name}");
    assert_eq!(frames[0], (ms(100), Some(ms(200)), GREEN, RED), "{name}");

    // Seeking to the start reads every frame again.
    dec.seek(Duration::ZERO).expect("seek");
    assert_eq!(read_all(&mut dec).len(), 3, "{name}");

    // Seeking past the end yields no picture.
    dec.seek(ms(5_000)).expect("seek");
    assert!(read_all(&mut dec).is_empty(), "{name}");
}

#[test]
fn a_gif_decodes_composes_and_seeks() {
    check_animation("anim.gif");
}

#[test]
fn an_apng_decodes_composes_and_seeks() {
    check_animation("anim.png");
}

#[test]
fn an_animated_webp_decodes_composes_and_seeks() {
    check_animation("anim.webp");
}

fn a_published_frame_is_unchanged_by_the_next_one(name: &str) {
    let mut dec = AnimDecoder::open(&fixture(name)).expect("open");
    let first = dec.next_picture().expect("first").expect("a frame");
    let PictureData::System { bytes, .. } = first.picture.data() else {
        panic!("a system picture");
    };
    let kept = bytes.clone();
    let second = dec.next_picture().expect("second").expect("a frame");
    let PictureData::System { bytes, .. } = first.picture.data() else {
        panic!("a system picture");
    };
    assert_eq!(
        bytes, &kept,
        "{name}: the next frame rewrote the published one"
    );
    let PictureData::System { bytes: next, .. } = second.picture.data() else {
        panic!("a system picture");
    };
    assert_ne!(next, &kept, "{name}");
}

#[test]
fn a_published_gif_apng_or_webp_frame_is_not_the_next_frame_canvas() {
    a_published_frame_is_unchanged_by_the_next_one("anim.gif");
    a_published_frame_is_unchanged_by_the_next_one("anim.png");
    a_published_frame_is_unchanged_by_the_next_one("anim.webp");
}

#[test]
fn a_still_png_is_one_frame_without_duration() {
    let mut dec = AnimDecoder::open(&fixture("still.png")).expect("open");
    assert_eq!(dec.duration(), None);
    let frames = read_all(&mut dec);
    assert_eq!(frames, vec![(ms(0), None, GREEN, RED)]);
}

#[test]
fn the_factory_opens_by_content_not_by_extension() {
    let factory = AnimFactory::new();
    assert!(factory.open(&fixture("anim.gif")).is_ok());
    // The crate's own manifest is no image.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    assert!(factory.open(&manifest).is_err());
}
