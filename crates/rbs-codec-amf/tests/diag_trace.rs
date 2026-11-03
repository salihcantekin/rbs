//! TEMPORARY diagnostic (to be removed before merge): runs the conformance suite
//! through a factory wrapper that logs every call, to find the call that crashes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rbs_codec::conformance::{self, SystemFrameSource};
use rbs_codec::{
    CreateError, EncodeError, EncoderChange, VideoEncoder, VideoEncoderConfig,
    VideoEncoderFactory,
};
use rbs_codec_amf::AmfFactory as Inner;
use rbs_core::capability::CapabilitySet;
use rbs_core::media::{EncodedPacket, StreamDescriptor, VideoFrame};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

#[derive(Debug)]
struct Logged(Inner);

impl AdapterFactory for Logged {
    fn id(&self) -> AdapterId {
        self.0.id()
    }
    fn display_name(&self) -> &str {
        self.0.display_name()
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        eprintln!("[diag] probe");
        let r = self.0.probe();
        eprintln!("[diag] probe -> {:?}", r.as_ref().map(|_| ()));
        r
    }
}

impl VideoEncoderFactory for Logged {
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
        eprintln!(
            "[diag] create {} {:?} {:?} {:?}",
            config.codec, config.format.size, config.format.pixel_format, config.rate_control
        );
        let r = self.0.create(config);
        eprintln!("[diag] create -> {:?}", r.as_ref().map(|_| ()).map_err(|e| e.to_string()));
        r.map(|inner| Box::new(Session { inner, n: 0 }) as Box<dyn VideoEncoder>)
    }
}

struct Session {
    inner: Box<dyn VideoEncoder>,
    n: u64,
}

impl VideoEncoder for Session {
    fn stream(&self) -> &StreamDescriptor {
        self.inner.stream()
    }
    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError> {
        self.n += 1;
        eprintln!("[diag] submit #{}", self.n);
        let r = self.inner.submit(frame);
        eprintln!("[diag] submit #{} -> {:?}", self.n, r);
        r
    }
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        let before = out.len();
        eprintln!("[diag] drain");
        let r = self.inner.drain(out);
        eprintln!("[diag] drain -> {:?} (+{})", r, out.len() - before);
        r
    }
    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        eprintln!("[diag] reconfigure {change:?}");
        let r = self.inner.reconfigure(change);
        eprintln!("[diag] reconfigure -> {r:?}");
        r
    }
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        eprintln!("[diag] flush");
        let r = self.inner.flush(out);
        eprintln!("[diag] flush -> {r:?}");
        r
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        eprintln!("[diag] drop session (after {} submits)", self.n);
    }
}

#[test]
fn diag_conformance_trace() {
    let report = conformance::video_encoder(&Logged(Inner::new()), &mut SystemFrameSource::new());
    eprintln!("{report}");
}
