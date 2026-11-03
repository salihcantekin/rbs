//! Restart with back-off and failure events (ADR-0024, principle P7).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    ApplyReport, Collected, Controller, Executor, FaultCause, JoinError, Node, NodeError,
    PrepareCtx, ProcessCtx, RestartPolicy, RuntimeEvent, Tick, channel,
};

/// What a test makes the flaky source do, and what it observed.
#[derive(Default)]
struct Script {
    /// `prepare` fails while this is above zero (and counts it down).
    prepare_failures: AtomicU32,
    /// `prepare` fails permanently.
    prepare_permanent: AtomicBool,
    /// `process` returns an error.
    process_fails: AtomicBool,
    /// `process` returns a permanent error.
    process_permanent: AtomicBool,
    prepares: AtomicU32,
    releases: AtomicU32,
}

struct Flaky {
    desc: NodeDescriptor,
    script: Arc<Script>,
}

impl Node<()> for Flaky {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        self.script.prepares.fetch_add(1, Ordering::Relaxed);
        if self.script.prepare_permanent.load(Ordering::Relaxed) {
            return Err(NodeError::Permanent("permission denied"));
        }
        let left = self.script.prepare_failures.load(Ordering::Relaxed);
        if left > 0 {
            self.script
                .prepare_failures
                .store(left - 1, Ordering::Relaxed);
            return Err(NodeError::Backend("device busy"));
        }
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {
        self.script.releases.fetch_add(1, Ordering::Relaxed);
    }
    fn process(&mut self, _: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        if self.script.process_permanent.load(Ordering::Relaxed) {
            return Err(NodeError::Permanent("device gone"));
        }
        if self.script.process_fails.load(Ordering::Relaxed) {
            return Err(NodeError::Backend("lost frame"));
        }
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
    fn process(&mut self, _: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
}

struct Rig {
    ctl: Controller<()>,
    exec: Executor<()>,
    graph: GraphDescription,
    clock: TestClock,
    cadence: Cadence,
    tick: u64,
    source: NodeId,
    script: Arc<Script>,
}

impl Rig {
    fn new() -> Self {
        let (mut ctl, exec) = channel::<()>(Arc::new(()), ());
        let clock = TestClock::new();
        ctl.set_clock(Arc::new(clock.clone()));
        let src = NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.flaky"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", LinkKind::Video)],
        );
        let out = NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.sink"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        );
        let mut graph = GraphDescription::new();
        let source = graph.add_node(Arc::new(src.clone())).expect("node");
        let sink = graph.add_node(Arc::new(out.clone())).expect("node");
        for n in [source, sink] {
            graph.set_state(n, Lifecycle::Standby).expect("standby");
            graph.set_state(n, Lifecycle::Active).expect("active");
        }
        graph.connect(source, "out", sink, "in").expect("link");
        let script = Arc::new(Script::default());
        ctl.adopt(
            source,
            Box::new(Flaky {
                desc: src,
                script: Arc::clone(&script),
            }),
        )
        .expect("adopt");
        ctl.adopt(sink, Box::new(Sink(out))).expect("adopt");
        Self {
            ctl,
            exec,
            graph,
            clock,
            cadence: Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60),
            tick: 0,
            source,
            script,
        }
    }

    fn run_tick(&mut self) {
        self.exec.run_tick(Tick::of(&self.cadence, self.tick));
        self.tick += 1;
    }

    /// `apply`, one tick, `collect`: one complete edit.
    fn edit(&mut self) -> (ApplyReport, Collected) {
        let report = self.ctl.apply(&self.graph).expect("apply");
        self.run_tick();
        let collected = self.ctl.collect().expect("executor").expect("swapped");
        (report, collected)
    }

    /// The source's instance is in the schedule (a node held without an instance
    /// while it waits is in the plan, but not running).
    fn scheduled(&self) -> bool {
        self.ctl.is_running(self.source)
    }

    fn now(&self) -> Timestamp {
        self.clock.now()
    }
}

fn failed_events(events: &[RuntimeEvent]) -> Vec<(u32, Option<Timestamp>)> {
    events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Failed {
                attempt, retry_at, ..
            } => Some((*attempt, *retry_at)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_failed_prepare_waits_for_its_back_off_instead_of_retrying_on_every_edit() {
    let mut rig = Rig::new();
    rig.script.prepare_failures.store(1, Ordering::Relaxed);
    let (report, _) = rig.edit();
    let at = rig.now() + Duration::from_millis(500);
    assert_eq!(failed_events(&report.events), vec![(1, Some(at))]);
    assert!(!rig.scheduled());

    // Edits during the wait do not call `prepare` again.
    rig.clock.advance(Duration::from_millis(499));
    assert!(!rig.ctl.restart_due());
    let (report, _) = rig.edit();
    assert_eq!(rig.script.prepares.load(Ordering::Relaxed), 1);
    assert!(
        report
            .failed
            .contains(&(rig.source, JoinError::BackingOff { retry_at: Some(at) }))
    );
    assert!(!rig.scheduled());

    // Once the wait is over the control loop sees it and re-applies.
    rig.clock.advance(Duration::from_millis(1));
    assert!(rig.ctl.restart_due());
    let (report, _) = rig.edit();
    assert_eq!(rig.script.prepares.load(Ordering::Relaxed), 2);
    assert!(report.events.contains(&RuntimeEvent::Restarted {
        node: rig.source,
        attempt: 1
    }));
    assert!(rig.scheduled());
    assert!(!rig.ctl.restart_due());
}

#[test]
fn the_wait_doubles_up_to_the_maximum() {
    let mut rig = Rig::new();
    rig.ctl.set_restart_policy(RestartPolicy::backoff(
        Duration::from_millis(100),
        Duration::from_millis(350),
    ));
    rig.script.prepare_failures.store(5, Ordering::Relaxed);
    let mut waits = Vec::new();
    for _ in 0..5 {
        let (report, _) = rig.edit();
        let (_, retry_at) = failed_events(&report.events)[0];
        let wait = retry_at.expect("retried").saturating_since(rig.now());
        waits.push(wait.as_millis());
        rig.clock.advance(wait);
    }
    assert_eq!(waits, vec![100, 200, 350, 350, 350]);
    let (report, _) = rig.edit();
    assert!(report.events.contains(&RuntimeEvent::Restarted {
        node: rig.source,
        attempt: 5
    }));
    assert!(rig.scheduled());
}

#[test]
fn a_bypassed_node_is_taken_out_released_and_restarted_without_an_edit() {
    let mut rig = Rig::new();
    rig.exec.set_failure_limit(2);
    rig.edit();
    assert!(rig.scheduled());
    assert!(!rig.ctl.restart_due());

    rig.script.process_fails.store(true, Ordering::Relaxed);
    rig.run_tick();
    rig.run_tick();
    // The executor bypassed it; the controller learns that without any edit.
    assert!(rig.ctl.restart_due());

    // Round 1: the swapped-out schedule reports the bypass.
    let (_, collected) = rig.edit();
    let failures = failed_events(&collected.events);
    assert_eq!(failures.len(), 1);
    assert!(matches!(
        collected.events[0],
        RuntimeEvent::Failed {
            cause: FaultCause::Bypassed(h),
            ..
        } if h.bypassed && h.consecutive_errors == 2
    ));
    assert!(
        rig.ctl.restart_due(),
        "it is still scheduled and must leave"
    );
    let releases = rig.script.releases.load(Ordering::Relaxed);

    // Round 2: it leaves the schedule and is released.
    let (report, collected) = rig.edit();
    assert!(matches!(
        report.failed.as_slice(),
        [(id, JoinError::BackingOff { retry_at: Some(_) })] if *id == rig.source
    ));
    assert!(collected.left.iter().any(|(id, _)| *id == rig.source));
    assert_eq!(rig.script.releases.load(Ordering::Relaxed), releases + 1);
    assert!(!rig.scheduled());
    assert!(!rig.ctl.restart_due());
    assert!(collected.events.is_empty(), "the bypass is counted once");

    // After the wait it is prepared and warmed again with fresh counters.
    rig.script.process_fails.store(false, Ordering::Relaxed);
    rig.clock.advance(Duration::from_millis(500));
    assert!(rig.ctl.restart_due());
    let (report, _) = rig.edit();
    assert!(report.events.contains(&RuntimeEvent::Restarted {
        node: rig.source,
        attempt: 1
    }));
    assert!(rig.scheduled());
    rig.run_tick();
    let health = rig.exec.health(rig.source).expect("scheduled");
    assert!(!health.bypassed);
    assert_eq!(health.processed, 1);
}

#[test]
fn a_node_that_runs_cleanly_long_enough_is_forgiven() {
    let mut rig = Rig::new();
    rig.script.prepare_failures.store(2, Ordering::Relaxed);
    rig.edit();
    rig.clock.advance(Duration::from_millis(500));
    rig.edit();
    rig.clock.advance(Duration::from_secs(1));
    rig.edit();
    assert!(rig.scheduled());
    assert_eq!(rig.ctl.fault(rig.source).map(|f| f.attempts), Some(2));

    rig.clock.advance(Duration::from_secs(10));
    // Forgiveness is due; the control loop sees it without an edit.
    assert!(rig.ctl.restart_due());
    let (_, collected) = rig.edit();
    assert!(
        collected
            .events
            .contains(&RuntimeEvent::Recovered { node: rig.source })
    );
    assert!(!rig.ctl.restart_due());
    assert_eq!(rig.ctl.fault(rig.source), None);

    // A later failure starts from the first wait again.
    rig.exec.set_failure_limit(1);
    rig.script.process_fails.store(true, Ordering::Relaxed);
    rig.run_tick();
    let (_, collected) = rig.edit();
    let at = rig.now() + Duration::from_millis(500);
    assert_eq!(failed_events(&collected.events), vec![(1, Some(at))]);
}

#[test]
fn a_permanent_prepare_error_waits_for_the_user() {
    let mut rig = Rig::new();
    rig.script.prepare_permanent.store(true, Ordering::Relaxed);
    let (report, _) = rig.edit();
    assert_eq!(failed_events(&report.events), vec![(1, None)]);
    assert_eq!(rig.ctl.next_due(), None);

    rig.clock.advance(Duration::from_secs(3600));
    assert!(!rig.ctl.restart_due());
    rig.edit();
    assert_eq!(rig.script.prepares.load(Ordering::Relaxed), 1);

    // The user fixed the permission and retries.
    rig.script.prepare_permanent.store(false, Ordering::Relaxed);
    assert!(rig.ctl.retry(rig.source));
    rig.edit();
    assert_eq!(rig.script.prepares.load(Ordering::Relaxed), 2);
    assert!(rig.scheduled());
}

#[test]
fn a_permanent_process_error_bypasses_at_once_and_is_not_restarted() {
    let mut rig = Rig::new();
    rig.edit();
    rig.script.process_permanent.store(true, Ordering::Relaxed);
    rig.run_tick();
    let health = rig.exec.health(rig.source).expect("scheduled");
    assert!(health.bypassed && health.permanent);
    assert_eq!(health.errors, 1);

    let (_, collected) = rig.edit();
    assert_eq!(failed_events(&collected.events), vec![(1, None)]);
    rig.edit();
    assert!(!rig.scheduled());
    rig.clock.advance(Duration::from_secs(3600));
    assert!(!rig.ctl.restart_due());
}

#[test]
fn the_policy_can_give_up() {
    let mut rig = Rig::new();
    rig.ctl
        .set_restart_policy(RestartPolicy::default().give_up_after(2));
    rig.script.prepare_failures.store(10, Ordering::Relaxed);
    let (report, _) = rig.edit();
    assert!(failed_events(&report.events)[0].1.is_some());
    rig.clock.advance(Duration::from_millis(500));
    let (report, _) = rig.edit();
    assert_eq!(failed_events(&report.events), vec![(2, None)]);
    rig.clock.advance(Duration::from_secs(3600));
    assert!(!rig.ctl.restart_due());
}

#[test]
fn switching_a_failed_node_off_and_on_retries_it_at_once() {
    let mut rig = Rig::new();
    rig.script.prepare_failures.store(1, Ordering::Relaxed);
    rig.edit();
    assert!(rig.ctl.fault(rig.source).is_some());

    rig.graph
        .set_state(rig.source, Lifecycle::Standby)
        .expect("standby");
    rig.edit();
    assert_eq!(rig.ctl.fault(rig.source), None);
    rig.graph
        .set_state(rig.source, Lifecycle::Active)
        .expect("active");
    rig.edit();
    assert_eq!(rig.script.prepares.load(Ordering::Relaxed), 2);
    assert!(rig.scheduled());
}

#[test]
fn a_failed_warm_up_is_reported_and_restarted() {
    struct ColdNode(NodeDescriptor, Arc<AtomicU32>);
    impl Node<()> for ColdNode {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.0
        }
        fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
        fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
        fn warm(&mut self, _: &mut rbs_graph::runtime::WarmCtx<'_, ()>) -> Result<(), NodeError> {
            if self.1.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(NodeError::NotReady)
            } else {
                Ok(())
            }
        }
        fn process(&mut self, _: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
    }
    let mut rig = Rig::new();
    let desc = NodeDescriptor::new(
        NodeTypeId::new("org.rbs.test.cold"),
        NodeKind::Source,
        vec![],
        vec![Pin::one("out", LinkKind::Video)],
    );
    let id = rig.graph.add_node(Arc::new(desc.clone())).expect("node");
    rig.graph
        .set_state(id, Lifecycle::Standby)
        .expect("standby");
    rig.graph.set_state(id, Lifecycle::Active).expect("active");
    let out = NodeDescriptor::new(
        NodeTypeId::new("org.rbs.test.sink"),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Video)],
        vec![],
    );
    let sink = rig.graph.add_node(Arc::new(out.clone())).expect("node");
    rig.graph
        .set_state(sink, Lifecycle::Standby)
        .expect("standby");
    rig.graph
        .set_state(sink, Lifecycle::Active)
        .expect("active");
    rig.graph.connect(id, "out", sink, "in").expect("link");
    rig.ctl.adopt(sink, Box::new(Sink(out))).expect("adopt");
    let warms = Arc::new(AtomicU32::new(0));
    rig.ctl
        .adopt(id, Box::new(ColdNode(desc, Arc::clone(&warms))))
        .expect("adopt");

    rig.edit(); // warm fails on the swap
    assert!(
        rig.ctl.restart_due(),
        "a failed warm-up is noticed without an edit"
    );
    let (_, collected) = rig.edit();
    assert!(collected.events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Failed { node, cause: FaultCause::Bypassed(h), .. }
            if *node == id && h.warm_failed
    )));
    rig.edit(); // taken out
    rig.clock.advance(Duration::from_millis(500));
    let (report, _) = rig.edit();
    assert!(report.events.contains(&RuntimeEvent::Restarted {
        node: id,
        attempt: 1
    }));
    rig.run_tick();
    assert!(!rig.exec.health(id).expect("scheduled").bypassed);
    assert_eq!(warms.load(Ordering::Relaxed), 2);
}

#[test]
fn switching_off_and_on_retries_a_node_that_failed_while_scheduled() {
    let mut rig = Rig::new();
    rig.edit();
    rig.script.process_permanent.store(true, Ordering::Relaxed);
    rig.run_tick();
    let (_, collected) = rig.edit();
    assert_eq!(failed_events(&collected.events), vec![(1, None)]);

    // Off while the failed instance is still scheduled: its leaving health is stale.
    rig.graph
        .set_state(rig.source, Lifecycle::Standby)
        .expect("standby");
    let (report, collected) = rig.edit();
    assert!(
        report
            .events
            .contains(&RuntimeEvent::Cleared { node: rig.source })
    );
    assert!(failed_events(&collected.events).is_empty());
    assert_eq!(rig.ctl.fault(rig.source), None);

    rig.script.process_permanent.store(false, Ordering::Relaxed);
    rig.graph
        .set_state(rig.source, Lifecycle::Active)
        .expect("active");
    rig.edit();
    assert!(rig.scheduled());
    rig.run_tick();
    assert!(!rig.exec.health(rig.source).expect("scheduled").bypassed);
}

#[test]
fn retry_while_the_failed_instance_is_scheduled_or_in_flight_is_not_undone() {
    let mut rig = Rig::new();
    rig.edit();
    rig.script.process_permanent.store(true, Ordering::Relaxed);
    rig.run_tick();
    rig.edit(); // records the permanent failure
    assert!(rig.ctl.fault(rig.source).is_some());

    // Retry while the take-out schedule is in flight.
    rig.ctl.apply(&rig.graph).expect("apply");
    rig.script.process_permanent.store(false, Ordering::Relaxed);
    assert!(rig.ctl.retry(rig.source));
    rig.run_tick();
    let collected = rig.ctl.collect().expect("executor").expect("swapped");
    assert!(
        failed_events(&collected.events).is_empty(),
        "stale health ignored"
    );
    assert!(rig.ctl.restart_due());
    rig.edit();
    assert!(rig.scheduled());
    assert_eq!(rig.ctl.fault(rig.source), None);

    // Retry while the bypassed instance is still scheduled: it is taken out, then
    // prepared again.
    rig.script.process_permanent.store(true, Ordering::Relaxed);
    rig.run_tick();
    rig.edit();
    rig.script.process_permanent.store(false, Ordering::Relaxed);
    let prepares = rig.script.prepares.load(Ordering::Relaxed);
    assert!(rig.ctl.retry(rig.source));
    assert!(rig.ctl.restart_due());
    rig.edit(); // taken out and released
    assert!(!rig.scheduled());
    assert!(rig.ctl.restart_due());
    rig.edit(); // prepared again
    assert!(rig.scheduled());
    assert_eq!(rig.script.prepares.load(Ordering::Relaxed), prepares + 1);
    assert!(!rig.ctl.restart_due());
}

#[test]
fn a_node_the_plan_skips_after_its_wait_does_not_keep_the_loop_busy() {
    let mut rig = Rig::new();
    rig.script.prepare_failures.store(1, Ordering::Relaxed);
    rig.edit();
    // The user switches off the only output: the source has no sink any more.
    let sink = rig.graph.nodes()[1].id;
    rig.graph
        .set_state(sink, Lifecycle::Standby)
        .expect("standby");
    rig.edit();
    rig.clock.advance(Duration::from_secs(1));
    assert!(rig.ctl.restart_due());
    rig.edit();
    assert!(!rig.ctl.restart_due(), "parked until an edit schedules it");
    assert_eq!(rig.ctl.next_due(), None);
    // Switching the output back on schedules and prepares it.
    rig.graph
        .set_state(sink, Lifecycle::Active)
        .expect("active");
    rig.edit();
    assert!(rig.scheduled());
}

#[test]
fn a_flapping_node_escalates_even_when_the_loop_is_late() {
    let mut rig = Rig::new();
    rig.exec.set_failure_limit(1);
    rig.edit();
    for expected in 1..=3_u32 {
        rig.script.process_fails.store(true, Ordering::Relaxed);
        rig.run_tick();
        // The control loop only gets to it much later than `stable_after`.
        rig.clock.advance(Duration::from_secs(60));
        let (_, collected) = rig.edit();
        assert!(
            !collected
                .events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::Recovered { .. })),
            "a bypassed node is never forgiven"
        );
        assert_eq!(failed_events(&collected.events)[0].0, expected);
        rig.script.process_fails.store(false, Ordering::Relaxed);
        rig.edit(); // taken out
        rig.clock.advance(Duration::from_secs(60));
        rig.edit(); // restarted
        assert!(rig.scheduled());
    }
}

#[test]
fn a_failure_stays_contained_to_its_node_in_a_chain() {
    // source -> filter -> output: a failing filter neither releases its source nor
    // its output (P7).
    let mut rig = Rig::new();
    let sink = rig.graph.nodes()[1].id;
    let filter_desc = NodeDescriptor::new(
        NodeTypeId::new("org.rbs.test.filter"),
        NodeKind::VideoFilter,
        vec![Pin::one("in", LinkKind::Video)],
        vec![Pin::one("out", LinkKind::Video)],
    );
    let filter_script = Arc::new(Script::default());
    let filter = rig
        .graph
        .add_node(Arc::new(filter_desc.clone()))
        .expect("node");
    rig.graph
        .set_state(filter, Lifecycle::Standby)
        .expect("standby");
    rig.graph
        .set_state(filter, Lifecycle::Active)
        .expect("active");
    let link = rig
        .graph
        .links()
        .iter()
        .copied()
        .find(|l| l.from.node == rig.source)
        .expect("link");
    assert!(rig.graph.disconnect(link));
    rig.graph
        .connect(rig.source, "out", filter, "in")
        .expect("link");
    rig.graph.connect(filter, "out", sink, "in").expect("link");
    rig.ctl
        .adopt(
            filter,
            Box::new(Flaky {
                desc: filter_desc,
                script: Arc::clone(&filter_script),
            }),
        )
        .expect("adopt");
    rig.edit();
    assert!(rig.ctl.is_running(filter));

    filter_script.process_fails.store(true, Ordering::Relaxed);
    rig.exec.set_failure_limit(1);
    rig.run_tick();
    rig.edit();
    let (report, collected) = rig.edit();
    assert!(!rig.ctl.is_running(filter));
    assert!(report.plan.order.contains(&filter), "held in the plan");
    assert!(rig.scheduled() && rig.ctl.is_running(sink));
    assert_eq!(rig.script.releases.load(Ordering::Relaxed), 0);
    assert_eq!(
        collected.left.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![filter]
    );
}

#[test]
fn removing_a_failed_node_drops_its_record() {
    let mut rig = Rig::new();
    rig.script.prepare_permanent.store(true, Ordering::Relaxed);
    rig.edit();
    assert_eq!(rig.ctl.faulted(), vec![rig.source]);
    rig.graph
        .set_state(rig.source, Lifecycle::Standby)
        .expect("standby");
    rig.graph.remove_node(rig.source).expect("remove");
    let (report, _) = rig.edit();
    assert!(
        report
            .events
            .contains(&RuntimeEvent::Cleared { node: rig.source })
    );
    assert!(rig.ctl.faulted().is_empty());
}

#[test]
fn a_permanent_warm_error_is_not_restarted() {
    struct NoPermission(NodeDescriptor);
    impl Node<()> for NoPermission {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.0
        }
        fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
        fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
        fn warm(&mut self, _: &mut rbs_graph::runtime::WarmCtx<'_, ()>) -> Result<(), NodeError> {
            Err(NodeError::Permanent("screen recording not allowed"))
        }
        fn process(&mut self, _: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }
    }
    let (mut ctl, mut exec) = channel::<()>(Arc::new(()), ());
    let clock = TestClock::new();
    ctl.set_clock(Arc::new(clock.clone()));
    let src = NodeDescriptor::new(
        NodeTypeId::new("org.rbs.test.screen"),
        NodeKind::Source,
        vec![],
        vec![Pin::one("out", LinkKind::Video)],
    );
    let out = NodeDescriptor::new(
        NodeTypeId::new("org.rbs.test.sink"),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Video)],
        vec![],
    );
    let mut graph = GraphDescription::new();
    let a = graph.add_node(Arc::new(src.clone())).expect("node");
    let b = graph.add_node(Arc::new(out.clone())).expect("node");
    for n in [a, b] {
        graph.set_state(n, Lifecycle::Standby).expect("standby");
        graph.set_state(n, Lifecycle::Active).expect("active");
    }
    graph.connect(a, "out", b, "in").expect("link");
    ctl.adopt(a, Box::new(NoPermission(src))).expect("adopt");
    ctl.adopt(b, Box::new(Sink(out))).expect("adopt");
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    let mut tick = 0;
    let mut edit = |ctl: &mut Controller<()>, exec: &mut Executor<()>| {
        ctl.apply(&graph).expect("apply");
        exec.run_tick(Tick::of(&cadence, tick));
        tick += 1;
        ctl.collect().expect("executor").expect("swapped")
    };
    edit(&mut ctl, &mut exec);
    let collected = edit(&mut ctl, &mut exec);
    assert!(collected.events.iter().any(|e| matches!(
        e,
        RuntimeEvent::Failed { cause: FaultCause::Bypassed(h), retry_at: None, .. }
            if h.permanent && h.last_error == Some("screen recording not allowed")
    )));
    clock.advance(Duration::from_secs(3600));
    edit(&mut ctl, &mut exec);
    assert!(!ctl.restart_due());
}

#[test]
fn the_policy_gives_up_on_repeated_bypasses_too() {
    let mut rig = Rig::new();
    rig.ctl
        .set_restart_policy(RestartPolicy::default().give_up_after(2));
    rig.exec.set_failure_limit(1);
    rig.script.process_fails.store(true, Ordering::Relaxed);
    rig.edit();
    rig.run_tick();
    let (_, collected) = rig.edit();
    assert!(failed_events(&collected.events)[0].1.is_some());
    rig.edit();
    rig.clock.advance(Duration::from_secs(1));
    rig.edit();
    rig.run_tick();
    let (_, collected) = rig.edit();
    assert_eq!(failed_events(&collected.events), vec![(2, None)]);
}

#[test]
fn delay_saturates_at_the_maximum() {
    let p = RestartPolicy::backoff(Duration::from_millis(500), Duration::from_secs(30));
    assert_eq!(p.delay(1), Duration::from_millis(500));
    assert_eq!(p.delay(2), Duration::from_secs(1));
    assert_eq!(p.delay(7), Duration::from_secs(30));
    assert_eq!(p.delay(u32::MAX), Duration::from_secs(30));
    let huge = RestartPolicy::backoff(Duration::MAX, Duration::MAX);
    assert_eq!(huge.delay(40), Duration::MAX);
}
