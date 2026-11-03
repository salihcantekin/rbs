//! End-to-end check of the audio graph on real devices (ADR-0037): the default
//! microphone and the system loopback -> input nodes (sync, drift correction) ->
//! mixer with meters -> audio queue -> AAC encoder node, on the real engine workers.
//!
//! Windows: WASAPI + Media Foundation AAC. macOS: Core Audio + AudioToolbox AAC.
//!
//! ```sh
//! cargo run --release -p rbs-app --example audio_graph -- 20
//! ```
//!
//! Prints one line per second and a summary; exits with an error when the path is not
//! continuous (resyncs after start-up, read errors, queue losses, skipped audio ticks,
//! packet gaps).

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rbs_audio::{
    AudioGraphConfig, EncoderNode, InputCounters, InputNode, InputStats, LevelMeter, MixerNode,
    MixerParams, Strip,
};
use rbs_audio_io::{AudioBackend, AudioEndpoint, EndpointKind, InputRequest};
use rbs_codec::{AudioEncoderConfig, AudioEncoderFactory};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{AudioFormat, CodecId};
use rbs_core::time::Timestamp;
use rbs_engine::{Engine, Pacing};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx};

const AUDIO: PartitionId = PartitionId(1);
const ENCODER: PartitionId = PartitionId(2);

#[cfg(windows)]
fn platform() -> Option<(Box<dyn AudioBackend>, Box<dyn AudioEncoderFactory>)> {
    Some((
        Box::new(rbs_audio_io_wasapi::WasapiAudioBackend::new()),
        Box::new(rbs_codec_mf::MfAacFactory::new()),
    ))
}

#[cfg(target_os = "macos")]
fn platform() -> Option<(Box<dyn AudioBackend>, Box<dyn AudioEncoderFactory>)> {
    Some((
        Box::new(rbs_audio_io_coreaudio::CoreAudioBackend::new()),
        Box::new(rbs_codec_audiotoolbox::AacFactory::new()),
    ))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform() -> Option<(Box<dyn AudioBackend>, Box<dyn AudioEncoderFactory>)> {
    None
}

/// Counts packets and checks their timing: pts strictly increasing, one AAC frame
/// (1024 samples) apart.
struct PacketCheck {
    desc: NodeDescriptor,
    packets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    irregular: Arc<AtomicU64>,
    last: Option<Timestamp>,
}

impl Node<()> for PacketCheck {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        // 1024 frames at 48 kHz, with 1 ms tolerance for rounding.
        let frame = Duration::from_nanos(1024 * 1_000_000_000 / 48_000);
        for p in ctx.inputs().packets(0) {
            if let Some(last) = self.last {
                let step = p.pts().saturating_since(last);
                if p.pts() <= last || step.abs_diff(frame) > Duration::from_millis(1) {
                    self.irregular.fetch_add(1, Ordering::Relaxed);
                }
            }
            self.last = Some(p.pts());
            self.packets.fetch_add(1, Ordering::Relaxed);
            self.bytes
                .fetch_add(p.data().len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }
}

fn active(g: &mut GraphDescription, d: &NodeDescriptor, p: PartitionId) -> NodeId {
    let id = g.add_node(Arc::new(d.clone())).expect("add node");
    g.set_state(id, Lifecycle::Standby).expect("standby");
    g.set_state(id, Lifecycle::Active).expect("active");
    g.set_partition(id, p).expect("partition");
    id
}

fn pick(backend: &dyn AudioBackend, kind: EndpointKind) -> Option<AudioEndpoint> {
    let list = backend.enumerate(kind).ok()?;
    list.iter()
        .find(|e| e.is_default)
        .or_else(|| list.first())
        .cloned()
}

fn db(peak: f32) -> String {
    if peak <= 0.0 {
        "  -inf".into()
    } else {
        format!("{:6.1}", 20.0 * peak.log10())
    }
}

fn main() -> ExitCode {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let Some((backend, aac)) = platform() else {
        eprintln!("audio_graph runs on Windows and macOS only");
        return ExitCode::SUCCESS;
    };
    let config = AudioGraphConfig::default();

    let mut engine = Engine::new();
    engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
    let clock = engine.clock();

    // Inputs: default microphone and default loopback, whichever exist.
    let mut g = GraphDescription::new();
    let mut inputs: Vec<(String, NodeId, Arc<InputStats>, Arc<LevelMeter>)> = Vec::new();
    let mut nodes: Vec<(NodeId, InputNode)> = Vec::new();
    for kind in [EndpointKind::Input, EndpointKind::OutputLoopback] {
        let Some(ep) = pick(backend.as_ref(), kind) else {
            println!("no {} endpoint", kind.as_str());
            continue;
        };
        let request = InputRequest::new(ep.id.clone(), Arc::clone(&clock))
            .with_sample_rates(&[config.format.sample_rate]);
        let stream = match backend.open_input(&request) {
            Ok(s) => s,
            Err(e) => {
                println!("{}: cannot open {}: {e}", kind.as_str(), ep.name);
                continue;
            }
        };
        let f = stream.format();
        println!(
            "{}: {} ({} Hz, {} ch)",
            kind.as_str(),
            ep.name,
            f.sample_rate.hz(),
            f.channels.channels()
        );
        let (node, _offset, stats) = InputNode::new(stream, config, Duration::from_millis(200));
        let id = active(&mut g, Node::<()>::descriptor(&node), AUDIO);
        inputs.push((
            kind.as_str().to_owned(),
            id,
            stats,
            Arc::new(LevelMeter::new(2)),
        ));
        nodes.push((id, node));
    }
    if inputs.is_empty() {
        eprintln!("no audio input could be opened");
        return ExitCode::FAILURE;
    }

    let master = Arc::new(LevelMeter::new(2));
    let strips = inputs
        .iter()
        .map(|(_, id, _, m)| Strip::new(*id, 1.0).with_meter(Arc::clone(m)))
        .collect();
    let params = MixerParams::new(strips).with_master_meter(Arc::clone(&master));
    let (mixer, _mixer_params, mixer_stats) = MixerNode::new(config, 8, params);
    let mix_id = active(&mut g, Node::<()>::descriptor(&mixer), AUDIO);

    let encoder = match aac.create(&AudioEncoderConfig::new(
        CodecId::Aac,
        AudioFormat::STEREO_48K,
        160_000,
    )) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot create the AAC encoder: {e}");
            return ExitCode::FAILURE;
        }
    };
    let enc = EncoderNode::new(encoder, 64);
    let enc_id = active(&mut g, Node::<()>::descriptor(&enc), ENCODER);
    let packets = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let irregular = Arc::new(AtomicU64::new(0));
    let check = PacketCheck {
        desc: NodeDescriptor::new(
            NodeTypeId::new("org.rbs.example.packet-check"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Packets)],
            vec![],
        ),
        packets: Arc::clone(&packets),
        bytes: Arc::clone(&bytes),
        irregular: Arc::clone(&irregular),
        last: None,
    };
    let check_id = active(&mut g, &check.desc, ENCODER);
    for (_, id, _, _) in &inputs {
        g.connect(*id, "out", mix_id, "in").expect("connect input");
    }
    g.connect(mix_id, "main", enc_id, "in")
        .expect("connect mixer");
    g.connect(enc_id, "packets", check_id, "in")
        .expect("connect encoder");

    let origin = clock.now();
    engine
        .add_partition::<()>(
            AUDIO,
            Arc::new(()),
            (),
            Pacing::stream(config.cadence(origin), 4),
        )
        .expect("audio partition");
    engine
        .add_partition::<()>(
            ENCODER,
            Arc::new(()),
            (),
            Pacing::Doorbell {
                timer: Duration::from_millis(100),
            },
        )
        .expect("encoder partition");
    for (id, node) in nodes {
        engine
            .adopt::<()>(AUDIO, id, Box::new(node))
            .expect("adopt input");
    }
    engine
        .adopt::<()>(AUDIO, mix_id, Box::new(mixer))
        .expect("adopt mixer");
    engine
        .adopt::<()>(ENCODER, enc_id, Box::new(enc))
        .expect("adopt encoder");
    engine
        .adopt::<()>(ENCODER, check_id, Box::new(check))
        .expect("adopt check");
    let applied = match engine.apply(&g) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("apply failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    for (p, r) in &applied.reports {
        if !r.failed.is_empty() {
            eprintln!("partition {p:?}: nodes failed to prepare: {:?}", r.failed);
            return ExitCode::FAILURE;
        }
    }
    let queue = engine
        .crossings()
        .iter()
        .find(|c| c.from == AUDIO && c.to == ENCODER)
        .map(|c| Arc::clone(&c.stats));
    let audio_worker = engine.worker_stats(AUDIO).expect("audio worker");

    println!(
        "{:>4} {:>44} {:>14} {:>8} {:>6} {:>5}",
        "s", "per input: dB peak / ppm / late / resyncs", "master dB", "packets", "lost", "late"
    );
    let start = Instant::now();
    let mut settled: Vec<InputCounters> = Vec::new();
    for s in 1..=seconds {
        let until = start + Duration::from_secs(s);
        while Instant::now() < until {
            engine.service();
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut line = String::new();
        for (name, _, stats, meter) in &inputs {
            let c = stats.read();
            let l = meter.take();
            line.push_str(&format!(
                " {name}:{} {:+5}ppm {:>5} {}",
                db(l.peak(0).max(l.peak(1))),
                c.correction_ppm,
                c.late_frames,
                c.resyncs
            ));
        }
        let m = master.take();
        println!(
            "{s:>4} {line} {:>14} {:>8} {:>6} {:>5}",
            db(m.peak(0).max(m.peak(1))),
            packets.load(Ordering::Relaxed),
            queue.as_ref().map_or(0, |q| q.overflows()),
            audio_worker.late()
        );
        if s == 2 {
            settled = inputs.iter().map(|(_, _, st, _)| st.read()).collect();
        }
    }
    let late_ticks = audio_worker.late();
    let caught_up = audio_worker.caught_up();
    let grant = audio_worker.thread_grant();
    let _ = engine.shutdown();

    println!("--- summary after {seconds} s");
    let mut ok = true;
    for (i, (name, _, stats, _)) in inputs.iter().enumerate() {
        let c = stats.read();
        let base = settled.get(i).copied().unwrap_or_default();
        println!("{name}: {c:?}");
        if c.resyncs > base.resyncs || c.read_errors > 0 {
            println!("  FAIL: resyncs or read errors after start-up");
            ok = false;
        }
    }
    println!(
        "mixer: invalid inputs {}, unknown inputs {}",
        mixer_stats.invalid_inputs(),
        mixer_stats.unknown_inputs()
    );
    let expected = seconds * 48_000 / 1024;
    let got = packets.load(Ordering::Relaxed);
    println!(
        "encoder: {got} packets ({} kbit/s), {} irregular, about {expected} expected",
        bytes.load(Ordering::Relaxed) * 8 / 1000 / seconds.max(1),
        irregular.load(Ordering::Relaxed)
    );
    if let Some(q) = &queue {
        println!(
            "queue: sent {} received {} overflows {} filled {}",
            q.sent(),
            q.received(),
            q.overflows(),
            q.filled()
        );
        ok &= q.overflows() == 0;
    }
    println!("audio worker: late {late_ticks}, caught up {caught_up}, thread grant {grant:?}");
    ok &= late_ticks == 0 && irregular.load(Ordering::Relaxed) == 0;
    // Allow the latency and the encoder's priming at the start.
    ok &= got + 10 >= expected.saturating_sub(5);
    if ok {
        println!("PASS");
        ExitCode::SUCCESS
    } else {
        println!("FAIL");
        ExitCode::FAILURE
    }
}
