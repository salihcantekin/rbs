//! Cost per block of the audio encoder side (hot path: encoder worker, every 10 ms):
//! the audio queue's reader and the encoder node on their partition, with a session
//! that takes the block and emits nothing, so the numbers are the graph's own cost.
//! The writer runs on its own executor outside the measured loop's partition.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benches fail by panicking.

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio::{AudioGraphConfig, EncoderNode};
use rbs_codec::{AudioEncoder, EncodeError, EncoderChange};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    AudioBlock, AudioFormat, BitstreamFormat, Bytes, CodecId, EncodedPacket, StreamDescriptor,
    StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::partition::Lowering;
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel};

const AUDIO: PartitionId = PartitionId(10);
const ENCODER: PartitionId = PartitionId(11);

/// Takes blocks, emits nothing.
struct Null(StreamDescriptor);

impl AudioEncoder for Null {
    fn stream(&self) -> &StreamDescriptor {
        &self.0
    }
    fn frame_size(&self) -> u32 {
        1024
    }
    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError> {
        std::hint::black_box(block.frames());
        Ok(())
    }
    fn drain(&mut self, _: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        Ok(())
    }
    fn reconfigure(&mut self, _: &EncoderChange) -> Result<(), EncodeError> {
        Ok(())
    }
    fn flush(&mut self, _: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        Ok(())
    }
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
        let time = ctx.tick().time;
        let frames = self.1[0].len();
        let mut out = ctx.outputs().audio(0)?;
        for (c, dst) in out.channels_mut(2).enumerate() {
            dst[..frames].copy_from_slice(&self.1[c]);
        }
        out.commit(AudioFormat::STEREO_48K, time, frames)?;
        Ok(())
    }
}

/// Takes packets and does nothing (an encoder needs a sink to be scheduled).
struct Out(NodeDescriptor);

impl Node<()> for Out {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        std::hint::black_box(ctx.inputs().packets(0).count());
        Ok(())
    }
}

fn active(g: &mut GraphDescription, d: &NodeDescriptor, p: PartitionId) -> NodeId {
    let id = g.add_node(Arc::new(d.clone())).unwrap();
    g.set_state(id, Lifecycle::Standby).unwrap();
    g.set_state(id, Lifecycle::Active).unwrap();
    g.set_partition(id, p).unwrap();
    id
}

fn encoder_side(c: &mut Criterion) {
    let config = AudioGraphConfig::default();
    let frames = config.block_frames as usize;
    let tone: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
    let block = Block(
        NodeDescriptor::new(
            NodeTypeId::new("org.rbs.bench.block"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", LinkKind::Audio)],
        ),
        [tone.clone(), tone],
    );
    let enc = EncoderNode::new(
        Box::new(Null(StreamDescriptor::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            Bytes::new(),
            StreamParams::audio(AudioFormat::STEREO_48K, 1024),
        ))),
        64,
    );
    let out = Out(NodeDescriptor::new(
        NodeTypeId::new("org.rbs.bench.packet-out"),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Packets)],
        vec![],
    ));
    let mut g = GraphDescription::new();
    let b = active(&mut g, &block.0, AUDIO);
    let e = active(&mut g, Node::<()>::descriptor(&enc), ENCODER);
    let o = active(&mut g, &out.0, ENCODER);
    g.connect(b, "out", e, "in").unwrap();
    g.connect(e, "packets", o, "in").unwrap();

    let mut lowering = Lowering::new();
    lowering.set_audio_queue(50, config.block_frames);
    let bell = lowering.doorbell(ENCODER);
    let mut lowered = lowering.lower(&g);
    assert!(lowered.problems.is_empty());
    let mut audio = channel::<()>(Arc::new(()), ());
    let mut encoder = channel::<()>(Arc::new(()), ());
    audio.0.set_audio_block_frames(config.block_frames);
    encoder.0.set_audio_block_frames(config.block_frames);
    audio.0.adopt(b, Box::new(block)).unwrap();
    encoder.0.adopt(e, Box::new(enc)).unwrap();
    encoder.0.adopt(o, Box::new(out)).unwrap();
    let new = lowered.created.pop().unwrap();
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
        exec.swap_pending();
        ctl.collect().unwrap().unwrap();
    }
    let cadence = config.cadence(Timestamp::ZERO);
    let mut i = 0u64;
    // One block written, then the reader and the encoder node take it: the measured
    // part is the encoder partition's tick (the writer's tick is included, as on the
    // audio thread it runs elsewhere; `audio_mixer_block` shows its share).
    c.bench_function(
        "audio_encoder_block/queue_reader_and_encoder_node",
        |bench| {
            bench.iter(|| {
                audio.1.run_tick(Tick::of(&cadence, i));
                encoder.1.run_tick(Tick::new(i, cadence.time_of(i)));
                i += 1;
            })
        },
    );
    std::hint::black_box(bell.wait(std::time::Duration::ZERO));
}

criterion_group!(benches, encoder_side);
criterion_main!(benches);
