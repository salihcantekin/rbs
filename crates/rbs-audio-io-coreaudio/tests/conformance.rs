//! Runs the audio I/O port conformance suite against the CoreAudio adapter.
//!
//! `probes_and_stays_idle` runs everywhere and fast: probing, enumeration and Standby
//! on whatever endpoints the machine has. The full suite needs real audio devices and
//! real time, so it is the slow, hardware-gated `slow_hardware_conformance`
//! (`RBS_AUDIO_HW=1`, macOS only). It reads the default microphone (asking for the
//! microphone permission if it was never asked), the default output's loopback, and
//! an application: a fixture child process (this test binary again) that plays a quiet
//! 440 Hz tone, which the suite also removes (kills) for the `endpoint lost` case.
//! Loopback and application capture need the "System Audio Recording" permission for
//! the terminal; without it macOS delivers silence and the tone check fails.
//!
//! ```text
//! RBS_AUDIO_HW=1 cargo test -p rbs-audio-io-coreaudio --test conformance -- --ignored --nocapture
//! ```

/// Allocations plus frees made by `f` on this thread.
#[cfg(target_os = "macos")]
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

/// Environment variable that makes this binary play the fixture tone.
#[cfg(target_os = "macos")]
const FIXTURE: &str = "RBS_COREAUDIO_FIXTURE";

/// A fixture process started by `slow_hardware_conformance`; does nothing on its own.
#[test]
#[ignore = "started by slow_hardware_conformance as a fixture process"]
fn fixture_process() {
    #[cfg(target_os = "macos")]
    if std::env::var_os(FIXTURE).is_some() {
        fixture::play_tone();
    }
}

#[test]
fn probes_and_stays_idle() {
    use std::sync::Arc;

    use rbs_audio_io::{AudioBackend, AudioIoError, EndpointId, EndpointKind, InputRequest};
    use rbs_audio_io_coreaudio::{CoreAudioBackend, owned_resources};
    use rbs_core::registry::AdapterFactory;
    use rbs_core::time::TestClock;

    let backend = CoreAudioBackend::new();
    let clock = Arc::new(TestClock::new());
    let missing = InputRequest::new(EndpointId::new("input:missing"), clock.clone());
    if cfg!(not(target_os = "macos")) {
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
    for kind in EndpointKind::ALL {
        let Ok(endpoints) = backend.enumerate(kind) else {
            // Taps need macOS 14.4.
            assert_ne!(kind, EndpointKind::Input);
            continue;
        };
        // Opening is Standby: nothing owned until `start`. (Microphones may refuse for
        // lack of permission; that is checked without owning anything too.)
        if let Some(endpoint) = endpoints.into_iter().next() {
            let _stream = backend.open_input(&InputRequest::new(endpoint.id, clock.clone()));
            assert_eq!(owned_resources(), 0);
        }
    }
    assert_eq!(owned_resources(), 0);
}

#[test]
#[ignore = "slow: about 15 s of real audio through CoreAudio on a machine with audio \
            devices; needs RBS_AUDIO_HW=1"]
fn slow_hardware_conformance() {
    if std::env::var_os("RBS_AUDIO_HW").is_none_or(|v| v.is_empty()) {
        eprintln!("RBS_AUDIO_HW is not set: hardware conformance skipped");
        return;
    }
    #[cfg(target_os = "macos")]
    hardware::run();
    #[cfg(not(target_os = "macos"))]
    panic!("RBS_AUDIO_HW is set but CoreAudio needs macOS");
}

#[cfg(target_os = "macos")]
mod hardware {
    use std::process::{Child, Command};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use crate::fixture;
    use rbs_audio_io::conformance::{self, Options, RealTimePacer, TestHooks};
    use rbs_audio_io::{
        AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, EndpointId, EndpointKind,
        InputRequest, Permission,
    };
    use rbs_audio_io_coreaudio::{CoreAudioBackend, owned_resources};
    use rbs_core::capability::{CapabilitySet, CapabilityValue};
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

    /// The default devices and the fixture application only.
    struct Selected {
        backend: CoreAudioBackend,
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

        fn permission(&self, kind: EndpointKind) -> Permission {
            self.backend.permission(kind)
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

    /// Waits (hang guard 5 s) until the fixture plays and is listed.
    fn fixture_endpoint(backend: &CoreAudioBackend, pid: u32) -> EndpointId {
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
        panic!("the fixture never appeared as a playing process");
    }

    /// Asks for the microphone permission if it was never asked (hang guard 60 s for
    /// the user's answer).
    fn microphone_permission(backend: &CoreAudioBackend) {
        if backend.permission(EndpointKind::Input) != Permission::NotDetermined {
            return;
        }
        eprintln!("asking for the microphone permission: answer the macOS prompt");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(CoreAudioBackend::new().request_permission(EndpointKind::Input));
        });
        let answer = rx.recv_timeout(Duration::from_secs(60));
        eprintln!("microphone permission: {answer:?}");
    }

    pub(super) fn run() {
        let backend = CoreAudioBackend::new();
        microphone_permission(&backend);
        assert_eq!(
            backend.permission(EndpointKind::Input),
            Permission::Granted,
            "grant the microphone permission to the terminal running the test"
        );
        silent_loopback_delivers(&backend);
        excludes_own_process(&backend);
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
        for kind in [EndpointKind::Input, EndpointKind::OutputLoopback] {
            let listed = selected.enumerate(kind).map(|e| e.len()).unwrap_or(0);
            assert_eq!(listed, 1, "no default {kind} endpoint");
        }
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

    /// The default output's loopback delivers (silence) before anything in this test
    /// plays: the aggregate must run without tapped audio (#109). Run with nothing
    /// else playing to check the silent case.
    fn silent_loopback_delivers(backend: &CoreAudioBackend) {
        let endpoints = backend.enumerate(EndpointKind::OutputLoopback);
        let Some(endpoint) = endpoints.ok().and_then(|e| e.into_iter().next()) else {
            panic!("no output loopback endpoint");
        };
        let clock = Arc::new(MonotonicClock::new());
        let stream = backend.open_input(&InputRequest::new(endpoint.id, clock));
        let Ok(mut stream) = stream else {
            panic!("cannot open the loopback: {:?}", stream.err());
        };
        let started = stream.start();
        assert!(started.is_ok(), "{started:?}");
        let rate = stream.format().sample_rate.hz() as usize;
        let want = rate / 10;
        let mut buffers: Vec<Vec<f32>> = vec![vec![0.0; rate]; stream.format().channels.channels()];
        let mut got = 0;
        // A hang guard: 100 ms of audio is due well within it.
        let deadline = Instant::now() + Duration::from_secs(2);
        while got < want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            let mut out: Vec<&mut [f32]> = buffers.iter_mut().map(|b| &mut b[..]).collect();
            if let Ok(read) = stream.read(&mut out) {
                got += read.frames;
            }
        }
        stream.stop();
        eprintln!("output loopback before playback: {got} frames");
        assert!(
            got >= want,
            "the output loopback delivered {got} frames while silent"
        );
    }

    /// RMS of about `seconds` of the first channel of the default output loopback.
    fn loopback_rms(backend: &CoreAudioBackend, exclude: bool, seconds: f32) -> f32 {
        let endpoints = backend.enumerate(EndpointKind::OutputLoopback);
        let Some(endpoint) = endpoints.ok().and_then(|e| e.into_iter().next()) else {
            panic!("no output loopback endpoint");
        };
        let clock = Arc::new(MonotonicClock::new());
        let request = InputRequest::new(endpoint.id, clock).with_exclude_own_process(exclude);
        let stream = backend.open_input(&request);
        let Ok(mut stream) = stream else {
            panic!("cannot open the loopback: {:?}", stream.err());
        };
        assert!(stream.start().is_ok());
        let rate = stream.format().sample_rate.hz() as usize;
        let want = (rate as f32 * seconds) as usize;
        let mut buffers: Vec<Vec<f32>> = vec![vec![0.0; rate]; stream.format().channels.channels()];
        let mut collected: Vec<f32> = Vec::new();
        // A hang guard.
        let deadline = Instant::now() + Duration::from_secs(5);
        while collected.len() < want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            let mut out: Vec<&mut [f32]> = buffers.iter_mut().map(|b| &mut b[..]).collect();
            if let Ok(read) = stream.read(&mut out) {
                collected.extend_from_slice(&buffers[0][..read.frames]);
            }
        }
        stream.stop();
        // Skip the first half: the tap and the process object settle.
        let tail = &collected[collected.len() / 2..];
        (tail.iter().map(|s| s * s).sum::<f32>() / tail.len().max(1) as f32).sqrt()
    }

    /// `exclude_own_process` (ADR-0096): a tone played by this very process is in the
    /// output loopback unless the flag is set. Needs nothing else playing.
    fn excludes_own_process(backend: &CoreAudioBackend) {
        assert!(backend.probe().is_ok_and(|caps| {
            caps.get(&rbs_audio_io::caps::LOOPBACK_EXCLUDE_SELF)
                == Some(&CapabilityValue::Flag(true))
        }));
        // Detached: it plays until this process exits (a 60 s hang guard).
        std::thread::spawn(fixture::play_tone);
        // Wait (hang guard 5 s) until the tone is audible in the full loopback.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut included = 0.0;
        while included <= 0.01 && Instant::now() < deadline {
            included = loopback_rms(backend, false, 0.5);
        }
        eprintln!("loopback with own tone: rms {included:.4}");
        assert!(included > 0.01, "this process's tone was not captured");
        let excluded = loopback_rms(backend, true, 0.5);
        eprintln!("loopback excluding own process: rms {excluded:.5}");
        assert!(excluded < 0.001, "own sound leaked: rms {excluded}");
    }

    /// The application tap delivers what the fixture plays: a 440 Hz tone.
    fn delivers_the_tone(backend: &CoreAudioBackend, app: &EndpointId) {
        let clock = Arc::new(MonotonicClock::new());
        let stream = backend.open_input(&InputRequest::new(app.clone(), clock));
        let Ok(mut stream) = stream else {
            panic!("cannot open the fixture: {:?}", stream.err());
        };
        let started = stream.start();
        assert!(started.is_ok(), "{started:?}");
        let rate = stream.format().sample_rate.hz() as usize;
        let mut left = vec![0.0_f32; rate];
        let mut right = vec![0.0_f32; rate];
        let mut collected: Vec<f32> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while collected.len() < rate && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            if let Ok(read) = stream.read(&mut [&mut left, &mut right]) {
                collected.extend_from_slice(&left[..read.frames]);
            }
        }
        stream.stop();
        let tail = &collected[collected.len() / 2..];
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len().max(1) as f32).sqrt();
        let crossings = tail
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let seconds = tail.len() as f32 / rate as f32;
        let frequency = crossings as f32 / 2.0 / seconds.max(f32::EPSILON);
        eprintln!("fixture capture: rms {rms:.4}, {frequency:.0} Hz over {seconds:.2} s");
        assert!(
            rms > 0.01,
            "the application's audio was not captured (System Audio Recording permission?)"
        );
        assert!((frequency - 440.0).abs() < 20.0, "{frequency} Hz");
    }
}

/// The fixture: plays a quiet 440 Hz tone on the default output until killed.
#[cfg(target_os = "macos")]
mod fixture {
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::time::Duration;

    use objc2_core_audio::{
        AudioDeviceCreateIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
        AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    };
    use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};

    /// Longest the fixture plays if nobody kills it (a hang guard).
    const LIFETIME: Duration = Duration::from_secs(60);

    struct Tone {
        phase: f32,
        step: f32,
    }

    unsafe extern "C-unwind" fn render(
        _device: AudioObjectID,
        _now: NonNull<AudioTimeStamp>,
        _input: NonNull<AudioBufferList>,
        _input_time: NonNull<AudioTimeStamp>,
        output: NonNull<AudioBufferList>,
        _output_time: NonNull<AudioTimeStamp>,
        client: *mut c_void,
    ) -> i32 {
        // SAFETY: test fixture; `client` is the leaked `Tone`, the list holds
        // `mNumberBuffers` float buffers of `mDataByteSize` bytes.
        unsafe {
            let tone = &mut *client.cast::<Tone>();
            let list = output.as_ref();
            let base = (&raw const list.mBuffers).cast::<AudioBuffer>();
            let mut phase = tone.phase;
            for i in 0..list.mNumberBuffers as usize {
                let buffer = *base.add(i);
                let channels = (buffer.mNumberChannels as usize).max(1);
                let samples = std::slice::from_raw_parts_mut(
                    buffer.mData.cast::<f32>(),
                    buffer.mDataByteSize as usize / 4,
                );
                phase = tone.phase;
                for frame in samples.chunks_exact_mut(channels) {
                    frame.fill(0.05 * phase.sin());
                    phase = (phase + tone.step) % std::f32::consts::TAU;
                }
            }
            tone.phase = phase;
        }
        0
    }

    fn get<T: Copy + Default>(object: AudioObjectID, selector: u32) -> T {
        let address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut value = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        // SAFETY: test fixture; `value` has room for `size` bytes.
        unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            );
        }
        value
    }

    pub(super) fn play_tone() {
        let device: AudioObjectID = get(
            kAudioObjectSystemObject as AudioObjectID,
            kAudioHardwarePropertyDefaultOutputDevice,
        );
        let rate: f64 = get(device, kAudioDevicePropertyNominalSampleRate);
        let tone = Box::into_raw(Box::new(Tone {
            phase: 0.0,
            step: 440.0 * std::f32::consts::TAU / rate.max(1.0) as f32,
        }));
        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: test fixture; the tone lives until the process is killed.
        unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(render),
                tone.cast(),
                NonNull::from(&mut proc_id),
            );
            AudioDeviceStart(device, proc_id);
        }
        std::thread::sleep(LIFETIME);
    }
}

#[test]
fn outputs_are_listed_with_a_device_key_and_open_in_standby() {
    use rbs_audio_io::{AudioBackend, AudioIoError, EndpointId, EndpointKind, OutputRequest};
    use rbs_audio_io_coreaudio::{CoreAudioBackend, owned_resources};

    let backend = CoreAudioBackend::new();
    let missing = OutputRequest::new(EndpointId::new("play:missing"));
    if cfg!(not(target_os = "macos")) {
        assert!(matches!(
            backend.open_output(&missing),
            Err(AudioIoError::Unsupported(_))
        ));
        return;
    }
    assert!(matches!(
        backend.open_output(&missing),
        Err(AudioIoError::EndpointNotFound)
    ));
    let Ok(outputs) = backend.enumerate(EndpointKind::Output) else {
        panic!("CoreAudio lists outputs");
    };
    for endpoint in &outputs {
        assert_eq!(endpoint.kind, EndpointKind::Output);
        assert!(endpoint.device_key.is_some(), "{endpoint:?}");
    }
    if let Some(endpoint) = outputs.into_iter().next() {
        let mut output = backend
            .open_output(&OutputRequest::new(endpoint.id))
            .unwrap_or_else(|e| panic!("open_output: {e}"));
        assert!(output.producer().is_some());
        assert!(output.producer().is_none());
        // Standby: nothing owned until `start`.
        assert_eq!(owned_resources(), 0);
        assert_eq!(output.stats().played, 0);
    }
    assert_eq!(owned_resources(), 0);
}

#[test]
#[ignore = "slow: plays about 0.5 s of silence on the default output; needs RBS_AUDIO_HW=1"]
fn slow_hardware_output() {
    if std::env::var_os("RBS_AUDIO_HW").is_none_or(|v| v.is_empty()) {
        eprintln!("RBS_AUDIO_HW is not set: hardware output skipped");
        return;
    }
    #[cfg(target_os = "macos")]
    hardware_output::run();
    #[cfg(not(target_os = "macos"))]
    panic!("RBS_AUDIO_HW is set but CoreAudio needs macOS");
}

#[cfg(target_os = "macos")]
mod hardware_output {
    use std::time::{Duration, Instant};

    use rbs_audio_io::{AudioBackend, AudioOutput, EndpointKind, OutputRequest};
    use rbs_audio_io_coreaudio::{CoreAudioBackend, owned_resources};

    /// Hang guard only: the device asks for audio every few milliseconds.
    const GUARD: Duration = Duration::from_secs(5);

    fn wait_for(output: &dyn AudioOutput, frames: u64) {
        let start = Instant::now();
        while output.stats().played + output.stats().silence < frames {
            assert!(
                start.elapsed() < GUARD,
                "the device did not pull {frames} frames"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn run() {
        let backend = CoreAudioBackend::new();
        let endpoint = backend
            .enumerate(EndpointKind::Output)
            .ok()
            .and_then(|l| l.into_iter().find(|e| e.is_default))
            .unwrap_or_else(|| panic!("no default output"));
        let mut output = backend
            .open_output(&OutputRequest::new(endpoint.id.clone()))
            .unwrap_or_else(|e| panic!("open_output: {e}"));
        let mut producer = output.producer().unwrap_or_else(|| panic!("no producer"));
        let rate = u64::from(output.format().sample_rate.hz());
        output.start().unwrap_or_else(|e| panic!("start: {e}"));
        assert!(owned_resources() > 0);
        // Half a second of silence, in the ring's capacity.
        let chunk = vec![0.0_f32; 1_024 * producer.channels()];
        let mut written = 0_u64;
        while written < rate / 2 {
            while producer.space() < 1_024 {
                wait_for(
                    &*output,
                    output.stats().played + output.stats().silence + 256,
                );
            }
            written += producer.push(&chunk) as u64;
        }
        wait_for(&*output, written);
        let s = output.stats();
        assert!(s.played > 0, "{s:?}");
        assert!(s.played + s.dropped <= s.written, "{s:?}");
        assert!(output.status().is_ok());
        assert!(output.latency_ns() > 0);
        output.stop();
        assert_eq!(owned_resources(), 0);
        // It starts again.
        output.start().unwrap_or_else(|e| panic!("restart: {e}"));
        let before = output.stats().played + output.stats().silence;
        wait_for(&*output, before + 256);
        output.stop();
        assert_eq!(owned_resources(), 0);
    }
}
