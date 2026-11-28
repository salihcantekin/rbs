//! The decoders a guest source uses, as the composition root registers them (spec
//! `guest-source.md`): live video decoder factories, best first, and one Opus decoder
//! factory. Making this opens nothing (P13): decoder sessions exist only in a running
//! worker, the video one only while the source is shown.

use std::sync::Arc;

use rbs_codec::{
    AudioDecoderConfig, AudioDecoderFactory, CreateError, DecodeOutput, VideoDecoderConfig,
    VideoDecoderFactory,
};
use rbs_core::media::{AudioFormat, CodecId};

use super::node::GuestDecoders;

/// The registered decoder factories. Cheap to clone.
#[derive(Clone, Default)]
pub struct GuestDecoding {
    video: Vec<Arc<dyn VideoDecoderFactory>>,
    audio: Option<Arc<dyn AudioDecoderFactory>>,
    output: DecodeOutput,
}

impl GuestDecoding {
    /// Adds a video decoder factory after those already added (best first).
    pub fn add_video(&mut self, factory: Arc<dyn VideoDecoderFactory>) {
        self.video.push(factory);
    }

    /// Sets the Opus decoder factory.
    pub fn set_audio(&mut self, factory: Arc<dyn AudioDecoderFactory>) {
        self.audio = Some(factory);
    }

    /// Where decoded pictures should live (the studio's GPU adapter and the memory
    /// kinds its importer takes). Default: system memory.
    pub fn set_output(&mut self, output: DecodeOutput) {
        self.output = output;
    }

    /// True when a guest source can run: some video factory and the Opus factory.
    pub fn is_complete(&self) -> bool {
        !self.video.is_empty() && self.audio.is_some()
    }

    /// The decoders for one worker: the Opus decoder now, video sessions on demand,
    /// each from the first factory that makes one. `Unsupported` when
    /// [`Self::is_complete`] is false.
    pub fn decoders(&self) -> Result<GuestDecoders, CreateError> {
        let audio = self
            .audio
            .as_ref()
            .ok_or_else(|| CreateError::Unsupported {
                what: "no Opus decoder".into(),
            })?;
        if self.video.is_empty() {
            return Err(CreateError::Unsupported {
                what: "no live video decoder".into(),
            });
        }
        let audio = audio.create(&AudioDecoderConfig::new(
            CodecId::Opus,
            AudioFormat::STEREO_48K,
        ))?;
        let factories = self.video.clone();
        let output = self.output;
        Ok(GuestDecoders {
            video: Box::new(move |config: &VideoDecoderConfig| {
                let config = config.clone().with_target(output);
                let mut last = CreateError::Unsupported {
                    what: "no live video decoder".into(),
                };
                for f in &factories {
                    match f.create(&config) {
                        Ok(d) => return Ok(d),
                        Err(e) => last = e,
                    }
                }
                Err(last)
            }),
            audio,
        })
    }
}
