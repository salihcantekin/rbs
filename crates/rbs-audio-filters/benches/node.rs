//! Tick cost of an audio filter node in a real executor: a source -> Gain -> sink, 48 kHz
//! stereo, 480 frames (ADR-0130 decision 3 changed `AudioFilterNode::process`; P15).
//!
//! Run: `cargo bench -p rbs-audio-filters --bench node`.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio_filters::gain::Gain;
use rbs_audio_filters::{AudioFilterNode, AudioFilterType, DspSetup};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::AudioFormat;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, Rational};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel};
use rbs_params::{Value, Values};

const FRAMES: usize = 480;

struct Tone(NodeDescriptor);

impl Node<()> for Tone {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let t = ctx.tick().time;
        let mut out = ctx.outputs().audio(0)?;
        for ch in out.channels_mut(2) {
            ch[..FRAMES].fill(0.1);
        }
        out.commit(AudioFormat::STEREO_48K, t, FRAMES)?;
        Ok(())
    }
}

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

fn active(g: &mut GraphDescription, d: &NodeDescriptor) -> NodeId {
    let id = g.add_node(Arc::new(d.clone())).expect("add");
    g.set_state(id, Lifecycle::Standby).expect("standby");
    g.set_state(id, Lifecycle::Active).expect("active");
    id
}

fn node_bench(c: &mut Criterion) {
    let setup = DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32);
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    let mut graph = GraphDescription::new();
    let tone = Tone(NodeDescriptor::new(
        NodeTypeId::new("org.rbs.bench.tone"),
        NodeKind::Source,
        vec![],
        vec![Pin::one("out", LinkKind::Audio)],
    ));
    let sink = Sink(NodeDescriptor::new(
        NodeTypeId::new("org.rbs.bench.sink"),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Audio)],
        vec![],
    ));
    let values = Values::from_pairs(
        &Gain as &dyn AudioFilterType,
        [("gain", Value::Number(-6.0))],
    )
    .expect("valid");
    let (filter, _handles) = AudioFilterNode::new(&Gain, setup, values).expect("valid");
    let s = active(&mut graph, &tone.0);
    let f = active(&mut graph, Node::<()>::descriptor(&filter));
    let k = active(&mut graph, &sink.0);
    graph.connect(s, "out", f, "in").expect("link");
    graph.connect(f, "out", k, "in").expect("link");
    ctl.adopt(s, Box::new(tone)).expect("adopt");
    ctl.adopt(f, Box::new(filter)).expect("adopt");
    ctl.adopt(k, Box::new(sink)).expect("adopt");
    ctl.apply(&graph).expect("apply");
    exec.swap_pending();
    ctl.collect().expect("collect").expect("collected");
    let cadence = Cadence::new(Timestamp::ZERO, Rational::new(100, 1).expect("rate"));
    let mut tick = 5u64;
    c.bench_function("node_tone_gain_sink_tick", |b| {
        b.iter(|| {
            exec.run_tick(Tick::of(&cadence, tick));
            tick += 1;
        })
    });
}

criterion_group!(benches, node_bench);
criterion_main!(benches);
