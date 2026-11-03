//! Runs the audio I/O port conformance suite against the PipeWire adapter.
//!
//! The test publishes one endpoint of each kind from its own child process (see
//! `common/fixture.rs`): a microphone, a null sink whose monitor is read, and an
//! application playing into an unlinked stream. It needs a running PipeWire daemon with
//! a session manager (WirePlumber) to link microphone and monitor streams; without a
//! daemon socket it only checks probing, unless `RBS_REQUIRE_PIPEWIRE` is set (CI
//! starts both and sets it, and then every case must run: strict mode).
//!
//! After the suite, the test checks the content of what the microphone and the
//! application deliver. The file holds a single test that runs by default, so the process's thread and fd
//! counts belong to the adapter alone.

#[cfg(target_os = "linux")]
#[path = "common/fixture.rs"]
mod fixture;

/// Allocations plus frees made by `f` on this thread.
#[cfg(target_os = "linux")]
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

/// A fixture process started by `passes_conformance`; does nothing on its own.
#[test]
#[ignore = "started by passes_conformance as a fixture process"]
fn fixture_process() {
    #[cfg(target_os = "linux")]
    if let Some(role) = std::env::var_os(fixture::SERVE) {
        fixture::serve(&role.to_string_lossy());
    }
}

fn required() -> bool {
    std::env::var_os("RBS_REQUIRE_PIPEWIRE").is_some_and(|v| !v.is_empty())
}

#[test]
fn passes_conformance() {
    use rbs_audio_io::conformance::{self, Options, RealTimePacer};
    use rbs_audio_io_pipewire::PipeWireAudioBackend;

    let backend = PipeWireAudioBackend::new();
    if !rbs_audio_io_pipewire::daemon_present() {
        assert!(
            !required(),
            "RBS_REQUIRE_PIPEWIRE is set but no PipeWire daemon socket was found"
        );
        eprintln!("no PipeWire daemon: only probing is checked");
        let report = conformance::run(&backend, &mut RealTimePacer::new(), &Options::default());
        eprintln!("{report}");
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let hooks = Hooks {
            fixtures: ["mic", "sink", "app"]
                .map(|role| fixture::Fixture::start(role, "fixture_process")),
        };
        let options = Options {
            measure_allocations: Some(measure),
            hooks: Some(&hooks),
            strict: required(),
            ..Options::default()
        };
        let report = conformance::run(&backend, &mut RealTimePacer::new(), &options);
        eprintln!("{report}");
        report.assert_ran(&[
            "probe",
            "enumerate",
            "unknown endpoint",
            "format",
            "standby",
            "buffer mismatch",
            "delivery",
            "timestamps",
            "no allocation",
            "overflow",
            "accounting",
            "stop",
            "reads",
            "restart",
            "drop",
            "endpoint lost",
        ]);
        // The suite checks timing and accounting, not content: check that the audio
        // delivered is the audio played, which also proves the application's ports
        // were linked.
        delivers_what_was_played();
    }
}

/// The fixtures play a 0.25 amplitude sine on every channel; the microphone is mono
/// and applications are captured in stereo.
#[cfg(target_os = "linux")]
fn delivers_what_was_played() {
    let _mic = fixture::Fixture::start("mic", "fixture_process");
    let _app = fixture::Fixture::start("app", "fixture_process");
    let mic = peaks(&format!("source:{}", fixture::MIC));
    assert_eq!(mic.len(), 1, "the microphone is mono");
    assert!(mic[0] > 0.2 && mic[0] <= 0.26, "microphone peaks {mic:?}");
    let app = peaks(&format!("app:{}", fixture::APP));
    assert_eq!(app.len(), 2, "applications are captured in stereo");
    assert!(
        app.iter().all(|p| *p > 0.2 && *p <= 0.26),
        "application peaks {app:?}"
    );
}

/// The peak of each channel over 200 ms of audio from `endpoint`.
#[cfg(target_os = "linux")]
fn peaks(endpoint: &str) -> Vec<f32> {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rbs_audio_io::{AudioBackend, EndpointId, InputRequest};
    use rbs_core::time::MonotonicClock;

    let backend = rbs_audio_io_pipewire::PipeWireAudioBackend::new();
    let request = InputRequest::new(EndpointId::new(endpoint), Arc::new(MonotonicClock::new()));
    let mut stream = match backend.open_input(&request) {
        Ok(stream) => stream,
        Err(e) => panic!("open {endpoint}: {e}"),
    };
    if let Err(e) = stream.start() {
        panic!("start {endpoint}: {e}");
    }
    let format = stream.format();
    let channels = format.channels.channels();
    let wanted = format.sample_rate.hz() as usize / 5;
    let mut buffers = vec![vec![0.0_f32; 1_024]; channels];
    let mut peaks = vec![0.0_f32; channels];
    let mut frames = 0;
    // Hang guard only (ADR-0032): audio normally arrives within one graph cycle.
    let guard = Instant::now() + Duration::from_secs(5);
    while frames < wanted {
        assert!(
            Instant::now() < guard,
            "{endpoint}: only {frames} frames in 5 s"
        );
        let mut slices: Vec<&mut [f32]> = buffers.iter_mut().map(Vec::as_mut_slice).collect();
        let read = match stream.read(&mut slices) {
            Ok(read) => read,
            Err(e) => panic!("read {endpoint}: {e}"),
        };
        if read.frames == 0 {
            std::thread::yield_now();
            continue;
        }
        for (peak, buffer) in peaks.iter_mut().zip(&buffers) {
            *peak = buffer[..read.frames]
                .iter()
                .fold(*peak, |p, s| p.max(s.abs()));
        }
        frames += read.frames;
    }
    stream.stop();
    peaks
}

#[cfg(target_os = "linux")]
struct Hooks {
    fixtures: [fixture::Fixture; 3],
}

#[cfg(target_os = "linux")]
impl rbs_audio_io::conformance::TestHooks for Hooks {
    fn remove(&self, endpoint: &rbs_audio_io::EndpointId) -> bool {
        let role = match endpoint.as_str() {
            id if id == format!("source:{}", fixture::MIC) => "mic",
            id if id == format!("monitor:{}", fixture::SINK) => "sink",
            id if id == format!("app:{}", fixture::APP) => "app",
            _ => return false,
        };
        self.fixtures
            .iter()
            .find(|f| f.role() == role)
            .is_some_and(fixture::Fixture::remove)
    }

    /// Threads (other than exiting ones) plus open file descriptors other than
    /// shared-memory buffers: daemon connections, event and timer fds. PipeWire maps
    /// buffer memory as memfds that the daemon may recycle, so those are not counted.
    fn resources(&self) -> Option<u64> {
        let fds = std::fs::read_dir("/proc/self/fd")
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| {
                std::fs::read_link(entry.path())
                    .is_ok_and(|target| !target.to_string_lossy().starts_with("/memfd:"))
            })
            .count();
        let threads = std::fs::read_dir("/proc/self/task")
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| !thread_exiting(&entry.path()))
            .count();
        Some((fds + threads) as u64)
    }
}

/// Whether a thread is exiting (`PF_EXITING` in its `stat` flags) or gone: a joined
/// thread can stay listed for a moment after `pthread_join` returns.
#[cfg(target_os = "linux")]
fn thread_exiting(task: &std::path::Path) -> bool {
    const PF_EXITING: u64 = 0x4;
    let Ok(stat) = std::fs::read_to_string(task.join("stat")) else {
        return true;
    };
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(6))
        .and_then(|flags| flags.parse::<u64>().ok())
        .is_some_and(|flags| flags & PF_EXITING != 0)
}
