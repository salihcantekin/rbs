//! Runs the `rbs-transport` conformance suite against the SRT transport, with a
//! local SRT listener as the destination.

mod support;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rbs_transport::conformance::{self, Destination};
use rbs_transport_srt::SrtTransportFactory;
use support::{Behaviour, TestServer};

#[derive(Default)]
struct LocalListener {
    current: Option<TestServer>,
    stalled: Vec<TestServer>,
}

impl Destination for LocalListener {
    fn url(&mut self) -> String {
        let server = TestServer::start(Behaviour::default());
        let url = server.url();
        self.current = Some(server);
        url
    }

    /// A listener that completes the handshake, announces a 16-packet flow window and
    /// never acknowledges anything: the sender can put at most 16 packets in flight.
    fn stalled_url(&mut self) -> Option<String> {
        let server = TestServer::start(Behaviour {
            flow_window: 16,
            gate: Some(Arc::new(AtomicBool::new(true))),
            ..Behaviour::default()
        });
        let url = server.url();
        self.stalled.push(server);
        Some(url)
    }

    fn received(&mut self) -> Option<Vec<u8>> {
        let sessions = self.current.take()?.finish();
        Some(
            sessions
                .first()
                .map(|s| s.payload.clone())
                .unwrap_or_default(),
        )
    }
}

#[test]
fn passes_the_transport_suite() {
    let mut destination = LocalListener::default();
    let report = conformance::transport(&SrtTransportFactory, &mut destination);
    println!("{report}");
    report.assert_passed();
    report.assert_ran(&[
        "container.mpegts/send-before-start",
        "container.mpegts/wrong-input",
        "container.mpegts/delivery",
        "container.mpegts/secret-redaction",
        "container.mpegts/never-blocks",
        "container.mpegts/secret-redaction-stalled",
    ]);
}
