//! Parameter channels: latest-value delivery to running nodes (ADR-0023).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use rbs_graph::runtime::param;

#[test]
fn the_receiver_starts_with_the_initial_value() {
    let (tx, mut rx) = param::channel(7_u32);
    assert_eq!(*rx.get(), 7);
    assert!(!rx.update());
    assert!(!tx.pending());
}

#[test]
fn only_the_newest_value_is_delivered() {
    let (mut tx, mut rx) = param::channel(0_u32);
    tx.send(1);
    tx.send(2);
    tx.send(3);
    assert!(tx.pending());
    assert!(rx.update());
    assert_eq!(*rx.get(), 3);
    assert!(!tx.pending());
    assert!(!rx.update(), "no new value, no change");
    assert_eq!(*rx.get(), 3);
}

#[test]
fn replaced_values_go_back_to_the_sender_for_reuse() {
    let (mut tx, mut rx) = param::channel(Vec::with_capacity(16));
    assert!(tx.recycle().is_none());
    tx.send(vec![1_u8, 2, 3]);
    assert!(rx.update());
    // The initial value (capacity 16) is now unused and comes back to the sender.
    let mut reused: Vec<u8> = tx.recycle().expect("the replaced value");
    assert!(reused.capacity() >= 16);
    assert!(tx.recycle().is_none(), "handed out once");
    reused.clear();
    reused.extend_from_slice(&[9, 9]);
    tx.send(reused);
    assert!(rx.update());
    assert_eq!(rx.get(), &[9, 9]);
    assert_eq!(tx.recycle().expect("previous value"), vec![1, 2, 3]);
}

#[test]
fn update_neither_allocates_nor_frees() {
    let (mut tx, mut rx) = param::channel(vec![0_u64; 64]);
    tx.send(vec![1; 64]);
    let first = allocation_counter::measure(|| {
        assert!(rx.update());
    });
    assert_eq!(first.count_total, 0, "update allocated");
    assert_eq!(first.count_current, 0, "update freed a value");

    // Steady state: the sender refills recycled buffers, the receiver swaps them in.
    for round in 2..100_u64 {
        let mut next = tx.recycle().unwrap_or_default();
        next.clear();
        next.resize(64, round);
        tx.send(next);
        let info = allocation_counter::measure(|| {
            assert!(rx.update());
            assert!(!rx.update());
        });
        assert_eq!(info.count_total, 0);
        assert_eq!(info.count_current, 0);
        assert_eq!(rx.get()[0], round);
    }
}

/// A value that records whether it was dropped on the receiving thread.
struct Tracked {
    value: u64,
    receiver_thread: Arc<AtomicBool>,
}

thread_local! {
    static ON_RECEIVER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if ON_RECEIVER.with(|r| r.get()) {
            self.receiver_thread.store(true, Ordering::Relaxed);
        }
    }
}

#[test]
fn values_from_another_thread_arrive_in_order_and_are_never_dropped_by_the_receiver() {
    let dropped_on_receiver = Arc::new(AtomicBool::new(false));
    let (mut tx, rx) = param::channel(Tracked {
        value: 0,
        receiver_thread: Arc::clone(&dropped_on_receiver),
    });
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let done = Arc::clone(&done);
        thread::spawn(move || {
            ON_RECEIVER.with(|r| r.set(true));
            let mut rx = rx;
            let mut last = 0;
            let mut changes = 0_u64;
            loop {
                let finished = done.load(Ordering::Acquire);
                if rx.update() {
                    let v = rx.get().value;
                    assert!(v > last, "values arrive in send order");
                    last = v;
                    changes += 1;
                }
                if finished && !rx.update() {
                    break;
                }
                thread::yield_now();
            }
            ON_RECEIVER.with(|r| r.set(false));
            (rx.get().value, changes, rx)
        })
    };
    for v in 1..=20_000 {
        tx.send(Tracked {
            value: v,
            receiver_thread: Arc::clone(&dropped_on_receiver),
        });
    }
    // The last value is taken even if the reader was busy when it was sent.
    while tx.pending() {
        thread::yield_now();
    }
    done.store(true, Ordering::Release);
    let (last, changes, rx) = reader.join().expect("reader");
    assert_eq!(last, 20_000);
    assert!(changes >= 1);
    assert!(
        !dropped_on_receiver.load(Ordering::Relaxed),
        "a value was dropped on the receiving thread"
    );
    assert!(!tx.is_closed());
    drop(rx);
    assert!(tx.is_closed());
}

mod in_a_running_graph {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rbs_core::lifecycle::Lifecycle;
    use rbs_core::time::Timestamp;
    use rbs_core::timing::{Cadence, FrameRate};
    use rbs_graph::description::GraphDescription;
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
    use rbs_graph::runtime::param::{self, ParamReceiver};
    use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel};

    /// A trivial sink so the plan schedules the source.
    struct Sink(NodeDescriptor);

    impl Node<()> for Sink {
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

    /// A source that publishes the parameter value it saw on each tick.
    struct Gain {
        desc: NodeDescriptor,
        gain: ParamReceiver<u64>,
        seen: Arc<AtomicU64>,
    }

    impl Node<()> for Gain {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.desc
        }
        fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
        fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
        fn process(&mut self, _: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
            self.gain.update();
            self.seen.store(*self.gain.get(), Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn a_node_sees_a_new_value_on_the_next_tick_without_a_new_schedule() {
        let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
        let desc = NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.gain"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", LinkKind::Video)],
        );
        let sink_desc = NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.sink"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        );
        let mut graph = GraphDescription::new();
        let id = graph.add_node(Arc::new(desc.clone())).expect("node");
        let sink = graph.add_node(Arc::new(sink_desc.clone())).expect("node");
        for n in [id, sink] {
            graph.set_state(n, Lifecycle::Standby).expect("standby");
            graph.set_state(n, Lifecycle::Active).expect("active");
        }
        graph.connect(id, "out", sink, "in").expect("link");
        ctl.adopt(sink, Box::new(Sink(sink_desc))).expect("adopt");
        let (mut tx, gain) = param::channel(1_u64);
        let seen = Arc::new(AtomicU64::new(0));
        ctl.adopt(
            id,
            Box::new(Gain {
                desc,
                gain,
                seen: Arc::clone(&seen),
            }),
        )
        .expect("adopt");
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        ctl.apply(&graph).expect("apply");
        exec.run_tick(Tick::of(&cadence, 0));
        ctl.collect().expect("executor").expect("swapped");
        exec.run_tick(Tick::of(&cadence, 1));
        assert_eq!(seen.load(Ordering::Relaxed), 1);

        tx.send(5);
        exec.run_tick(Tick::of(&cadence, 2));
        assert_eq!(seen.load(Ordering::Relaxed), 5);
        assert!(!ctl.in_flight(), "no schedule was sent");

        ctl.drain().expect("drain");
        exec.run_tick(Tick::of(&cadence, 3));
        ctl.collect().expect("executor").expect("swapped");
        // The node (and its receiver) is idle on the control side, so the channel
        // stays open until the instance is taken and dropped.
        assert!(!tx.is_closed());
        drop(ctl.take(id).expect("idle"));
        assert!(tx.is_closed());
    }
}
