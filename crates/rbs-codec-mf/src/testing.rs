//! A fake encoder engine behind the real session (feature `testing`), so the port
//! suite and the adapter's behaviour tests run on every platform. It mimics the
//! Microsoft encoder's shape: one access unit per 1024-sample frame, delayed by
//! [`DELAY_FRAMES`] frames, which `finish` releases.

use rbs_codec::{AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError, EncodeError};
use rbs_core::capability::CapabilitySet;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

use crate::session::{self, Engine, Output, Session, delegate_audio_encoder};

/// Frames the fake engine holds back, like an encoder's priming delay.
pub const DELAY_FRAMES: usize = 2;

/// Factory of sessions over the fake engine. Same checks and capabilities as the
/// real adapter.
#[derive(Debug, Default)]
pub struct FakeAacFactory;

impl FakeAacFactory {
    /// Creates the factory.
    pub fn new() -> Self {
        Self
    }

    /// Creates a session with its concrete type.
    pub fn create_session(
        &self,
        config: &AudioEncoderConfig,
    ) -> Result<FakeAacSession, CreateError> {
        let checked = session::check(config)?;
        Ok(FakeAacSession(Session::new(
            FakeEngine::default(),
            &checked,
            &checked.asc.to_bytes(),
        )))
    }
}

impl AdapterFactory for FakeAacFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("org.rbs.codec.mf-aac.fake")
    }

    fn display_name(&self) -> &str {
        "Fake AAC (tests)"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(session::capabilities())
    }
}

impl AudioEncoderFactory for FakeAacFactory {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
        Ok(Box::new(self.create_session(config)?))
    }
}

/// A session over the fake engine.
pub struct FakeAacSession(Session<FakeEngine>);

delegate_audio_encoder!(FakeAacSession);

impl FakeAacSession {
    /// Frames the engine has received since the session started.
    pub fn frames_encoded(&self) -> u64 {
        self.0.engine().frames
    }
}

/// Payload of a fake access unit: a checksum of the frame's samples and its index on
/// the engine's timeline, so equal input gives equal packets.
#[derive(Default)]
pub struct FakeEngine {
    held: [Option<[u8; 8]>; DELAY_FRAMES],
    index: u32,
    frames: u64,
}

impl FakeEngine {
    fn emit(&mut self, payload: [u8; 8], out: &mut Output) -> Result<(), EncodeError> {
        let slot = self.index as usize % DELAY_FRAMES;
        self.index += 1;
        if let Some(old) = self.held[slot].replace(payload) {
            out.push(&old)?;
        }
        Ok(())
    }
}

impl Engine for FakeEngine {
    fn encode(&mut self, pcm: &[i16], out: &mut Output) -> Result<(), EncodeError> {
        let sum = pcm.iter().fold(0u32, |acc, v| {
            acc.wrapping_mul(31).wrapping_add(*v as u16 as u32)
        });
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&sum.to_le_bytes());
        payload[4..].copy_from_slice(&self.index.to_le_bytes());
        self.frames += 1;
        self.emit(payload, out)
    }

    fn finish(&mut self, out: &mut Output) -> Result<(), EncodeError> {
        // The delay line drains as silence pushes it out; the start is the oldest slot.
        for _ in 0..DELAY_FRAMES {
            let mut payload = [0u8; 8];
            payload[4..].copy_from_slice(&self.index.to_le_bytes());
            self.emit(payload, out)?;
        }
        self.held = [None; DELAY_FRAMES];
        self.index = 0;
        Ok(())
    }
}
