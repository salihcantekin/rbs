//! Conformance of the AVFoundation adapter (ADR-0108 decision 13), run on macOS.
//!
//! `fixtures/clip.mp4`: H.264 Main 160x90 (coded 160x96, cropped by the decoder),
//! 10 fps, 5 frames, red with a green top-left 80x45, and 0.5 s of 48 kHz stereo AAC.
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

    /// The copy (`cpu-upload`) decoder, whose pixels the tests read.
    fn open() -> Option<Box<dyn MediaDecoder>> {
        open_with(&AvfMediaFactory::cpu_upload())
    }

    fn open_with(factory: &AvfMediaFactory) -> Option<Box<dyn MediaDecoder>> {
        open_file(factory, "clip.mp4")
    }

    fn open_file(factory: &AvfMediaFactory, name: &str) -> Option<Box<dyn MediaDecoder>> {
        match factory.open(&fixture(name)) {
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
            assert_eq!((p.picture.width, p.picture.height), (160, 90));
            assert_eq!(p.pts, Some(ms(100 * i as u64)), "picture {i}");
            assert_eq!(p.frame_duration, Some(ms(100)), "picture {i}");
            let green = pixel(p, 8, 8);
            let red = pixel(p, 150, 80);
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

    /// The luma at (`x`, `y`) of an NV12 `IOSurface`.
    fn luma(surface: usize, x: usize, y: usize) -> u8 {
        use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};
        // SAFETY: the picture's lease keeps the surface alive while the caller holds it;
        // the plane is read between lock and unlock, inside its rows.
        unsafe {
            let surface = &*(surface as *const IOSurfaceRef);
            assert_eq!(
                surface.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()),
                0
            );
            let base = surface.base_address_of_plane(0).as_ptr().cast::<u8>();
            let stride = surface.bytes_per_row_of_plane(0);
            let value = *base.add(y * stride + x);
            surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
            value
        }
    }

    /// `fixtures/rotated.mp4`: `clip.mp4` with a display matrix that turns it a quarter
    /// turn clockwise (as a phone held upright stores it), no re-encode. Upright it is
    /// 90x160 with the green quarter top right (spec invariant 12).
    #[test]
    fn a_rotated_track_is_turned_upright() {
        let Some(mut dec) = open_file(&AvfMediaFactory::cpu_upload(), "rotated.mp4") else {
            return;
        };
        let mut count = 0u64;
        while let Some(p) = dec.next_picture().unwrap() {
            assert_eq!(
                (p.picture.width, p.picture.height),
                (90, 160),
                "picture {count}"
            );
            assert_eq!(p.pts, Some(ms(100 * count)), "picture {count}");
            let green = pixel(&p, 80, 8);
            let red = pixel(&p, 8, 150);
            assert!(
                near(green, [0, 255, 0, 255]) && near(red, [255, 0, 0, 255]),
                "picture {count}: top right {green:?}, bottom left {red:?}\n{}",
                map(&p)
            );
            count += 1;
            assert!(count < 50, "the stream must end");
        }
        assert_eq!(count, 5);
        // After a seek the pictures are still upright.
        dec.seek(ms(200)).unwrap();
        let p = dec
            .next_picture()
            .unwrap()
            .expect("a picture after the seek");
        assert_eq!((p.picture.width, p.picture.height), (90, 160));
    }

    #[test]
    fn a_rotated_track_stays_native_and_upright() {
        use rbs_core::media::{FramePayload, NativeSurface};

        let Some(mut dec) = open_file(&AvfMediaFactory::new(), "rotated.mp4") else {
            return;
        };
        let mut count = 0u64;
        while let Some(p) = dec.next_picture().unwrap() {
            assert_eq!(
                (p.picture.width, p.picture.height),
                (90, 160),
                "picture {count}"
            );
            let PictureData::Native(frame) = p.picture.data() else {
                panic!("picture {count} is not native");
            };
            let FramePayload::Native {
                surface: NativeSurface::IoSurface { surface, .. },
                ..
            } = frame.payload()
            else {
                panic!("picture {count} is not an IOSurface");
            };
            // Green (luma about 173) top right, red (about 63) bottom left.
            let (green, red) = (luma(*surface, 80, 8), luma(*surface, 8, 150));
            assert!(
                green > 140 && red < 100,
                "picture {count}: luma {green} and {red}"
            );
            count += 1;
            assert!(count < 50, "the stream must end");
        }
        assert_eq!(count, 5);
    }

    #[test]
    fn native_pictures_are_nv12_io_surfaces_with_stamps() {
        use rbs_core::media::{FramePayload, NativeSurface, PixelFormat};

        let Some(mut dec) = open_with(&AvfMediaFactory::new()) else {
            return;
        };
        let mut pictures = Vec::new();
        while let Some(sample) = dec.next_picture().unwrap() {
            pictures.push(sample);
            assert!(pictures.len() < 50, "the stream must end");
        }
        assert_eq!(pictures.len(), 5);
        for (i, p) in pictures.iter().enumerate() {
            assert_eq!((p.picture.width, p.picture.height), (160, 90));
            assert_eq!(p.pts, Some(ms(100 * i as u64)), "picture {i}");
            let PictureData::Native(frame) = p.picture.data() else {
                panic!("picture {i} is not native");
            };
            assert_eq!(frame.format().pixel_format, PixelFormat::Nv12);
            let FramePayload::Native {
                surface: NativeSurface::IoSurface { surface, .. },
                ..
            } = frame.payload()
            else {
                panic!("picture {i} is not an IOSurface");
            };
            // Green (luma about 173) top left, red (about 63) bottom right.
            let (green, red) = (luma(*surface, 8, 8), luma(*surface, 150, 80));
            assert!(
                green > 140 && red < 100,
                "picture {i}: luma {green} and {red}"
            );
        }
        // Every picture is held at once: the pool hands each its own surface.
        let surfaces: std::collections::HashSet<usize> = pictures
            .iter()
            .filter_map(|p| match p.picture.data() {
                PictureData::Native(frame) => match frame.payload() {
                    rbs_core::media::FramePayload::Native {
                        surface: rbs_core::media::NativeSurface::IoSurface { surface, .. },
                        ..
                    } => Some(*surface),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(surfaces.len(), 5);
        // Pictures outlive the decoder: their surfaces stay valid (the graveyard).
        drop(dec);
        let first = surfaces.iter().copied().next().unwrap();
        let _ = luma(first, 8, 8);
    }

    #[test]
    fn released_native_pictures_are_reused_and_the_seek_still_works() {
        let Some(mut dec) = open_with(&AvfMediaFactory::new()) else {
            return;
        };
        for _ in 0..3 {
            // Each pass drops its pictures, so the slots come back.
            dec.seek(Duration::ZERO).unwrap();
            let mut count = 0;
            while let Some(sample) = dec.next_picture().unwrap() {
                assert!(sample.picture.is_native());
                count += 1;
                assert!(count < 50, "the stream must end");
            }
            assert_eq!(count, 5);
        }
    }
}
