//! Cost per block of the audio graph's nodes (hot path: audio thread, every 10 ms).
//!
//! - `input`: one input node on the reference adapter, 48 kHz mono and 44.1 kHz stereo
//!   sources into the 48 kHz stereo graph (read, steer, resample, map channels).
//! - `input_split`: the 48 kHz to 48 kHz sinc (`coefficients` + `apply`) and the
//!   mono-to-stereo channel-matrix dot, each over one 480-frame block.
//! - `mixer`: a mixer of 16 stereo inputs with a meter on every strip and the output,
//!   without and with the monitor output read (4 strips monitored).
//! - `output`: one output node from the 48 kHz stereo graph to a 48 kHz and a 44.1 kHz
//!   stereo device (FIFO, steer, resample, map channels, volume, ring push); the
//!   bench pulls the ring like a device, outside the node.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benches fail by panicking.

use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rbs_audio::{
    AudioGraphConfig, DEFAULT_OUTPUT_TARGET, InputNode, LevelMeter, MixerNode, MixerParams,
    MonitorMode, OutputNode, Strip,
};
use rbs_audio_io::{
    AudioBackend, AudioIoError, AudioOutput, EndpointId, InputRequest, OutputConsumer,
    OutputProducer, OutputStats,
};
use rbs_audio_io_test::{LOOPBACK, MONO_INPUT, TestAudioBackend};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::AudioFormat;
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel};

/// Takes the block and does nothing.
struct Sink(NodeDescriptor);

impl Node<()> for Sink {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        std::hint::black_box(ctx.inputs().audio(0, 0).map(|b| b.frames()));
        Ok(())
    }
}

fn sink() -> Sink {
    Sink(NodeDescriptor::new(
        NodeTypeId::new("org.rbs.bench.sink"),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Audio)],
        vec![],
    ))
}

/// Writes a pre-filled stereo block.
struct Block(NodeDescriptor, [Vec<f32>; 2]);

impl Node<()> for Block {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let Some(t) = AudioGraphConfig::default().block_time(ctx.tick()) else {
            return Ok(());
        };
        let mut out = ctx.outputs().audio(0)?;
        for (dst, src) in out.channels_mut(2).zip(&self.1) {
            dst[..480].copy_from_slice(src);
        }
        out.commit(AudioFormat::STEREO_48K, t, 480)?;
        Ok(())
    }
}

/// A stream that delivers 10 ms of a fixed signal at each 10 ms of the clock, at the
/// device's own pace, so the bench measures the node and not signal generation.
struct Cheap {
    format: AudioFormat,
    clock: TestClock,
    next: u64,
    signal: Vec<f32>,
}

impl rbs_audio_io::AudioInputStream for Cheap {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn start(&mut self) -> Result<(), rbs_audio_io::AudioIoError> {
        Ok(())
    }
    fn stop(&mut self) {}
    fn read(
        &mut self,
        out: &mut [&mut [f32]],
    ) -> Result<rbs_audio_io::AudioRead, rbs_audio_io::AudioIoError> {
        let rate = u64::from(self.format.sample_rate.hz());
        let packet = rate / 100;
        let t = |k: u64| Timestamp::from_nanos(k * 1_000_000_000 / rate);
        if self.clock.now() < t(self.next + packet) {
            return Ok(rbs_audio_io::AudioRead::EMPTY);
        }
        let n = (packet as usize).min(out[0].len());
        for ch in out.iter_mut() {
            ch[..n].copy_from_slice(&self.signal[..n]);
        }
        let read = rbs_audio_io::AudioRead::new(n, t(self.next), self.next == 0);
        self.next += n as u64;
        Ok(read)
    }
    fn available(&self) -> usize {
        0
    }
    fn stats(&self) -> rbs_audio_io::InputStats {
        rbs_audio_io::InputStats::default()
    }
}

fn active(g: &mut GraphDescription, d: &NodeDescriptor) -> NodeId {
    let id = g.add_node(Arc::new(d.clone())).unwrap();
    g.set_state(id, Lifecycle::Standby).unwrap();
    g.set_state(id, Lifecycle::Active).unwrap();
    id
}

fn input(c: &mut Criterion) {
    let mut group = c.benchmark_group("audio_input_block");
    let cheap = |rate, layout, clock: &TestClock| -> Box<dyn rbs_audio_io::AudioInputStream> {
        Box::new(Cheap {
            format: AudioFormat::new(rate, layout).unwrap(),
            clock: clock.clone(),
            next: 0,
            signal: (0..1000).map(|f| (f as f32 * 0.05).sin() * 0.3).collect(),
        })
    };
    use rbs_core::media::ChannelLayout;
    use rbs_core::timing::SampleRate;
    for case in [
        "adapter_48k_mono",
        "adapter_44k1_stereo",
        "node_48k_mono",
        "node_44k1_stereo",
    ] {
        let config = AudioGraphConfig::default();
        let clock = TestClock::new();
        let stream = match case {
            "adapter_48k_mono" | "adapter_44k1_stereo" => TestAudioBackend::new()
                .open_input(&InputRequest::new(
                    EndpointId::new(if case == "adapter_48k_mono" {
                        MONO_INPUT
                    } else {
                        LOOPBACK
                    }),
                    Arc::new(clock.clone()),
                ))
                .unwrap(),
            "node_48k_mono" => cheap(SampleRate::HZ_48000, ChannelLayout::Mono, &clock),
            _ => cheap(SampleRate::HZ_44100, ChannelLayout::Stereo, &clock),
        };
        let label = case;
        let (node, _, _) = InputNode::new(stream, config, Duration::ZERO);
        let s = sink();
        let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
        let mut g = GraphDescription::new();
        let a = active(&mut g, Node::<()>::descriptor(&node));
        let b = active(&mut g, &s.0);
        g.connect(a, "out", b, "in").unwrap();
        ctl.adopt(a, Box::new(node)).unwrap();
        ctl.adopt(b, Box::new(s)).unwrap();
        ctl.apply(&g).unwrap();
        exec.swap_pending();
        ctl.collect().unwrap().unwrap();
        let cadence = config.cadence(Timestamp::ZERO);
        let mut i = 0u64;
        // Settle past start-up.
        for _ in 0..100 {
            clock.advance(cadence.time_of(i + 1).saturating_since(clock.now()));
            exec.run_tick(Tick::of(&cadence, i));
            i += 1;
        }
        group.bench_function(BenchmarkId::new("source", label), |bench| {
            bench.iter(|| {
                clock.advance(cadence.time_of(i).saturating_since(clock.now()));
                exec.run_tick(Tick::of(&cadence, i));
                i += 1;
            })
        });
    }
    group.finish();
}

fn mixer(c: &mut Criterion) {
    // `monitored` strips on the monitor (`Also`), its output read when non-zero;
    // the last `muted` strips muted.
    for (label, monitored, muted) in [
        ("16_stereo_inputs_with_meters", 0, 0),
        ("16_stereo_inputs_with_meters_4_monitored", 4, 0),
        ("16_stereo_inputs_with_meters_8_muted", 0, 8),
    ] {
        let config = AudioGraphConfig::default();
        let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
        let mut g = GraphDescription::new();
        let mut strips = Vec::new();
        let mut ids = Vec::new();
        for k in 0..16 {
            let tone: Vec<f32> = (0..480)
                .map(|f| ((f + k) as f32 * 0.01).sin() * 0.3)
                .collect();
            let node = Block(
                NodeDescriptor::new(
                    NodeTypeId::new("org.rbs.bench.block"),
                    NodeKind::Source,
                    vec![],
                    vec![Pin::one("out", LinkKind::Audio)],
                ),
                [tone.clone(), tone],
            );
            let id = active(&mut g, &node.0);
            ctl.adopt(id, Box::new(node)).unwrap();
            let mode = if k < monitored {
                MonitorMode::Also
            } else {
                MonitorMode::Off
            };
            strips.push(
                Strip::new(id, 0.7)
                    .with_meter(Arc::new(LevelMeter::new(2)))
                    .with_monitor(mode)
                    .with_muted(k >= 16 - muted),
            );
            ids.push(id);
        }
        let params = MixerParams::new(strips).with_master_meter(Arc::new(LevelMeter::new(2)));
        let (mixer, _, _) = MixerNode::new(config, 16, params);
        let m = active(&mut g, Node::<()>::descriptor(&mixer));
        let s = sink();
        let o = active(&mut g, &s.0);
        for id in ids {
            g.connect(id, "out", m, "in").unwrap();
        }
        g.connect(m, "main", o, "in").unwrap();
        ctl.adopt(m, Box::new(mixer)).unwrap();
        ctl.adopt(o, Box::new(s)).unwrap();
        if monitored > 0 {
            let s = sink();
            let mo = active(&mut g, &s.0);
            g.connect(m, "monitor", mo, "in").unwrap();
            ctl.adopt(mo, Box::new(s)).unwrap();
        }
        ctl.apply(&g).unwrap();
        exec.swap_pending();
        ctl.collect().unwrap().unwrap();
        let cadence = config.cadence(Timestamp::ZERO);
        let mut i = 5u64;
        c.bench_function(&format!("audio_mixer_block/{label}"), |b| {
            b.iter(|| {
                exec.run_tick(Tick::of(&cadence, i));
                i += 1;
            })
        });
    }
}

/// An output whose device side the bench plays.
struct Pulled {
    format: AudioFormat,
    producer: Option<OutputProducer>,
}

impl AudioOutput for Pulled {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn producer(&mut self) -> Option<OutputProducer> {
        self.producer.take()
    }
    fn start(&mut self) -> Result<(), AudioIoError> {
        Ok(())
    }
    fn stop(&mut self) {}
    fn latency_ns(&self) -> u64 {
        0
    }
    fn stats(&self) -> OutputStats {
        OutputStats::default()
    }
    fn status(&self) -> Result<(), AudioIoError> {
        Ok(())
    }
}

fn output(c: &mut Criterion) {
    use rbs_core::media::ChannelLayout;
    use rbs_core::timing::SampleRate;
    let mut group = c.benchmark_group("audio_output_block");
    for (label, rate) in [
        ("device_48k_stereo", SampleRate::HZ_48000),
        ("device_44k1_stereo", SampleRate::HZ_44100),
    ] {
        let config = AudioGraphConfig::default();
        let format = AudioFormat::new(rate, ChannelLayout::Stereo).unwrap();
        let (producer, mut consumer): (OutputProducer, OutputConsumer) =
            rbs_audio_io::output::channel(2, 16_384);
        let (node, _, _) = OutputNode::new(
            Box::new(Pulled {
                format,
                producer: Some(producer),
            }),
            config,
            DEFAULT_OUTPUT_TARGET,
        );
        let tone: Vec<f32> = (0..480).map(|f| (f as f32 * 0.01).sin() * 0.3).collect();
        let src = Block(
            NodeDescriptor::new(
                NodeTypeId::new("org.rbs.bench.block"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Audio)],
            ),
            [tone.clone(), tone],
        );
        let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
        let mut g = GraphDescription::new();
        let a = active(&mut g, &src.0);
        let b = active(&mut g, Node::<()>::descriptor(&node));
        g.connect(a, "out", b, "in").unwrap();
        ctl.adopt(a, Box::new(src)).unwrap();
        ctl.adopt(b, Box::new(node)).unwrap();
        ctl.apply(&g).unwrap();
        exec.swap_pending();
        ctl.collect().unwrap().unwrap();
        let cadence = config.cadence(Timestamp::ZERO);
        // The device's share of one block: 10 ms at its rate.
        let mut buf = vec![0.0f32; 2 * (rate.hz() as usize / 100)];
        let mut i = 5u64;
        for _ in 0..100 {
            exec.run_tick(Tick::of(&cadence, i));
            consumer.pull(&mut buf);
            i += 1;
        }
        group.bench_function(BenchmarkId::new("sink", label), |bench| {
            bench.iter(|| {
                exec.run_tick(Tick::of(&cadence, i));
                consumer.pull(&mut buf);
                i += 1;
            })
        });
    }
    group.finish();
}

/// The resampler and the channel-matrix dot of one 480-frame block, measured apart
/// from the input node's ring read and FIFO.
///
/// The sinc bench is the pair of calls in `InputAligner::render` for one mono
/// channel at a 1:1 rate: [`Kernel::coefficients`](rbs_audio::resample::Kernel::coefficients)
/// on the shared 48 kHz to 48 kHz kernel, then [`apply`](rbs_audio::resample::apply).
/// The matrix bench cannot call `input::matrix` (`pub(crate)`); it repeats that
/// function's mono-to-stereo weights and the dot loop in `render`.
fn input_split(c: &mut Criterion) {
    use rbs_audio::resample::{AFTER, BEFORE, Kernel, TAPS, apply};
    use rbs_core::media::MAX_CHANNELS;
    use rbs_core::timing::SampleRate;

    let frames = AudioGraphConfig::default().block_frames as usize;
    assert_eq!(frames, 480, "the published input block is 480 frames");

    let mut group = c.benchmark_group("audio_input_split");

    let kernel = Kernel::shared(SampleRate::HZ_48000, SampleRate::HZ_48000);
    let input_len = BEFORE + frames + AFTER;
    let input: Vec<f32> = (0..input_len)
        .map(|i| (i as f32 * 0.05).sin() * 0.3)
        .collect();
    let mut sinc_out = vec![0.0f32; frames];
    group.bench_function("sinc_48k_to_48k_mono", |bench| {
        bench.iter(|| {
            let mut weights = [0.0f32; TAPS];
            // The step is 1.0 (equal rates, no steering). One barrier per block keeps
            // the phase lookup in the frame loop; the outputs are black-boxed below.
            let step = std::hint::black_box(1.0f64);
            let mut p = BEFORE as f64;
            for frame in 0..frames {
                let i = p.floor();
                let at = i as usize - BEFORE;
                kernel.coefficients(p - i, &mut weights);
                sinc_out[frame] = apply(&weights, &input[at..at + TAPS]);
                p += step;
            }
            std::hint::black_box(sinc_out.as_slice());
        });
    });

    // Same weights as `matrix(ChannelLayout::Mono, ChannelLayout::Stereo)`.
    let mut matrix = [[0.0f32; MAX_CHANNELS]; MAX_CHANNELS];
    matrix[0][0] = 1.0;
    matrix[1][0] = 1.0;
    let src_channels = 1usize;
    let n_out = 2usize;
    let source: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    group.bench_function("matrix_mono_to_stereo", |bench| {
        bench.iter(|| {
            let src_channels = std::hint::black_box(src_channels);
            let n_out = std::hint::black_box(n_out);
            let matrix = std::hint::black_box(&matrix);
            {
                let mut chans: [&mut [f32]; 2] = [&mut left, &mut right];
                for frame in 0..frames {
                    let mut v = [0.0f32; MAX_CHANNELS];
                    for slot in v.iter_mut().take(src_channels) {
                        *slot = source[frame];
                    }
                    for (o, ch) in chans.iter_mut().enumerate().take(n_out) {
                        let row = &matrix[o];
                        let mut acc = 0.0f32;
                        for c in 0..src_channels {
                            acc += row[c] * v[c];
                        }
                        ch[frame] = acc;
                    }
                }
            }
            std::hint::black_box(left.as_slice());
            std::hint::black_box(right.as_slice());
        });
    });

    group.finish();
}

criterion_group!(benches, input, mixer, output, input_split);
criterion_main!(benches);
