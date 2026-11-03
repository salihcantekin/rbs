//! Runs the audio I/O port conformance suite against the WASAPI adapter.
//!
//! `probes_and_stays_idle` runs everywhere and fast: probing, enumeration and Standby
//! on whatever endpoints the machine has (CI runners have none). The full suite needs
//! real audio endpoints and real time, so it is the slow, hardware-gated
//! `slow_hardware_conformance` (`RBS_AUDIO_HW=1`, Windows only). It reads the default
//! microphone, the default output's loopback, and an application: a fixture child
//! process (this test binary again) that plays a quiet 440 Hz tone, which the suite
//! also removes (kills) for the `endpoint lost` case.
//!
//! ```text
//! RBS_AUDIO_HW=1 cargo test -p rbs-audio-io-wasapi --test conformance -- --ignored --nocapture
//! ```

/// Allocations plus frees made by `f` on this thread.
#[cfg(windows)]
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

/// Environment variable that makes this binary play the fixture tone.
#[cfg(windows)]
const FIXTURE: &str = "RBS_WASAPI_FIXTURE";

/// A fixture process started by `slow_hardware_conformance`; does nothing on its own.
#[test]
#[ignore = "started by slow_hardware_conformance as a fixture process"]
fn fixture_process() {
    #[cfg(windows)]
    if std::env::var_os(FIXTURE).is_some() {
        fixture::play_tone();
    }
}

#[test]
fn probes_and_stays_idle() {
    use std::sync::Arc;

    use rbs_audio_io::{AudioBackend, AudioIoError, EndpointId, EndpointKind, InputRequest};
    use rbs_audio_io_wasapi::{WasapiAudioBackend, owned_resources};
    use rbs_core::registry::AdapterFactory;
    use rbs_core::time::TestClock;

    let backend = WasapiAudioBackend::new();
    let clock = Arc::new(TestClock::new());
    let missing = InputRequest::new(EndpointId::new("capture:missing"), clock.clone());
    if cfg!(not(windows)) {
        assert!(backend.probe().is_err());
        assert!(matches!(
            backend.open_input(&missing),
            Err(AudioIoError::Unsupported(_))
        ));
        return;
    }
    assert!(backend.probe().is_ok());
    assert!(matches!(
        backend.open_input(&missing),
        Err(AudioIoError::EndpointNotFound)
    ));
    for kind in [EndpointKind::Input, EndpointKind::OutputLoopback] {
        let endpoints = backend.enumerate(kind);
        assert!(endpoints.is_ok(), "{kind}: {endpoints:?}");
        // Opening is Standby: nothing owned until `start`.
        if let Some(endpoint) = endpoints.ok().and_then(|e| e.into_iter().next()) {
            let stream = backend.open_input(&InputRequest::new(endpoint.id, clock.clone()));
            assert!(stream.is_ok());
            assert_eq!(owned_resources(), 0);
        }
    }
    assert_eq!(owned_resources(), 0);
}

#[test]
#[ignore = "slow: about 15 s of real audio through WASAPI on a machine with audio \
            endpoints; needs RBS_AUDIO_HW=1"]
fn slow_hardware_conformance() {
    if std::env::var_os("RBS_AUDIO_HW").is_none_or(|v| v.is_empty()) {
        eprintln!("RBS_AUDIO_HW is not set: hardware conformance skipped");
        return;
    }
    #[cfg(windows)]
    hardware::run();
    #[cfg(not(windows))]
    panic!("RBS_AUDIO_HW is set but WASAPI needs Windows");
}

#[cfg(windows)]
mod hardware {
    use std::process::{Child, Command};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use rbs_audio_io::conformance::{self, Options, RealTimePacer, TestHooks};
    use rbs_audio_io::{
        AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, EndpointId, EndpointKind,
        InputRequest,
    };
    use rbs_audio_io_wasapi::{WasapiAudioBackend, owned_resources};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
    use rbs_core::time::MonotonicClock;

    /// The fixture child playing a tone; killed by `remove`.
    struct Hooks {
        child: Mutex<Option<Child>>,
        id: EndpointId,
    }

    impl TestHooks for Hooks {
        fn remove(&self, endpoint: &EndpointId) -> bool {
            if *endpoint != self.id {
                return false;
            }
            let Ok(mut child) = self.child.lock() else {
                return false;
            };
            child
                .take()
                .is_some_and(|mut c| c.kill().is_ok() && c.wait().is_ok())
        }

        fn resources(&self) -> Option<u64> {
            Some(owned_resources())
        }
    }

    impl Drop for Hooks {
        fn drop(&mut self) {
            if let Ok(mut child) = self.child.lock()
                && let Some(mut child) = child.take()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// The default devices and the fixture application only: a developer's machine
    /// has many endpoints the suite cannot remove.
    struct Selected {
        backend: WasapiAudioBackend,
        app: EndpointId,
    }

    impl AdapterFactory for Selected {
        fn id(&self) -> AdapterId {
            self.backend.id()
        }
        fn display_name(&self) -> &str {
            self.backend.display_name()
        }
        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            self.backend.probe()
        }
    }

    impl AudioBackend for Selected {
        fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
            Ok(self
                .backend
                .enumerate(kind)?
                .into_iter()
                .filter(|e| match kind {
                    EndpointKind::Application => e.id == self.app,
                    _ => e.is_default,
                })
                .collect())
        }

        fn open_input(
            &self,
            request: &InputRequest,
        ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
            self.backend.open_input(request)
        }
    }

    fn start_fixture() -> Child {
        let exe = std::env::current_exe().unwrap_or_default();
        let child = Command::new(exe)
            .args(["fixture_process", "--exact", "--ignored", "--nocapture"])
            .env(super::FIXTURE, "1")
            .spawn();
        assert!(child.is_ok(), "cannot start the fixture: {child:?}");
        child.unwrap_or_else(|_| unreachable!())
    }

    /// Waits (hang guard 5 s) until the fixture's audio session is listed.
    fn fixture_endpoint(backend: &WasapiAudioBackend, pid: u32) -> EndpointId {
        let id = EndpointId::new(format!("app:{pid}"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let listed = backend
                .enumerate(EndpointKind::Application)
                .is_ok_and(|e| e.iter().any(|e| e.id == id));
            if listed {
                return id;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the fixture's audio session never appeared");
    }

    pub(super) fn run() {
        let backend = WasapiAudioBackend::new();
        let child = start_fixture();
        let app = fixture_endpoint(&backend, child.id());
        delivers_the_tone(&backend, &app);
        let hooks = Hooks {
            child: Mutex::new(Some(child)),
            id: app.clone(),
        };
        let options = Options {
            measure_allocations: Some(super::measure),
            hooks: Some(&hooks),
            ..Options::default()
        };
        let selected = Selected { backend, app };
        // A machine without a microphone still runs every case on the loopback and
        // the fixture; the output device is required.
        let inputs = selected
            .enumerate(EndpointKind::Input)
            .map(|e| e.len())
            .unwrap_or(0);
        if inputs == 0 {
            eprintln!("no active microphone: the Input kind is not covered on this machine");
        }
        let outputs = selected
            .enumerate(EndpointKind::OutputLoopback)
            .map(|e| e.len())
            .unwrap_or(0);
        assert_eq!(outputs, 1, "no default output endpoint");
        let report = conformance::run(&selected, &mut RealTimePacer::new(), &options);
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
        assert_eq!(owned_resources(), 0);
    }

    /// The tail of half a second of audio from `endpoint` (hang guard 3 s): its rms and
    /// its frequency from zero crossings.
    fn listen(backend: &WasapiAudioBackend, endpoint: &EndpointId) -> (f32, f32, f32) {
        let clock = Arc::new(MonotonicClock::new());
        let stream = backend.open_input(&InputRequest::new(endpoint.clone(), clock));
        let Ok(mut stream) = stream else {
            panic!("cannot open {endpoint}: {:?}", stream.err());
        };
        assert!(stream.start().is_ok());
        let rate = stream.format().sample_rate.hz() as usize;
        let channels = stream.format().channels.channels();
        let mut planes = vec![vec![0.0_f32; rate]; channels];
        let mut collected: Vec<f32> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while collected.len() < rate / 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            let mut out: Vec<&mut [f32]> = planes.iter_mut().map(|p| p.as_mut_slice()).collect();
            if let Ok(read) = stream.read(&mut out) {
                collected.extend_from_slice(&planes[0][..read.frames]);
            }
        }
        stream.stop();
        // Skip the start, which may hold silence filled before the first packet.
        let tail = &collected[collected.len() / 2..];
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len().max(1) as f32).sqrt();
        let crossings = tail
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let seconds = tail.len() as f32 / rate as f32;
        let frequency = crossings as f32 / 2.0 / seconds.max(f32::EPSILON);
        (rms, frequency, seconds)
    }

    /// Process loopback delivers what the fixture plays: a 440 Hz tone. Skipped when
    /// the default output's own mix is silent while the fixture plays, as with Remote
    /// Desktop's "Remote Audio" device: then no loopback can hear anything.
    fn delivers_the_tone(backend: &WasapiAudioBackend, app: &EndpointId) {
        let (rms, frequency, seconds) = listen(backend, app);
        eprintln!("fixture capture: rms {rms:.4}, {frequency:.0} Hz over {seconds:.2} s");
        if rms <= 0.01 {
            let output = backend
                .enumerate(EndpointKind::OutputLoopback)
                .ok()
                .and_then(|e| e.into_iter().find(|e| e.is_default));
            if let Some(output) = output {
                let (output_rms, _, _) = listen(backend, &output.id);
                if output_rms == 0.0 {
                    eprintln!(
                        "skip tone check: the default output {} is silent while the fixture \
                         plays (a remote session's audio device?)",
                        output.name
                    );
                    return;
                }
            }
        }
        assert!(rms > 0.01, "the application's audio was not captured");
        assert!((frequency - 440.0).abs() < 20.0, "{frequency} Hz");
    }
}

/// The fixture: plays a quiet 440 Hz tone on the default output until killed.
#[cfg(windows)]
mod fixture {
    use std::time::{Duration, Instant};

    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, eConsole, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    };

    /// Longest the fixture plays if nobody kills it (a hang guard).
    const LIFETIME: Duration = Duration::from_secs(60);

    pub(super) fn play_tone() {
        // SAFETY: test fixture; plain COM calls on live interfaces. The mix format is
        // the shared-mode format, 32-bit float in every Windows audio engine, read
        // before it is freed.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let Ok(enumerator) =
                CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            else {
                return;
            };
            let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
                return;
            };
            let Ok(client) = device.Activate::<IAudioClient>(CLSCTX_ALL, None) else {
                return;
            };
            let Ok(mix) = client.GetMixFormat() else {
                return;
            };
            let format = mix.read_unaligned();
            let channels = usize::from(format.nChannels);
            let rate = format.nSamplesPerSec as f32;
            let ok = client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix, None)
                .is_ok();
            CoTaskMemFree(Some(mix as _));
            if !ok {
                return;
            }
            let Ok(render) = client.GetService::<IAudioRenderClient>() else {
                return;
            };
            let Ok(size) = client.GetBufferSize() else {
                return;
            };
            let _ = client.Start();
            let mut phase = 0.0_f32;
            let step = 440.0 * std::f32::consts::TAU / rate;
            let end = Instant::now() + LIFETIME;
            while Instant::now() < end {
                let padding = client.GetCurrentPadding().unwrap_or(size);
                let free = size.saturating_sub(padding);
                if free > 0
                    && let Ok(data) = render.GetBuffer(free)
                {
                    let samples = std::slice::from_raw_parts_mut(
                        data.cast::<f32>(),
                        free as usize * channels,
                    );
                    for frame in samples.chunks_exact_mut(channels) {
                        frame.fill(0.05 * phase.sin());
                        phase = (phase + step) % std::f32::consts::TAU;
                    }
                    let _ = render.ReleaseBuffer(free, 0);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
