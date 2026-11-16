//! The fake adapter reads pictures and sound separately, as the port says (ADR-0108
//! decision 4).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use rbs_media_decode::{InterleavedPcm, MediaDecoderFactory, SHORT_DELAY_AS};
use rbs_media_decode_test::{SOUND_CHUNK_FRAMES, ScriptClip, ScriptFactory, ScriptFrame};

fn with_sound(rate: u32, seconds: f32) -> ScriptClip {
    let frames = (rate as f32 * seconds) as usize;
    ScriptClip {
        audio: Some(InterleavedPcm {
            sample_rate: rate,
            channels: 2,
            samples: vec![0.25; frames * 2],
        }),
        duration: Some(Duration::from_millis(300)),
        ..ScriptClip::silent_frames()
    }
}

#[test]
fn pictures_and_sound_are_read_per_stream() {
    let factory = ScriptFactory::new(with_sound(44_100, 0.3));
    let mut decoder = factory.open(Path::new("clip")).unwrap();
    assert!(decoder.has_audio());
    assert_eq!(decoder.duration(), Some(Duration::from_millis(300)));
    let mut pictures = 0;
    while decoder.next_picture().unwrap().is_some() {
        pictures += 1;
    }
    assert_eq!(pictures, 3);
    let mut frames = 0;
    let mut last_pts = None;
    while let Some(sound) = decoder.next_sound().unwrap() {
        assert_eq!(sound.pcm.sample_rate, 44_100);
        assert!(sound.pcm.samples.len() / 2 <= SOUND_CHUNK_FRAMES);
        if let Some(prev) = last_pts {
            assert!(sound.pts.unwrap() > prev);
        }
        last_pts = sound.pts;
        frames += sound.pcm.samples.len() / 2;
    }
    assert_eq!(frames, 13_230);
}

#[test]
fn seek_moves_both_streams() {
    let factory = ScriptFactory::new(with_sound(48_000, 0.3));
    let mut decoder = factory.open(Path::new("clip")).unwrap();
    decoder.seek(Duration::from_millis(100)).unwrap();
    let picture = decoder.next_picture().unwrap().unwrap();
    assert_eq!(picture.pts, Some(Duration::from_millis(100)));
    let sound = decoder.next_sound().unwrap().unwrap();
    assert_eq!(sound.pts, Some(Duration::from_millis(100)));
}

#[test]
fn a_clip_without_sound_has_no_sound_stream() {
    let factory = ScriptFactory::new(ScriptClip::silent_frames());
    let mut decoder = factory.open(Path::new("clip")).unwrap();
    assert!(!decoder.has_audio());
    assert!(decoder.next_sound().unwrap().is_none());
    assert_eq!(decoder.duration(), None);
}

#[test]
fn a_zero_delay_animation_frame_reads_as_100_ms() {
    let clip = ScriptClip {
        frames: vec![ScriptFrame::rgba(2, 2, Duration::ZERO, Duration::ZERO, 1)],
        animated: true,
        ..ScriptClip::silent_frames()
    };
    let factory = ScriptFactory::new(clip);
    let mut decoder = factory.open(Path::new("clip.gif")).unwrap();
    let picture = decoder.next_picture().unwrap().unwrap();
    assert_eq!(picture.frame_duration, Some(SHORT_DELAY_AS));
}
