//! Conformance of the AVFoundation adapter (ADR-0108 decision 13), run on macOS.
//!
//! `fixtures/clip.mp4`: H.264 64x36 (coded 64x48, cropped by the decoder),
//! 10 fps, 5 frames, red with a green top-left 32x18, and 0.5 s of 48 kHz stereo AAC.
//! A codec the machine lacks is a skip with a reason instead of a failure.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use rbs_media_avf::AvfMediaFactory;
use rbs_media_decode::{DecodeError, MediaDecoderFactory};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[cfg(not(target_os = "macos"))]
#[test]
fn other_platforms_refuse_as_unsupported() {
    let refused = AvfMediaFactory::new().open(&fixture("clip.mp4"));
    assert!(matches!(refused, Err(DecodeError::Unsupported(_))));
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::time::Duration;

    use rbs_media_decode::{MediaDecoder, PictureData, PictureLayout, PictureSample};

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn open() -> Option<Box<dyn MediaDecoder>> {
        match AvfMediaFactory::new().open(&fixture("clip.mp4")) {
            Ok(decoder) => Some(decoder),
            Err(DecodeError::Unsupported(why)) => {
                eprintln!("skipped: {why}");
                None
            }
            Err(e) => panic!("open: {e}"),
        }
    }

    fn near(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 24)
    }

    fn pixel(sample: &PictureSample, x: usize, y: usize) -> [u8; 4] {
        let PictureData::System { layout, bytes, .. } = sample.picture.data() else {
            panic!("a system picture");
        };
        assert_eq!(*layout, PictureLayout::Rgba8);
        let w = sample.picture.width as usize;
        let i = (y * w + x) * 4;
        [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
    }

    /// A coarse view of the picture for a failure message: the colour at every 8th
    /// pixel of every 6th row.
    fn map(sample: &PictureSample) -> String {
        let (w, h) = (
            sample.picture.width as usize,
            sample.picture.height as usize,
        );
        let mut out = String::new();
        for y in (0..h).step_by(6) {
            for x in (0..w).step_by(8) {
                let [r, g, b, _] = pixel(sample, x, y);
                out.push_str(&format!("{r:3},{g:3},{b:3} "));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn pictures_are_cropped_upright_rgba_with_stamps() {
        let Some(mut dec) = open() else { return };
        assert!(dec.has_audio());
        let length = dec.duration().expect("a stated length");
        assert!(length >= ms(450) && length <= ms(550), "{length:?}");
        let mut pictures = Vec::new();
        while let Some(sample) = dec.next_picture().unwrap() {
            pictures.push(sample);
            assert!(pictures.len() < 50, "the stream must end");
        }
        assert_eq!(pictures.len(), 5);
        for (i, p) in pictures.iter().enumerate() {
            assert_eq!((p.picture.width, p.picture.height), (64, 36));
            assert_eq!(p.pts, Some(ms(100 * i as u64)), "picture {i}");
            assert_eq!(p.frame_duration, Some(ms(100)), "picture {i}");
            let green = pixel(p, 4, 4);
            let red = pixel(p, 60, 32);
            assert!(
                near(green, [0, 255, 0, 255]),
                "picture {i} top-left {green:?}\n{}",
                map(p)
            );
            assert!(
                near(red, [255, 0, 0, 255]),
                "picture {i} bottom-right {red:?}\n{}",
                map(p)
            );
        }
    }

    #[test]
    fn a_seek_moves_both_streams() {
        let Some(mut dec) = open() else { return };
        dec.seek(ms(300)).unwrap();
        let first = dec
            .next_picture()
            .unwrap()
            .expect("a picture after the seek");
        // The picture at or before the cursor (a keyframe at most 400 ms earlier).
        assert!(first.pts.unwrap() <= ms(300));
        let mut last = first.pts.unwrap();
        while let Some(p) = dec.next_picture().unwrap() {
            last = p.pts.unwrap();
        }
        assert_eq!(last, ms(400));
        let sound = dec.next_sound().unwrap().expect("sound after the seek");
        assert!(sound.pts.unwrap() <= ms(300) && sound.pts.unwrap() >= ms(200));
    }

    #[test]
    fn sound_is_interleaved_float_of_the_whole_clip() {
        let Some(mut dec) = open() else { return };
        let mut frames = 0usize;
        let mut chunks = 0;
        let mut loudest = 0.0f32;
        while let Some(sound) = dec.next_sound().unwrap() {
            assert_eq!(sound.pcm.sample_rate, 48_000);
            assert_eq!(sound.pcm.channels, 2);
            assert_eq!(sound.pcm.samples.len() % 2, 0);
            loudest = sound
                .pcm
                .samples
                .iter()
                .fold(loudest, |m, s| m.max(s.abs()));
            frames += sound.pcm.samples.len() / 2;
            chunks += 1;
            assert!(chunks < 500, "the stream must end");
        }
        // The fixture's tone peaks near 0.1 to 0.14; a single AAC frame may stay below
        // 0.1, so the level is checked over the whole clip.
        assert!(loudest > 0.05, "the sound is silent: peak {loudest}");
        // 0.5 s, give or take one AAC frame of priming or padding.
        assert!(
            (24_000 - 1024..=24_000 + 1024).contains(&frames),
            "{frames}"
        );
    }

    #[test]
    fn a_file_that_is_no_media_is_refused() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(AvfMediaFactory::new().open(&manifest).is_err());
    }
}
