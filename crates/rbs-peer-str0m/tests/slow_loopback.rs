//! The port's conformance suite over real UDP sockets on this machine (no STUN).
//! Slow because it opens sockets and threads and runs in real time, so it is ignored
//! by default (ADR-0032); the simulated tests in `src/sim_tests.rs` cover the same
//! paths deterministically.

use std::time::{Duration, Instant};

use rbs_peer::SideSettings;
use rbs_peer::conformance::{Harness, run_all};
use rbs_peer_str0m::WebRtcPeerFactory;

/// Hang guard only: the condition is what the suite waits for.
const HANG_GUARD: Duration = Duration::from_secs(30);

fn wait(what: &str, done: &mut dyn FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < HANG_GUARD, "hang guard: {what}");
        // Yield the core to the pairing threads; the loop re-checks the condition.
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
#[ignore = "slow: real sockets and threads"]
fn slow_conformance_over_loopback() {
    run_all(&Harness {
        factory: &WebRtcPeerFactory,
        side: SideSettings::with_stun_servers(Vec::new()),
        wait: &wait,
    });
}
