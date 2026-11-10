//! Audio values on links and audio queues between partitions (ADR-0037, audio graph
//! spec "Audio values on links" and "Audio queue").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{AudioBlock, AudioFormat, ChannelLayout, DEFAULT_BLOCK_FRAMES};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, Rational, SampleRate};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::partition::Lowering;
use rbs_graph::runtime::queue::Doorbell;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, OutputError, PrepareCtx, ProcessCtx, Tick, channel,
};

const FRAMES: usize = DEFAULT_BLOCK_FRAMES as usize;

/// Blocks seen: time, frames, ch 0 first, ch 1 first, ch 1 last.
type Seen = Arc<Mutex<Vec<(Timestamp, usize, f32, f32, f32)>>>;

fn cadence() -> Cadence {
    Cadence::new(Timestamp::ZERO, Rational::new(100, 1).unwrap())
}

/// Writes a block per tick: channel `c`, frame `f` holds `tick * 1000 + c * 100 + f % 100`.
struct Tone {
    desc: NodeDescriptor,
    frames: usize,
    format: AudioFormat,
}

impl Tone {
    fn new(frames: usize) -> Self {
        Self {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.tone"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Audio)],
            ),
            frames,
            format: AudioFormat::STEREO_48K,
        }
    }
}

fn sample(tick: u64, c: usize, f: usize) -> f32 {
    (tick * 1000 + c as u64 * 100 + (f % 100) as u64) as f32
}

impl Node<()> for Tone {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        let mut out = ctx.outputs().audio(0)?;
        for (c, ch) in out.channels_mut(2).enumerate() {
            for (f, s) in ch[..self.frames].iter_mut().enumerate() {
                *s = sample(tick.index, c, f);
            }
        }
        out.commit(self.format, tick.time, self.frames)?;
        Ok(())
    }
}

/// Records `(timestamp, frames, first sample of each channel, last sample of ch 1)` of
/// every block on input 0, and counts silent ticks.
struct Sink {
    desc: NodeDescriptor,
    seen: Seen,
    silent: Arc<AtomicU64>,
}

impl Sink {
    fn new(kind: NodeKind) -> Self {
        let outputs = if kind == NodeKind::Encoder {
            vec![Pin::one("out", LinkKind::Packets)]
        } else {
            vec![]
        };
        Self {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.audio-sink"),
                kind,
                vec![Pin::one("in", LinkKind::Audio)],
                outputs,
            ),
            seen: Arc::default(),
            silent: Arc::default(),
        }
    }
}

impl Node<()> for Sink {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        match ctx.inputs().audio(0, 0) {
            Some(block) => {
                let a = block.channel(0).unwrap();
                let b = block.channel(1).unwrap();
                self.seen.lock().unwrap().push((
                    block.timestamp(),
                    block.frames(),
                    a[0],
                    b[0],
                    b[block.frames() - 1],
                ));
            }
            None => {
                self.silent.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

fn active(g: &mut GraphDescription, desc: &NodeDescriptor) -> NodeId {
    let id = g.add_node(Arc::new(desc.clone())).unwrap();
    g.set_state(id, Lifecycle::Standby).unwrap();
    g.set_state(id, Lifecycle::Active).unwrap();
    id
}

#[test]
fn audio_blocks_flow_inside_a_partition_and_slots_empty_each_tick() {
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    let mut g = GraphDescription::new();
    let tone = Tone::new(FRAMES);
    let sink = Sink::new(NodeKind::Output);
    let seen = Arc::clone(&sink.seen);
    let t = active(&mut g, &tone.desc);
    let s = active(&mut g, &sink.desc);
    g.connect(t, "out", s, "in").unwrap();
    ctl.adopt(t, Box::new(tone)).unwrap();
    ctl.adopt(s, Box::new(sink)).unwrap();
    ctl.apply(&g).expect("audio links are carried");
    let c = cadence();
    for i in 0..3 {
        exec.run_tick(Tick::of(&c, i));
    }
    ctl.collect().unwrap();
    let seen = seen.lock().unwrap().clone();
    // The swap happens after tick 0, so ticks 1 and 2 ran the graph.
    assert_eq!(seen.len(), 2);
    for (k, &(ts, frames, a, b, b_last)) in seen.iter().enumerate() {
        let i = k as u64 + 1;
        assert_eq!(ts, c.time_of(i));
        assert_eq!(frames, FRAMES);
        assert_eq!(a, sample(i, 0, 0));
        assert_eq!(b, sample(i, 1, 0));
        assert_eq!(b_last, sample(i, 1, FRAMES - 1));
    }
}

/// A producer that commits only on even ticks: the consumer sees no block (silence) on
/// odd ticks, not the previous block.
struct Sometimes(Tone);

impl Node<()> for Sometimes {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        if ctx.tick().index % 2 == 0 {
            self.0.process(ctx)
        } else {
            Ok(())
        }
    }
}

#[test]
fn a_block_is_an_event_not_state() {
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    let mut g = GraphDescription::new();
    let tone = Sometimes(Tone::new(FRAMES));
    let sink = Sink::new(NodeKind::Output);
    let (seen, silent) = (Arc::clone(&sink.seen), Arc::clone(&sink.silent));
    let t = active(&mut g, &tone.0.desc);
    let s = active(&mut g, &sink.desc);
    g.connect(t, "out", s, "in").unwrap();
    ctl.adopt(t, Box::new(tone)).unwrap();
    ctl.adopt(s, Box::new(sink)).unwrap();
    ctl.apply(&g).unwrap();
    let c = cadence();
    for i in 0..9 {
        exec.run_tick(Tick::of(&c, i));
    }
    assert_eq!(seen.lock().unwrap().len(), 4);
    assert_eq!(silent.load(Ordering::Relaxed), 4);
}

#[test]
fn commits_larger_than_the_pin_buffer_are_refused() {
    struct Big(Tone);
    impl Node<()> for Big {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.0.desc
        }
        fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
        fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
        fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
            let mut out = ctx.outputs().audio(0)?;
            assert_eq!(out.capacity(), 64);
            assert!(out.channel_mut(8).is_none());
            let err = out.commit(AudioFormat::STEREO_48K, Timestamp::ZERO, 65);
            assert!(matches!(
                err,
                Err(OutputError::AudioTooLarge {
                    pin: 0,
                    capacity: 64
                })
            ));
            out.commit(AudioFormat::STEREO_48K, Timestamp::ZERO, 64)?;
            // Asking a video pin's API for an audio pin is a kind error.
            assert!(matches!(
                ctx.outputs().set_video(0, None),
                Err(OutputError::WrongKind { .. })
            ));
            Ok(())
        }
    }
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    ctl.set_audio_block_frames(64);
    assert_eq!(ctl.audio_block_frames(), 64);
    let mut g = GraphDescription::new();
    let big = Big(Tone::new(64));
    let sink = Sink::new(NodeKind::Output);
    let seen = Arc::clone(&sink.seen);
    let t = active(&mut g, &big.0.desc);
    let s = active(&mut g, &sink.desc);
    g.connect(t, "out", s, "in").unwrap();
    ctl.adopt(t, Box::new(big)).unwrap();
    ctl.adopt(s, Box::new(sink)).unwrap();
    ctl.apply(&g).unwrap();
    for i in 0..3 {
        exec.run_tick(Tick::of(&cadence(), i));
    }
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(seen.lock().unwrap().iter().all(|s| s.1 == 64));
}

const AUDIO: PartitionId = PartitionId(5);
const ENCODER: PartitionId = PartitionId(6);

struct Split {
    lowering: Lowering,
    audio: (Controller<()>, Executor<()>),
    encoder: (Controller<()>, Executor<()>),
    seen: Seen,
    stats: Arc<rbs_graph::runtime::queue::QueueStats>,
    bell: Arc<Doorbell>,
    encoder_part: GraphDescription,
}

fn split(blocks: usize) -> Split {
    let mut g = GraphDescription::new();
    let tone = Tone::new(FRAMES);
    let sink = Sink::new(NodeKind::Encoder);
    let seen = Arc::clone(&sink.seen);
    let t = active(&mut g, &tone.desc);
    let s = active(&mut g, &sink.desc);
    // An encoder needs a sink downstream to be scheduled.
    let out_desc = NodeDescriptor::new(
        NodeTypeId::new("org.rbs.test.packet-out"),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Packets)],
        vec![],
    );
    struct Out(NodeDescriptor);
    impl Node<()> for Out {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.0
        }
        fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
        fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
        fn process(&mut self, _: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
    }
    let o = active(&mut g, &out_desc);
    g.connect(t, "out", s, "in").unwrap();
    g.connect(s, "out", o, "in").unwrap();
    g.set_partition(t, AUDIO).unwrap();
    g.set_partition(s, ENCODER).unwrap();
    g.set_partition(o, ENCODER).unwrap();

    let mut lowering = Lowering::new();
    lowering.set_audio_queue(blocks, DEFAULT_BLOCK_FRAMES);
    let bell = lowering.doorbell(ENCODER);
    let mut lowered = lowering.lower(&g);
    assert!(lowered.problems.is_empty(), "{:?}", lowered.problems);
    assert_eq!(lowered.created.len(), 1);
    let mut audio = channel::<()>(Arc::new(()), ());
    let mut encoder = channel::<()>(Arc::new(()), ());
    audio.0.adopt(t, Box::new(tone)).unwrap();
    encoder.0.adopt(s, Box::new(sink)).unwrap();
    encoder.0.adopt(o, Box::new(Out(out_desc))).unwrap();
    let new = lowered.created.pop().unwrap();
    let stats = Arc::clone(&new.crossing.stats);
    let (w, r) = new.ends.writer::<()>();
    audio.0.adopt(new.crossing.writer, w).unwrap();
    encoder
        .0
        .adopt(new.crossing.reader, r.into_node::<()>())
        .unwrap();
    for (p, part) in &lowered.parts {
        let (ctl, exec) = if *p == AUDIO {
            &mut audio
        } else {
            &mut encoder
        };
        ctl.apply(part).unwrap();
        // Swap without running anything.
        exec.swap_pending();
        ctl.collect().unwrap().unwrap();
    }
    let encoder_part = lowered.parts[&ENCODER].clone();
    Split {
        encoder_part,
        lowering,
        audio,
        encoder,
        seen,
        stats,
        bell,
    }
}

#[test]
fn audio_crosses_partitions_once_and_in_order() {
    let mut rig = split(8);
    let c = cadence();
    for i in 0..5 {
        rig.audio.1.run_tick(Tick::of(&c, i));
    }
    assert!(rig.bell.wait(std::time::Duration::ZERO), "the writer rang");
    assert_eq!(rig.stats.sent(), 5);
    // The consumer gets one block per tick, in order, and keeps ringing its own
    // doorbell while more wait.
    for k in 0..5 {
        rig.encoder.1.run_tick(Tick::new(k, Timestamp::ZERO));
        assert_eq!(rig.bell.wait(std::time::Duration::ZERO), k < 4, "tick {k}");
    }
    rig.encoder.1.run_tick(Tick::new(5, Timestamp::ZERO));
    let seen = rig.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 5);
    for (i, &(ts, frames, a, b, b_last)) in seen.iter().enumerate() {
        let i = i as u64;
        assert_eq!((ts, frames), (c.time_of(i), FRAMES));
        assert_eq!(
            (a, b, b_last),
            (sample(i, 0, 0), sample(i, 1, 0), sample(i, 1, FRAMES - 1))
        );
    }
    assert_eq!(rig.stats.received(), 5);
    assert_eq!(rig.stats.overflows(), 0);
    let _ = &rig.lowering;
}

/// Runs final ticks as a draining doorbell worker does: one, then one more for each
/// ring. Returns how many ran.
fn final_ticks(rig: &mut Split, from: u64) -> u64 {
    let mut k = 0;
    loop {
        rig.encoder
            .1
            .run_tick(Tick::new(from + k, Timestamp::ZERO).draining());
        k += 1;
        if !rig.bell.wait(std::time::Duration::ZERO) {
            return k;
        }
        assert!(k < 64, "final ticks never end");
    }
}

#[test]
fn final_ticks_pass_every_queued_block_and_end_with_one_that_passes_none() {
    let mut rig = split(8);
    let c = cadence();
    for i in 0..3 {
        rig.audio.1.run_tick(Tick::of(&c, i));
    }
    while rig.bell.wait(std::time::Duration::ZERO) {}
    // Three final ticks pass the three blocks; the reader rings after the last one too,
    // so a fourth runs and passes nothing: the consumer sees its input end (ADR-0045).
    assert_eq!(final_ticks(&mut rig, 0), 4);
    assert_eq!(rig.seen.lock().unwrap().len(), 3);
    assert_eq!(rig.stats.received(), 3);
    // An empty queue: one final tick, which passes nothing and rings nothing.
    assert_eq!(final_ticks(&mut rig, 4), 1);
    assert_eq!(rig.seen.lock().unwrap().len(), 3);
}

#[test]
fn a_full_audio_queue_loses_blocks_and_counts_them() {
    let mut rig = split(3);
    let c = cadence();
    for i in 0..5 {
        rig.audio.1.run_tick(Tick::of(&c, i));
    }
    assert_eq!(rig.stats.sent(), 3);
    assert_eq!(rig.stats.overflows(), 2);
    for k in 0..4 {
        rig.encoder.1.run_tick(Tick::new(k, Timestamp::ZERO));
    }
    // The first three blocks arrive; the two lost ones are filled with silence at
    // their times before block 5, so the consumer's timeline has no gap.
    rig.audio.1.run_tick(Tick::of(&c, 5));
    for k in 4..8 {
        rig.encoder.1.run_tick(Tick::new(k, Timestamp::ZERO));
    }
    let seen = rig.seen.lock().unwrap().clone();
    let ts: Vec<Timestamp> = seen.iter().map(|s| s.0).collect();
    assert_eq!(ts, [0, 1, 2, 3, 4, 5].map(|i| c.time_of(i)));
    assert_eq!(
        (seen[3].2, seen[4].3),
        (0.0, 0.0),
        "filled blocks are silent"
    );
    assert_eq!(seen[5].2, sample(5, 0, 0));
    assert_eq!(rig.stats.filled(), 2);
    assert_eq!(rig.stats.received(), 4);
}

#[test]
fn a_released_reader_does_not_keep_ringing() {
    let mut rig = split(8);
    let c = cadence();
    for i in 0..3 {
        rig.audio.1.run_tick(Tick::of(&c, i));
    }
    // The encoder side lets everything go: the reader is released with blocks queued.
    rig.encoder.0.drain().unwrap();
    rig.encoder.1.run_tick(Tick::new(0, Timestamp::ZERO));
    rig.encoder.0.collect().unwrap().unwrap();
    // The drain tick passed one block on; the other two were discarded.
    assert_eq!(rig.stats.skipped(), 2);
    // The reader joins again and finds the queue empty: it does not ring, so its
    // worker does not keep ticking.
    rig.encoder.0.apply(&rig.encoder_part).unwrap();
    rig.encoder.1.swap_pending();
    rig.encoder.0.collect().unwrap().unwrap();
    while rig.bell.wait(std::time::Duration::ZERO) {}
    rig.encoder.1.run_tick(Tick::new(1, Timestamp::ZERO));
    assert!(!rig.bell.wait(std::time::Duration::ZERO));
    rig.audio.1.run_tick(Tick::of(&c, 3));
    assert!(rig.bell.wait(std::time::Duration::ZERO), "the writer rang");
    rig.encoder.1.run_tick(Tick::new(2, Timestamp::ZERO));
    assert!(!rig.bell.wait(std::time::Duration::ZERO));
}

#[test]
fn an_interleaved_block_written_to_an_output_reads_back_planar() {
    struct Inter(NodeDescriptor);
    impl Node<()> for Inter {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.0
        }
        fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
        fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
        fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
            let fmt = AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Stereo).unwrap();
            let samples = [1.0, -1.0, 2.0, -2.0, 3.0, -3.0];
            let block = AudioBlock::interleaved(fmt, Timestamp::from_nanos(7), &samples).unwrap();
            ctx.outputs().audio(0)?.write(&block)?;
            Ok(())
        }
    }
    let tone = Tone::new(3);
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    let mut g = GraphDescription::new();
    let sink = Sink::new(NodeKind::Output);
    let seen = Arc::clone(&sink.seen);
    let t = active(&mut g, &tone.desc);
    let s = active(&mut g, &sink.desc);
    g.connect(t, "out", s, "in").unwrap();
    ctl.adopt(t, Box::new(Inter(tone.desc.clone()))).unwrap();
    ctl.adopt(s, Box::new(sink)).unwrap();
    ctl.apply(&g).unwrap();
    exec.run_tick(Tick::of(&cadence(), 0));
    exec.run_tick(Tick::of(&cadence(), 1));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen, [(Timestamp::from_nanos(7), 3, 1.0, -1.0, -3.0)]);
}

#[test]
fn the_audio_path_does_not_allocate_per_tick() {
    let mut rig = split(8);
    let c = cadence();
    rig.audio.1.run_tick(Tick::of(&c, 0));
    rig.encoder.1.run_tick(Tick::new(0, Timestamp::ZERO));
    rig.seen.lock().unwrap().reserve(4096);
    let counts = allocation_counter::measure(|| {
        for i in 1..1_001 {
            rig.audio.1.run_tick(Tick::of(&c, i));
            rig.encoder.1.run_tick(Tick::new(i, Timestamp::ZERO));
        }
    });
    assert_eq!(counts.count_total, 0);
    assert_eq!(rig.stats.received(), 1_001);
}

#[test]
fn a_writer_whose_reader_is_gone_keeps_its_buffer_instead_of_freeing_it() {
    let Split {
        mut audio,
        encoder,
        stats,
        ..
    } = split(4);
    let c = cadence();
    audio.1.run_tick(Tick::of(&c, 0));
    // The encoder side goes away with its reader (and the queued block).
    drop(encoder);
    let counts = allocation_counter::measure(|| {
        for i in 1..20 {
            audio.1.run_tick(Tick::of(&c, i));
        }
    });
    // Nothing allocated or freed on the audio thread: the buffer taken from the free
    // list is kept as a spare and reused.
    assert_eq!(counts.count_total, 0);
    assert_eq!(counts.count_current, 0);
    assert_eq!(stats.sent(), 1);
}

/// Two audio outputs; records which of them a scheduled node reads, each tick.
struct Split2 {
    desc: NodeDescriptor,
    linked: Arc<Mutex<Vec<(bool, bool, bool)>>>,
    /// Instances prepared, to prove a link edit keeps the node.
    prepared: Arc<AtomicU64>,
}

impl Node<()> for Split2 {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        self.prepared.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let out = ctx.outputs();
        let seen = (out.is_linked(0), out.is_linked(1), out.is_linked(7));
        self.linked.lock().unwrap().push(seen);
        Ok(())
    }
}

#[test]
fn a_node_sees_which_outputs_are_read_and_a_link_edit_keeps_it() {
    let split = Split2 {
        desc: NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.split"),
            NodeKind::Source,
            vec![],
            vec![
                Pin::one("main", LinkKind::Audio),
                Pin::one("monitor", LinkKind::Audio),
            ],
        ),
        linked: Arc::default(),
        prepared: Arc::default(),
    };
    let (linked, prepared) = (Arc::clone(&split.linked), Arc::clone(&split.prepared));
    let (a, b) = (Sink::new(NodeKind::Output), Sink::new(NodeKind::Output));
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    let mut g = GraphDescription::new();
    let s = active(&mut g, &split.desc);
    let ia = active(&mut g, &a.desc);
    let ib = active(&mut g, &b.desc);
    g.connect(s, "main", ia, "in").unwrap();
    ctl.adopt(s, Box::new(split)).unwrap();
    ctl.adopt(ia, Box::new(a)).unwrap();
    ctl.adopt(ib, Box::new(b)).unwrap();
    let c = cadence();
    let mut tick = 0;
    let mut run = |exec: &mut Executor<()>| {
        exec.run_tick(Tick::of(&c, tick));
        tick += 1;
    };
    ctl.apply(&g).unwrap();
    exec.swap_pending();
    ctl.collect().unwrap().unwrap();
    run(&mut exec);
    g.connect(s, "monitor", ib, "in").unwrap();
    ctl.apply(&g).unwrap();
    exec.swap_pending();
    ctl.collect().unwrap().unwrap();
    run(&mut exec);
    assert_eq!(
        *linked.lock().unwrap(),
        vec![(true, false, false), (true, true, false)]
    );
    assert_eq!(prepared.load(Ordering::Relaxed), 1);
}
