//! A synthetic audio input adapter: sine-wave microphones and an output loopback.
//!
//! It runs on every platform without hardware and is fully deterministic: audio is
//! produced in 10 ms packets on the cadence of the request's clock when
//! [`AudioInputStream::read`] is called, with no threads. It is the reference
//! implementation of the audio I/O port (`docs/specs/audio-io-port.md`) and lets the
//! engine, CI and other crates' tests use audio input without an OS audio API.
//!
//! Like a real adapter, it has its own device clock (offset and drifting from the
//! master clock, see [`to_source`]), stamps packets at the edge with an
//! [`EdgeClock`], and hands samples to `read` through the port's lock-free
//! [`ring`], which `start` allocates. A consumer that stops
//! reading loses the newest packets, exactly as with an OS callback.
//!
//! With the `conformance` feature, the backend also implements the port's
//! `conformance::TestHooks`, so tests can remove endpoints and count the streams it
//! holds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use rbs_audio_io::edge::EdgeClock;
use rbs_audio_io::ring::{self, Push, RingConsumer, RingProducer};
use rbs_audio_io::{
    AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, AudioRead, EndpointId,
    EndpointKind, InputRequest, InputStats, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::SampleRate;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.audio_io.test";

/// Offset of the device clock from the master clock.
pub const SOURCE_OFFSET: Duration = Duration::from_secs(3);

/// Drift of the device clock in parts per million (it runs fast by this much).
pub const SOURCE_DRIFT_PPM: u64 = 100;

/// Length of one device packet.
pub const PACKET: Duration = Duration::from_millis(10);

/// Peak amplitude of the test signal.
pub const AMPLITUDE: f32 = 0.5;

/// The mono microphone: 48 kHz native, also 44.1 and 96 kHz.
pub const MONO_INPUT: &str = "test.input.mono";
/// The stereo microphone, the default input: 48 kHz only.
pub const STEREO_INPUT: &str = "test.input.stereo";
/// The loopback of the default output: 44.1 kHz native, also 48 kHz.
pub const LOOPBACK: &str = "test.loopback";

/// The device clock in ns, as a function of master time: offset by
/// [`SOURCE_OFFSET`] and running fast by [`SOURCE_DRIFT_PPM`].
pub fn to_source(master: Timestamp) -> u64 {
    let m = master.as_nanos();
    SOURCE_OFFSET.as_nanos() as u64
        + m
        + m / 1_000_000 * SOURCE_DRIFT_PPM
        + m % 1_000_000 * SOURCE_DRIFT_PPM / 1_000_000
}

/// The master time of device time `source` (inverse of [`to_source`], to 1 ns).
pub fn to_master(source: u64) -> Timestamp {
    let s = u128::from(source.saturating_sub(SOURCE_OFFSET.as_nanos() as u64));
    let m = s * 1_000_000 / (1_000_000 + u128::from(SOURCE_DRIFT_PPM));
    Timestamp::from_nanos(u64::try_from(m).unwrap_or(u64::MAX))
}

/// Sample `frame` (counted from `start`) of `channel`: a sine at 440 Hz times the
/// channel number plus one.
pub fn signal(frame: u64, channel: usize, rate: SampleRate) -> f32 {
    let hz = 440.0 * (channel as f64 + 1.0);
    let rate = f64::from(rate.hz());
    // Reduce the phase in integers first so long runs stay exact.
    let period = rate / hz;
    let phase = (frame as f64 % (period * 1_000.0)) / period;
    (AMPLITUDE as f64 * (std::f64::consts::TAU * phase).sin()) as f32
}

fn endpoints() -> Vec<AudioEndpoint> {
    let mono = AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Mono)
        .unwrap_or(AudioFormat::STEREO_48K);
    let loopback = AudioFormat::new(SampleRate::HZ_44100, ChannelLayout::Stereo)
        .unwrap_or(AudioFormat::STEREO_48K);
    vec![
        AudioEndpoint::new(
            EndpointId::new(MONO_INPUT),
            EndpointKind::Input,
            "Test microphone (mono)",
            mono,
        )
        .with_sample_rates(&[
            SampleRate::HZ_48000,
            SampleRate::HZ_44100,
            SampleRate::HZ_96000,
        ]),
        AudioEndpoint::new(
            EndpointId::new(STEREO_INPUT),
            EndpointKind::Input,
            "Test microphone (stereo)",
            AudioFormat::STEREO_48K,
        )
        .with_default(true),
        AudioEndpoint::new(
            EndpointId::new(LOOPBACK),
            EndpointKind::OutputLoopback,
            "Test output (loopback)",
            loopback,
        )
        .with_sample_rates(&[SampleRate::HZ_44100, SampleRate::HZ_48000])
        .with_default(true),
    ]
}

/// The synthetic backend. Cheap to create; owns nothing until a stream starts.
#[derive(Debug)]
pub struct TestAudioBackend {
    endpoints: Vec<(AudioEndpoint, Arc<AtomicBool>)>,
    live: Arc<AtomicUsize>,
}

impl Default for TestAudioBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TestAudioBackend {
    /// A backend with every test endpoint present.
    pub fn new() -> Self {
        Self {
            endpoints: endpoints()
                .into_iter()
                .map(|e| (e, Arc::new(AtomicBool::new(true))))
                .collect(),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Active streams right now.
    pub fn active_streams(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// Makes `endpoint` disappear, as if unplugged. Returns `false` for unknown ids.
    pub fn remove(&self, endpoint: &EndpointId) -> bool {
        self.endpoints
            .iter()
            .find(|(e, _)| e.id == *endpoint)
            .is_some_and(|(_, present)| {
                present.store(false, Ordering::Release);
                true
            })
    }
}

#[cfg(feature = "conformance")]
impl rbs_audio_io::conformance::TestHooks for TestAudioBackend {
    fn remove(&self, endpoint: &EndpointId) -> bool {
        TestAudioBackend::remove(self, endpoint)
    }

    fn resources(&self) -> Option<u64> {
        Some(self.active_streams() as u64)
    }
}

impl AdapterFactory for TestAudioBackend {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Test audio (synthetic)"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let kinds = || {
            CapabilityValue::Set(vec![
                EndpointKind::Input.as_str().into(),
                EndpointKind::OutputLoopback.as_str().into(),
            ])
        };
        Ok(CapabilitySet::new()
            .with(caps::INPUT_KINDS, kinds())
            .with(caps::INPUT_ENUMERATE, kinds())
            .with(caps::INPUT_CHANNELS_MAX, CapabilityValue::Int(2))
            .with(caps::INPUT_PERMISSION, CapabilityValue::Flag(false))
            .with(HARDWARE, CapabilityValue::Flag(false)))
    }
}

impl AudioBackend for TestAudioBackend {
    fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
        if kind == EndpointKind::Application {
            return Err(AudioIoError::Unsupported("application capture"));
        }
        Ok(self
            .endpoints
            .iter()
            .filter(|(e, present)| e.kind == kind && present.load(Ordering::Acquire))
            .map(|(e, _)| e.clone())
            .collect())
    }

    fn open_input(
        &self,
        request: &InputRequest,
    ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
        let Some((endpoint, present)) = self
            .endpoints
            .iter()
            .find(|(e, present)| e.id == request.endpoint && present.load(Ordering::Acquire))
        else {
            return Err(AudioIoError::EndpointNotFound);
        };
        Ok(Box::new(TestStream {
            format: request.format_for(endpoint),
            clock: Arc::clone(&request.clock),
            buffer: request.clamped_buffer(),
            present: Arc::clone(present),
            live: Arc::clone(&self.live),
            active: None,
            totals: InputStats::default(),
            edge: EdgeClock::new(Duration::from_millis(100)),
        }))
    }
}

/// Counts an Active stream for [`TestHooks::resources`] while it lives.
struct LiveGuard(Arc<AtomicUsize>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Active {
    producer: RingProducer,
    consumer: RingConsumer,
    /// Device time of the first frame.
    start_source: u64,
    /// Packets pushed or lost so far.
    packets: u64,
    packet_frames: usize,
    /// One packet of interleaved samples, reused.
    scratch: Vec<f32>,
    _live: LiveGuard,
}

struct TestStream {
    format: AudioFormat,
    clock: Arc<dyn Clock>,
    buffer: Duration,
    present: Arc<AtomicBool>,
    live: Arc<AtomicUsize>,
    active: Option<Active>,
    /// Counters of rings closed by `stop`.
    totals: InputStats,
    edge: EdgeClock,
}

impl TestStream {
    /// Produces every packet the device completed by master time `now`, as its
    /// callback would have. No allocation.
    fn pump(&mut self, now: Timestamp) {
        let rate = self.format.sample_rate;
        let channels = self.format.channels.channels();
        let source_now = to_source(now);
        self.edge.observe(source_now, now);
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let elapsed = u128::from(source_now.saturating_sub(active.start_source));
        let frames = elapsed * u128::from(rate.hz()) / 1_000_000_000;
        let complete = u64::try_from(frames / active.packet_frames as u128).unwrap_or(u64::MAX);
        let pending = complete.saturating_sub(active.packets);
        // Packets beyond the free space would be dropped one by one; count them at
        // once so a long pause costs no more than one buffer of work.
        let fit = (active.producer.free() / active.packet_frames) as u64 + 1;
        for _ in 0..pending.min(fit) {
            let first = active.packets * active.packet_frames as u64;
            for frame in 0..active.packet_frames {
                for channel in 0..channels {
                    active.scratch[frame * channels + channel] =
                        signal(first + frame as u64, channel, rate);
                }
            }
            let offset = u128::from(first) * 1_000_000_000 / u128::from(rate.hz());
            let source = active.start_source + u64::try_from(offset).unwrap_or(u64::MAX);
            let timestamp = self.edge.to_master(source, PACKET, now);
            if active.producer.push(timestamp, &active.scratch) == Push::Dropped {
                // Counted by the ring; the next packet starts a new run.
            }
            active.packets += 1;
        }
        let rest = pending.saturating_sub(fit);
        if rest > 0 {
            active.producer.lost(rest * active.packet_frames as u64);
            active.packets += rest;
        }
    }
}

impl AudioInputStream for TestStream {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn start(&mut self) -> Result<(), AudioIoError> {
        if self.active.is_some() {
            return Ok(());
        }
        if !self.present.load(Ordering::Acquire) {
            return Err(AudioIoError::EndpointLost);
        }
        let rate = self.format.sample_rate;
        let packet_frames = ring::frames_for(rate, PACKET);
        let capacity = ring::capacity_frames(rate, self.buffer, packet_frames);
        let (producer, consumer) = ring::channel(self.format, capacity, ring::DEFAULT_MARKS);
        let now = self.clock.now();
        self.edge.restart();
        self.edge.observe(to_source(now), now);
        self.live.fetch_add(1, Ordering::AcqRel);
        self.active = Some(Active {
            producer,
            consumer,
            start_source: to_source(now),
            packets: 0,
            packet_frames,
            scratch: vec![0.0; packet_frames * self.format.channels.channels()],
            _live: LiveGuard(Arc::clone(&self.live)),
        });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(active) = self.active.take() {
            self.totals = self.totals.plus(active.consumer.close());
        }
    }

    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError> {
        let channels = self.format.channels.channels();
        if out.len() != channels || out.iter().any(|c| c.len() != out[0].len()) {
            return Err(AudioIoError::BufferMismatch);
        }
        if self.active.is_none() {
            return Ok(AudioRead::EMPTY);
        }
        if !self.present.load(Ordering::Acquire) {
            return Err(AudioIoError::EndpointLost);
        }
        self.pump(self.clock.now());
        match self.active.as_mut() {
            Some(active) => active.consumer.read(out),
            None => Ok(AudioRead::EMPTY),
        }
    }

    fn available(&self) -> usize {
        self.active.as_ref().map_or(0, |a| a.consumer.available())
    }

    fn stats(&self) -> InputStats {
        match &self.active {
            Some(active) => self.totals.plus(active.consumer.stats()),
            None => self.totals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_clock_round_trips() {
        for m in [0_u64, 1, 999_999, 1_000_000, 123_456_789_012] {
            let t = Timestamp::from_nanos(m);
            let back = to_master(to_source(t));
            assert!(back.as_nanos().abs_diff(m) <= 1, "{m} -> {back:?}");
        }
    }

    #[test]
    fn signal_is_a_bounded_sine() {
        let rate = SampleRate::HZ_48000;
        assert_eq!(signal(0, 0, rate), 0.0);
        // A quarter period of 440 Hz is about 27.27 frames.
        assert!(signal(27, 0, rate) > 0.49);
        assert!((0..96_000).all(|n| signal(n, 1, rate).abs() <= AMPLITUDE));
    }
}
