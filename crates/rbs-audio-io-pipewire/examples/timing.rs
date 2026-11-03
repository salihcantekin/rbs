//! Prints how long `enumerate`, `open_input` and `start` take against the running
//! daemon (budgets: 100 ms, -, 500 ms). Manual measurement aid.
fn main() {
    use std::sync::Arc;
    use std::time::Instant;

    use rbs_audio_io::{AudioBackend, EndpointKind, InputRequest};
    use rbs_core::time::MonotonicClock;

    let backend = rbs_audio_io_pipewire::PipeWireAudioBackend::new();
    for kind in EndpointKind::ALL {
        let t = Instant::now();
        let endpoints = backend.enumerate(kind);
        println!(
            "enumerate {kind}: {:?} {:?}",
            t.elapsed(),
            endpoints.as_ref().map(Vec::len)
        );
        let Ok(endpoints) = endpoints else { continue };
        for endpoint in endpoints {
            let request = InputRequest::new(endpoint.id.clone(), Arc::new(MonotonicClock::new()));
            let t = Instant::now();
            let Ok(mut stream) = backend.open_input(&request) else {
                continue;
            };
            let open = t.elapsed();
            let t = Instant::now();
            let started = stream.start();
            println!(
                "  {}: open {open:?}, start {:?} {started:?}",
                endpoint.id,
                t.elapsed()
            );
        }
    }
}
