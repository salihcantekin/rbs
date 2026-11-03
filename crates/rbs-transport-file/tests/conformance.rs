//! Runs the `rbs-transport` conformance suite against the file transport.

use rbs_transport::conformance::{self, TempFileDestination};
use rbs_transport_file::FileTransportFactory;

#[test]
fn passes_the_transport_suite() {
    let mut destination = TempFileDestination::new("file-transport");
    let report = conformance::transport(&FileTransportFactory, &mut destination);
    println!("{report}");
    report.assert_passed();
    report.assert_ran(&[
        "container.mpegts/send-before-start",
        "container.mpegts/wrong-input",
        "container.mpegts/delivery",
        "container.mpegts/secret-redaction",
        "container.flv/delivery",
        "container.mp4.fragmented/delivery",
        "container.matroska/delivery",
    ]);
}
