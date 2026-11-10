//! Output node: resampling to the device, drift steering against the device clock,
//! underrun and stall recovery, volume and no allocation per block (audio graph spec
//! "Output node", ADR-0097 decision 3).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_audio::{
    AudioGraphConfig, DEFAULT_OUTPUT_TARGET, InputNode, OutputCounters, OutputNode,
    OutputNodeStats, OutputParams,
};
use rbs_audio_io::output::channel as ring;
use rbs_audio_io::{AudioIoError, AudioOutput, OutputConsumer, OutputProducer, OutputStats};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::{Cadence, SampleRate};
use rbs_graph::description::GraphDescription;
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{Controller, Executor, Node, Tick, channel};
use support::{Device, Script};

/// The device half of a scripted output: the test plays the render callback.
struct DeviceSide {
    consumer: Mutex<OutputConsumer>,
    running: AtomicBool,
}

struct ScriptedOutput {
    format: AudioFormat,
    producer: Option<OutputProducer>,
    side: Arc<DeviceSide>,
}

impl AudioOutput for ScriptedOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn producer(&mut self) -> Option<OutputProducer> {
        self.producer.take()
    }
    fn start(&mut self) -> Result<(), AudioIoError> {
        self.side.running.store(true, Ordering::Release);
        Ok(())
    }
    fn stop(&mut self) {
        self.side.running.store(false, Ordering::Release);
    }
    fn latency_ns(&self) -> u64 {
        10_000_000
    }
    fn stats(&self) -> OutputStats {
        self.side.consumer.lock().unwrap().stats()
    }
    fn status(&self) -> Result<(), AudioIoError> {
        Ok(())
    }
}

/// What the scripted output device does.
#[derive(Clone, Copy)]
struct Out {
    format: AudioFormat,
    /// Device clock runs fast by this fraction.
    drift: f64,
    /// Frames per render callback.
    period: usize,
    ring: usize,
}

impl Default for Out {
    fn default() -> Self {
        Self {
            format: AudioFormat::STEREO_48K,
            drift: 0.0,
            period: 512,
            ring: 16_384,
        }
    }
}

/// A tone input through an output node into a scripted device, on one executor.
struct Rig {
    clock: TestClock,
    _ctl: Controller<()>,
    exec: Executor<()>,
    cadence: Cadence,
    tick: u64,
    out: Out,
    side: Arc<DeviceSide>,
    /// Device frames owed to the device clock, not yet pulled.
    owed: f64,
    /// Channel 0 of everything the device played.
    played: Vec<f32>,
    buf: Vec<f32>,
    stats: Arc<OutputNodeStats>,
    params: ParamSender<OutputParams>,
    latency: Duration,
}

impl Rig {
    fn new(config: AudioGraphConfig, out: Out) -> Self {
        let clock = TestClock::new();
        let device = Device::new(Script::default(), clock.clone());
        let (input, _, _) = InputNode::new(Box::new(device), config, Duration::from_millis(200));
        let (producer, consumer) = ring(out.format.channels.channels(), out.ring);
        let side = Arc::new(DeviceSide {
            consumer: Mutex::new(consumer),
            running: AtomicBool::new(false),
        });
        let output = ScriptedOutput {
            format: out.format,
            producer: Some(producer),
            side: Arc::clone(&side),
        };
        let (node, params, stats) =
            OutputNode::new(Box::new(output), config, DEFAULT_OUTPUT_TARGET);
        let latency = node.latency();
        let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
        ctl.set_audio_block_frames(config.block_frames);
        let mut g = GraphDescription::new();
        let ids: Vec<_> = [
            Node::<()>::descriptor(&input).clone(),
            Node::<()>::descriptor(&node).clone(),
        ]
        .into_iter()
        .map(|d| {
            let id = g.add_node(Arc::new(d)).unwrap();
            g.set_state(id, Lifecycle::Standby).unwrap();
            g.set_state(id, Lifecycle::Active).unwrap();
            id
        })
        .collect();
        g.connect(ids[0], "out", ids[1], "in").unwrap();
        ctl.adopt(ids[0], Box::new(input)).unwrap();
        ctl.adopt(ids[1], Box::new(node)).unwrap();
        ctl.apply(&g).unwrap();
        exec.swap_pending();
        ctl.collect().unwrap().unwrap();
        let channels = out.format.channels.channels();
        Self {
            clock,
            _ctl: ctl,
            exec,
            cadence: config.cadence(Timestamp::ZERO),
            tick: 0,
            out,
            side,
            owed: 0.0,
            played: Vec::new(),
            buf: vec![0.0; out.period * channels],
            stats,
            params,
            latency,
        }
    }

    /// The device plays for `d` of real time: its clock owes it frames, which it pulls
    /// in whole periods.
    fn play(&mut self, d: Duration) {
        if !self.side.running.load(Ordering::Acquire) {
            return;
        }
        let rate = f64::from(self.out.format.sample_rate.hz()) * (1.0 + self.out.drift);
        self.owed += d.as_secs_f64() * rate;
        let channels = self.out.format.channels.channels();
        while self.owed >= self.out.period as f64 {
            self.owed -= self.out.period as f64;
            self.side.consumer.lock().unwrap().pull(&mut self.buf);
            self.played
                .extend(self.buf.chunks_exact(channels).map(|f| f[0]));
        }
    }

    /// Runs `n` ticks; the device plays the time between them.
    fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.step(|exec, tick| exec.run_tick(tick));
        }
    }

    fn step(&mut self, run: impl FnOnce(&mut Executor<()>, Tick)) {
        let t = self.cadence.time_of(self.tick);
        let now = self.clock.now();
        if t > now {
            self.clock.advance(t.saturating_since(now));
            self.play(t.saturating_since(now));
        }
        run(&mut self.exec, Tick::of(&self.cadence, self.tick));
        self.tick += 1;
    }

    /// The graph stalls for `d` while the device keeps playing.
    fn stall_graph(&mut self, d: Duration) {
        self.clock.advance(d);
        self.play(d);
    }

    fn ring(&self) -> OutputStats {
        self.side.consumer.lock().unwrap().stats()
    }

    fn counters(&self) -> OutputCounters {
        self.stats.read()
    }

    /// Largest change between two played samples from device frame `from` on.
    fn largest_step_after(&self, from: usize) -> f32 {
        self.played[from.min(self.played.len())..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    fn target_frames(&self) -> f64 {
        DEFAULT_OUTPUT_TARGET.as_secs_f64() * f64::from(self.out.format.sample_rate.hz())
    }
}

/// The tone (50 Hz, amplitude 0.5) changes by at most this much per frame at 44.1 kHz,
/// with margin; a gap, a skip or a click is a far larger step.
const SMOOTH: f32 = 0.006;

fn settle(r: &mut Rig) -> (OutputCounters, OutputStats, usize) {
    r.run(800); // 8 s
    (r.counters(), r.ring(), r.played.len())
}

#[test]
fn a_drifting_device_is_steered_without_underruns_or_skips() {
    for drift in [1e-4, -1e-4, 1e-3, -1e-3] {
        let mut r = Rig::new(
            AudioGraphConfig::default(),
            Out {
                drift,
                ..Out::default()
            },
        );
        let (c0, ring0, from) = settle(&mut r);
        assert_eq!(c0.primes, 1, "drift {drift}");
        let mut ppm_sum = 0.0;
        for _ in 0..1_200 {
            // 12 s more
            r.run(1);
            ppm_sum += r.counters().correction_ppm as f64;
        }
        let (c, ring) = (r.counters(), r.ring());
        assert_eq!(
            c.primes, 1,
            "drift {drift}: no underrun after the first prime"
        );
        assert_eq!(c.skipped_frames + c.overflow_frames + c.invalid_inputs, 0);
        assert_eq!(
            ring.silence, ring0.silence,
            "drift {drift}: the device ran dry"
        );
        assert_eq!(ring.dropped, 0, "drift {drift}");
        // On average the correction cancels the drift (a fast device needs more
        // frames); single blocks vary with the device's burst-wise pulls.
        let ppm = ppm_sum / 1_200.0;
        assert!(
            (ppm + drift * 1e6).abs() < 0.1 * drift.abs() * 1e6 + 10.0,
            "drift {drift}: correction {ppm} ppm"
        );
        // The fill stays near the target: within a device period and a block.
        let fill = c.fill as f64;
        assert!(
            (fill - r.target_frames()).abs() < 512.0 + 480.0,
            "drift {drift}: fill {fill}"
        );
        let peak = r.played[from..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.45, "drift {drift}: the tone did not play ({peak})");
        assert!(
            r.largest_step_after(from) < SMOOTH,
            "drift {drift}: discontinuity {}",
            r.largest_step_after(from)
        );
    }
}

#[test]
fn a_mono_graph_plays_on_both_channels_of_a_44_1_khz_device() {
    let config = AudioGraphConfig::new(
        AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Mono).unwrap(),
        480,
        rbs_audio::DEFAULT_LATENCY,
    )
    .unwrap();
    let mut r = Rig::new(
        config,
        Out {
            format: AudioFormat::new(SampleRate::HZ_44100, ChannelLayout::Stereo).unwrap(),
            drift: -1.5e-4,
            period: 441,
            ..Out::default()
        },
    );
    let (_, ring0, from) = settle(&mut r);
    r.run(500);
    let (c, ring) = (r.counters(), r.ring());
    assert_eq!(c.primes, 1);
    assert_eq!(ring.silence, ring0.silence);
    assert!(r.largest_step_after(from) < SMOOTH);
    // Played for 13 s at 44.1 kHz: the device got its rate, not the graph's.
    let secs = r.played.len() as f64 / 44_100.0 / (1.0 - 1.5e-4);
    assert!((secs - 13.0).abs() < 0.1, "{secs} s");
    // Both channels carry the tone at full level.
    let mut frame = vec![0.0f32; 2 * 441];
    r.side.consumer.lock().unwrap().pull(&mut frame);
    let peak = |c: usize| {
        frame
            .iter()
            .skip(c)
            .step_by(2)
            .fold(0.0f32, |m, v| m.max(v.abs()))
    };
    assert!(peak(0) > 0.3 && peak(1) > 0.3, "{} {}", peak(0), peak(1));
    assert!(frame.chunks_exact(2).all(|f| f[0] == f[1]));
}

#[test]
fn an_underrun_is_filled_once_and_playback_continues() {
    let mut r = Rig::new(AudioGraphConfig::default(), Out::default());
    settle(&mut r);
    // The graph stalls for 100 ms (longer than the ring's target): the device runs
    // dry, then the graph catches up with a burst of ticks.
    r.stall_graph(Duration::from_millis(100));
    r.run(400);
    let (c1, ring1) = (r.counters(), r.ring());
    assert!(ring1.silence > 0);
    assert!(c1.primes >= 2, "{c1:?}");
    r.run(800);
    let (c, ring) = (r.counters(), r.ring());
    assert_eq!(c.primes, c1.primes, "steady again: no further underrun");
    assert_eq!(ring.silence, ring1.silence);
}

#[test]
fn a_stalled_device_is_caught_up_by_skipping_not_by_a_full_ring() {
    let mut r = Rig::new(AudioGraphConfig::default(), Out::default());
    settle(&mut r);
    // The device stops pulling for 150 ms while the graph runs on.
    let running = Arc::clone(&r.side);
    running.running.store(false, Ordering::Release);
    r.run(15);
    running.running.store(true, Ordering::Release);
    r.run(400);
    let c1 = r.counters();
    assert!(c1.skipped_frames > 0, "{c1:?}");
    assert_eq!(r.ring().dropped, 0);
    r.run(600);
    let c = r.counters();
    assert_eq!(c.skipped_frames, c1.skipped_frames, "back near the target");
    assert!((c.fill as f64 - r.target_frames()).abs() < 512.0 + 480.0);
}

#[test]
fn volume_changes_ramp_and_zero_is_silent() {
    let mut r = Rig::new(AudioGraphConfig::default(), Out::default());
    let (_, _, from) = settle(&mut r);
    r.params.send(OutputParams::with_volume(0.0));
    r.run(100);
    // Played until the ring's content made after the change reached the device.
    r.run(100);
    let n = r.played.len();
    let tail = &r.played[n - 4_800..];
    assert!(tail.iter().all(|v| *v == 0.0));
    // The fade had no step larger than the tone's own.
    assert!(r.largest_step_after(from) < SMOOTH);
}

#[test]
fn latency_adds_graph_ring_target_and_device() {
    let r = Rig::new(AudioGraphConfig::default(), Out::default());
    assert_eq!(
        r.latency,
        rbs_audio::DEFAULT_LATENCY + DEFAULT_OUTPUT_TARGET + Duration::from_millis(10)
    );
}

#[test]
fn process_does_not_allocate() {
    let mut r = Rig::new(AudioGraphConfig::default(), Out::default());
    settle(&mut r);
    let mut total = 0;
    for _ in 0..300 {
        r.step(|exec, tick| {
            total += allocation_counter::measure(|| exec.run_tick(tick)).count_total;
        });
    }
    assert_eq!(total, 0, "input or output node allocated");
}
