//! Separate audio files (ADR-0086 step 2): one AAC file per marked source, tapped
//! before the fader, on partitions that are not output slots. Reference adapters on
//! a `TestClock`, as in `studio.rs`. Waits are for packet counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use rbs_codec::{
    AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError, EncodeError, EncoderChange,
    caps as codec_caps,
};
use rbs_core::capability::CapabilitySet;
use rbs_core::media::{
    AudioBlock, BitstreamFormat, Bytes, CodecId, ContainerId, EncodedPacket, StreamDescriptor,
    StreamParams,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_record::{
    ISO_AUDIO_ENCODER, ISO_OUTPUT, IsoAudioStart, OutputSettings, RecordError, Studio,
    StudioSettings,
};
use rbs_scene::model::RecordSeparately;
use support::*;

fn aac(log: &Log) -> usize {
    log.packets
        .lock()
        .unwrap()
        .iter()
        .filter(|packet| packet.codec == CodecId::Aac)
        .count()
}

fn start_request(url: &str) -> IsoAudioStart {
    IsoAudioStart::new(
        url,
        ContainerId::Matroska,
        "master.mkv",
        "2026-11-13T19:00:00Z",
        1_000,
    )
}

#[test]
fn separate_audio_follows_a_muted_source_and_is_not_an_output() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let display = col.sources().find(|s| s.name == "Display").unwrap().id;
    col.set_source_muted(mic, true).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    let idle = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap();
    assert_eq!(idle.audio_tracks, 0);
    assert!(!studio.iso_audio_running());
    assert!(!studio.engine().has_worker(ISO_AUDIO_ENCODER));
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 0);

    col.set_source_record_separately(mic, RecordSeparately::new(false, true))
        .unwrap();
    col.set_source_record_separately(display, RecordSeparately::new(true, false))
        .unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    let started = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap();
    assert_eq!(started.audio_tracks, 1);
    assert_eq!(started.video_pending, 1);
    assert!(studio.iso_audio_running());
    assert!(studio.outputs().is_empty());
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 0);
    let text = started.manifest.to_toml();
    assert!(text.contains("name = \"Microphone.mkv\"\n"));
    assert!(text.contains("name = \"Display\"\n"));
    assert!(!text.contains("start_offset_ns"));
    let err = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap_err();
    assert!(matches!(err, RecordError::IsoRunning));

    let mic_node = studio
        .sources()
        .into_iter()
        .find(|s| s.name == "Microphone")
        .unwrap()
        .node;
    assert!(
        studio.engine().crossings().iter().any(|crossing| {
            crossing.link.from.node == mic_node && crossing.to == ISO_AUDIO_ENCODER
        }),
        "the file is tapped from the microphone, before the mixer"
    );
    let reached = run_until(&mut studio, &clock, |_| {
        aac(&r.log) >= 4 && r.log.headers.load(Ordering::Relaxed) >= 1
    });
    assert!(
        reached,
        "aac {} headers {}",
        aac(&r.log),
        r.log.headers.load(Ordering::Relaxed)
    );
    assert!(studio.engine().has_worker(ISO_AUDIO_ENCODER));
    assert!(studio.engine().has_worker(ISO_OUTPUT));

    let id = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert_eq!(studio.outputs(), [id]);
    assert!(studio.iso_audio_running());
    let report = studio.stop_output(id).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");

    assert!(studio.stop_iso_audio());
    assert!(!studio.iso_audio_running());
    assert!(!studio.engine().has_worker(ISO_AUDIO_ENCODER));
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    assert!(r.log.trailers.load(Ordering::Relaxed) >= 1);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_separate_audio_file_that_cannot_open_leaves_the_studio_running() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    col.set_source_record_separately(mic, RecordSeparately::new(false, true))
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

    let err = studio
        .start_iso_audio(&r.adapters, &start_request("mem://fail"))
        .unwrap_err();
    assert!(matches!(err, RecordError::Start(_)), "{err}");
    assert!(!studio.iso_audio_running());
    assert!(!studio.engine().has_worker(ISO_AUDIO_ENCODER));
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    assert!(studio.outputs().is_empty());

    let started = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap();
    assert_eq!(started.audio_tracks, 1);
    assert!(studio.stop_iso_audio());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_marked_source_the_mix_does_not_hear_fails_or_is_left_out() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    // In the collection, on no scene: the program mix does not hear it.
    let (off_air, _) = col
        .add_source(
            "Spare mic",
            rbs_graph::node::NodeTypeId::new_static(rbs_sources::types::AUDIO_INPUT_SOURCE),
            rbs_scene::model::Settings::new(),
        )
        .unwrap();
    for id in [mic, off_air] {
        col.set_source_record_separately(id, RecordSeparately::new(false, true))
            .unwrap();
    }
    let (engine, _clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    let err = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap_err();
    assert!(
        matches!(err, RecordError::Iso(rbs_record::IsoError::NotHeard { .. })),
        "{err}"
    );
    assert!(!studio.iso_audio_running());

    let mut skipping = start_request("mem://iso");
    skipping.skip_unheard = true;
    let started = studio.start_iso_audio(&r.adapters, &skipping).unwrap();
    assert_eq!(started.audio_tracks, 1);
    assert_eq!(started.not_heard, [(off_air, "Spare mic".to_owned())]);
    let text = started.manifest.to_toml();
    assert!(text.contains("Microphone.mkv"), "{text}");
    assert!(!text.contains("Spare mic"), "{text}");
    assert!(studio.stop_iso_audio());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// Remembers the adapter options of the audio session it opens, and emits nothing.
struct SeenAudio {
    codec: CodecId,
    options: Arc<Mutex<Vec<(String, String)>>>,
}

impl AdapterFactory for SeenAudio {
    fn id(&self) -> AdapterId {
        match self.codec {
            CodecId::Flac => AdapterId::new("org.rbs.test.flac"),
            _ => AdapterId::new("org.rbs.test.pcm"),
        }
    }
    fn display_name(&self) -> &str {
        "seen audio"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new().with(codec_caps::AUDIO_ENCODE, set(&[self.codec.as_str()])))
    }
}

impl AudioEncoderFactory for SeenAudio {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
        *self.options.lock().unwrap() = config.adapter_options.clone();
        let stream = StreamDescriptor::new(
            self.codec,
            BitstreamFormat::Raw,
            Bytes::new(),
            StreamParams::audio(config.format, 480),
        );
        Ok(Box::new(SeenSession { stream }))
    }
}

struct SeenSession {
    stream: StreamDescriptor,
}

impl AudioEncoder for SeenSession {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }
    fn frame_size(&self) -> u32 {
        480
    }
    fn submit(&mut self, _: &AudioBlock<'_>) -> Result<(), EncodeError> {
        Ok(())
    }
    fn drain(&mut self, _: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        Ok(())
    }
    fn reconfigure(&mut self, _: &EncoderChange) -> Result<(), EncodeError> {
        Ok(())
    }
    fn flush(&mut self, _: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        Ok(())
    }
}

fn iso_bit_depth_reaches(codec: CodecId) {
    let Some(gpu) = gpu() else { return };
    let mut r = rig();
    let seen = Arc::new(Mutex::new(Vec::new()));
    r.adapters
        .audio_encoders
        .register(Arc::new(SeenAudio {
            codec,
            options: Arc::clone(&seen),
        }))
        .unwrap();
    r.adapters.probe();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    col.set_source_record_separately(mic, RecordSeparately::new(false, true))
        .unwrap();
    let (engine, _) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let mut start = start_request("mem://iso");
    start.audio.codec = codec;
    start.audio.bitrate_bps = 0;
    start.audio.bit_depth = Some(24);
    let started = studio.start_iso_audio(&r.adapters, &start).unwrap();
    assert_eq!(started.audio_tracks, 1);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [("bit_depth".to_owned(), "24".to_owned())]
    );
    assert!(studio.stop_iso_audio());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn iso_pcm_bit_depth_reaches_the_audio_encoder() {
    iso_bit_depth_reaches(CodecId::Pcm);
}

#[test]
fn iso_flac_bit_depth_reaches_the_audio_encoder() {
    iso_bit_depth_reaches(CodecId::Flac);
}
