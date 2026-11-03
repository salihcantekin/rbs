//! Runs the `rbs-transport` conformance suite against the RTMP transport, with a
//! local RTMP server as the destination.

mod support;

use rbs_transport::conformance::{self, Destination};
use rbs_transport_rtmp::RtmpTransportFactory;
use support::{Behaviour, Stalled, TestServer};

#[derive(Default)]
struct LocalServer {
    current: Option<TestServer>,
    stalled: Vec<Stalled>,
}

impl Destination for LocalServer {
    fn url(&mut self) -> String {
        let server = TestServer::start(Behaviour::default());
        let url = server.url();
        self.current = Some(server);
        url
    }

    fn stalled_url(&mut self) -> Option<String> {
        let stalled = Stalled::new();
        let url = stalled.url();
        self.stalled.push(stalled);
        Some(url)
    }

    fn received(&mut self) -> Option<Vec<u8>> {
        let sessions = self.current.take()?.finish();
        Some(
            sessions
                .first()
                .map(support::Session::flv)
                .unwrap_or_default(),
        )
    }
}

#[test]
fn passes_the_transport_suite() {
    let mut destination = LocalServer::default();
    let report = conformance::transport(&RtmpTransportFactory, &mut destination);
    println!("{report}");
    report.assert_passed();
    report.assert_ran(&[
        "packets/send-before-start",
        "packets/wrong-input",
        "packets/secret-redaction",
        "packets/never-blocks",
        "packets/secret-redaction-stalled",
    ]);
}
