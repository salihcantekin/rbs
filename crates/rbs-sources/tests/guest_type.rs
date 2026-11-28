//! The guest source type in the registry (spec `guest-source.md`, "Registration"):
//! installed only with live decoders, a picture node and a sound node, Standby on
//! instantiate (no worker thread).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;

use rbs_audio::AudioGraphConfig;
use rbs_codec::{
    AudioDecoder, AudioDecoderConfig, AudioDecoderFactory, CreateError, DecodeError, VideoDecoder,
    VideoDecoderConfig, VideoDecoderFactory,
};
use rbs_core::capability::CapabilitySet;
use rbs_core::media::{AudioFormat, EncodedPacket};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::TestClock;
use rbs_graph::node::NodeTypeId;
use rbs_graph::runtime::Node;
use rbs_scene::SceneCollection;
use rbs_scene::binder::NodeTypes;
use rbs_scene::model::{SettingValue, Settings};
use rbs_sources::types;
use rbs_sources::{InstanceContext, SourceError, SourceInstance, SourceTypes, guest};

struct NoVideo;

impl AdapterFactory for NoVideo {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.video-decode")
    }
    fn display_name(&self) -> &str {
        "Test video decoder"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new())
    }
}

impl VideoDecoderFactory for NoVideo {
    fn create(&self, _: &VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError> {
        Err(CreateError::ResourceExhausted)
    }
}

struct Silence;

impl AudioDecoder for Silence {
    fn format(&self) -> AudioFormat {
        AudioFormat::STEREO_48K
    }
    fn max_frames(&self) -> usize {
        5760
    }
    fn decode(&mut self, _: &EncodedPacket, out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        out.clear();
        Ok(0)
    }
    fn conceal(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        out.clear();
        out.resize(frames * 2, 0.0);
        Ok(frames)
    }
    fn reset(&mut self) {}
}

struct SilenceFactory;

impl AdapterFactory for SilenceFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.opus-decode")
    }
    fn display_name(&self) -> &str {
        "Test Opus decoder"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new())
    }
}

impl AudioDecoderFactory for SilenceFactory {
    fn create(&self, _: &AudioDecoderConfig) -> Result<Box<dyn AudioDecoder>, CreateError> {
        Ok(Box::new(Silence))
    }
}

fn ty() -> NodeTypeId {
    NodeTypeId::new(types::GUEST_SOURCE)
}

fn ctx() -> InstanceContext {
    InstanceContext::new(Arc::new(TestClock::new()), AudioGraphConfig::default())
}

fn source(settings: Settings) -> rbs_scene::model::Source {
    let mut col = SceneCollection::new();
    let (id, _) = col.add_source("Guest", ty(), settings).unwrap();
    col.source(id).unwrap().clone()
}

#[test]
fn the_guest_type_is_installed_only_with_both_decoders() {
    let mut reg = SourceTypes::new();
    assert!(!reg.is_installed(&ty()));
    assert!(reg.node_types().descriptor(&ty()).is_none());
    assert!(matches!(
        reg.instantiate(&source(Settings::new()), &ctx()),
        Err(SourceError::NoAdapter(_))
    ));
    reg.guest_decoding().add_video(Arc::new(NoVideo));
    assert!(!reg.is_installed(&ty()), "no Opus decoder yet");
    reg.guest_decoding().set_audio(Arc::new(SilenceFactory));
    assert!(reg.is_installed(&ty()));
    assert!(reg.installed().contains(&ty()));
    let table = reg.node_types();
    assert!(table.descriptor(&ty()).is_some());
    assert_eq!(
        table.sound_type(&ty()),
        Some(NodeTypeId::new(guest::GUEST_SOUND_NODE_TYPE))
    );
}

#[test]
fn an_instance_is_standby_and_reads_its_settings() {
    let mut reg = SourceTypes::new();
    reg.guest_decoding().add_video(Arc::new(NoVideo));
    reg.guest_decoding().set_audio(Arc::new(SilenceFactory));
    let mut settings = Settings::new();
    settings.insert("jitter_ms".into(), SettingValue::Int(250));
    let SourceInstance::Guest(g) = reg.instantiate(&source(settings), &ctx()).unwrap() else {
        panic!("expected a guest instance");
    };
    assert!(!g.control.running(), "no worker before a pairing connects");
    assert_eq!(g.control.settings().jitter.as_millis(), 250);
    let sound = <_ as Node<rbs_gpu::node::GpuRuntime>>::descriptor(&g.sound);
    assert_eq!(sound.type_id.as_str(), guest::GUEST_SOUND_NODE_TYPE);
}

#[test]
fn edits_are_checked_against_the_schema() {
    let ok: Settings = [("check_phrase".to_owned(), SettingValue::Bool(false))].into();
    assert!(rbs_sources::validate_settings(&ty(), &ok).is_ok());
    let bad: Settings = [("jitter_ms".to_owned(), SettingValue::Int(5000))].into();
    assert!(rbs_sources::validate_settings(&ty(), &bad).is_err());
    let other: Settings = [("url".to_owned(), SettingValue::Text("x".into()))].into();
    assert!(rbs_sources::validate_settings(&ty(), &other).is_err());
}
