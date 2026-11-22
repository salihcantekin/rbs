//! OS permission statuses across adapters (ADR-0141), with the scripted reference
//! adapters: no devices, no prompts, no sleeps.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;

use rbs_audio_io::{EndpointKind, Permission as AudioPermission};
use rbs_audio_io_test::TestAudioBackend;
use rbs_capture::{Permission, TargetKind};
use rbs_capture_test::TestCaptureFactory;
use rbs_sources::SourceTypes;
use rbs_sources::permissions::{PermissionKind, PermissionStatus};

mod input {
    //! A scripted input backend with a permission (ADR-0165 decision 7).

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use rbs_core::capability::{CapabilitySet, CapabilityValue};
    use rbs_input::{
        EventMask, InputBackend, InputError, InputSink, Permission, RunningInput, caps,
    };

    pub struct Asking {
        pub state: Arc<Mutex<Permission>>,
        pub answer: Permission,
        pub prompts: Arc<AtomicU32>,
        pub has_permission: bool,
    }

    impl InputBackend for Asking {
        fn name(&self) -> &'static str {
            "asking"
        }
        fn capabilities(&self) -> CapabilitySet {
            let set = CapabilitySet::new()
                .with(caps::POINTER, CapabilityValue::Flag(true))
                .with(caps::BUTTONS, CapabilityValue::Flag(true));
            if self.has_permission {
                set.with(caps::PERMISSION, CapabilityValue::Flag(true))
                    .with(caps::PERMISSION_PROMPT, CapabilityValue::Flag(true))
                    .with(caps::PERMISSION_RESTART, CapabilityValue::Flag(true))
            } else {
                set
            }
        }
        fn start(&self, _: EventMask, _: InputSink) -> Result<Box<dyn RunningInput>, InputError> {
            Err(InputError::PermissionDenied)
        }
        fn permission(&self) -> Permission {
            *self.state.lock().unwrap()
        }
        fn request_permission(&self) -> Permission {
            self.prompts.fetch_add(1, Ordering::Relaxed);
            *self.state.lock().unwrap() = self.answer;
            self.answer
        }
    }
}

fn types(
    capture: TestCaptureFactory,
    audio: TestAudioBackend,
) -> (SourceTypes, Arc<TestCaptureFactory>, Arc<TestAudioBackend>) {
    let capture = Arc::new(capture);
    let audio = Arc::new(audio);
    let mut reg = SourceTypes::new();
    reg.register_capture(capture.clone()).unwrap();
    reg.register_audio(audio.clone()).unwrap();
    reg.probe();
    (reg, capture, audio)
}

#[test]
fn adapters_without_a_permission_model_need_none() {
    let (reg, _, _) = types(TestCaptureFactory::new(), TestAudioBackend::new());
    for kind in PermissionKind::ALL {
        // Input monitoring comes from the input monitor, which is not set here.
        let expected = if kind == PermissionKind::InputMonitoring {
            PermissionStatus::Unavailable
        } else {
            PermissionStatus::NotNeeded
        };
        assert_eq!(reg.permission(kind).status, expected, "{kind:?}");
    }
}

#[test]
fn a_promptable_camera_asks_once_and_is_allowed() {
    let capture = TestCaptureFactory::new().with_permission(
        TargetKind::Camera,
        Permission::NotDetermined,
        Some(Permission::Granted),
        false,
    );
    let (reg, capture, _) = types(capture, TestAudioBackend::new());
    let before = reg.permission(PermissionKind::Camera);
    assert_eq!(before.status, PermissionStatus::NotAsked);
    assert!(before.can_prompt);
    assert_eq!(capture.prompts(), 0, "reading never prompts");

    let after = reg.request_permission(PermissionKind::Camera);
    assert_eq!(after.status, PermissionStatus::Allowed);
    assert!(!after.can_prompt);
    assert_eq!(capture.prompts(), 1);

    reg.request_permission(PermissionKind::Camera);
    assert_eq!(
        capture.prompts(),
        1,
        "an answered permission is not asked again"
    );
}

#[test]
fn an_adapter_that_cannot_prompt_is_never_asked() {
    let capture = TestCaptureFactory::new().with_permission(
        TargetKind::Camera,
        Permission::NotDetermined,
        None,
        false,
    );
    let (reg, capture, _) = types(capture, TestAudioBackend::new());
    let info = reg.request_permission(PermissionKind::Camera);
    assert_eq!(info.status, PermissionStatus::NotAsked);
    assert!(!info.can_prompt);
    assert_eq!(capture.prompts(), 0);
}

#[test]
fn screen_folds_displays_and_windows_and_reports_restart() {
    let capture = TestCaptureFactory::new()
        .with_permission(TargetKind::Display, Permission::Granted, None, true)
        .with_permission(TargetKind::Window, Permission::Denied, None, true);
    let (reg, _, _) = types(capture, TestAudioBackend::new());
    let info = reg.permission(PermissionKind::Screen);
    assert_eq!(info.status, PermissionStatus::Blocked);
    assert!(info.restart);
}

#[test]
fn audio_kinds_read_their_own_answers() {
    let audio = TestAudioBackend::new()
        .with_permission(EndpointKind::Input, AudioPermission::Restricted, None)
        .with_permission(EndpointKind::OutputLoopback, AudioPermission::Unknown, None);
    let (reg, _, _) = types(TestCaptureFactory::new(), audio);
    assert_eq!(
        reg.permission(PermissionKind::Microphone).status,
        PermissionStatus::Managed
    );
    assert_eq!(
        reg.permission(PermissionKind::SystemSound).status,
        PermissionStatus::Unknown
    );
}

#[test]
fn a_promptable_microphone_is_asked() {
    let audio = TestAudioBackend::new().with_permission(
        EndpointKind::Input,
        AudioPermission::NotDetermined,
        Some(AudioPermission::Denied),
    );
    let (reg, _, audio) = types(TestCaptureFactory::new(), audio);
    let info = reg.request_permission(PermissionKind::Microphone);
    assert_eq!(info.status, PermissionStatus::Blocked);
    assert_eq!(audio.prompts(), 1);
}

#[test]
fn no_serving_adapter_is_unavailable() {
    let reg = SourceTypes::new();
    assert_eq!(
        reg.permission(PermissionKind::Camera).status,
        PermissionStatus::Unavailable
    );
}

#[test]
fn a_request_runs_off_the_registry() {
    let capture = TestCaptureFactory::new().with_permission(
        TargetKind::Camera,
        Permission::NotDetermined,
        Some(Permission::Granted),
        false,
    );
    let (reg, capture, _) = types(capture, TestAudioBackend::new());
    let request = reg.permission_request(PermissionKind::Camera);
    assert!(!request.is_empty());
    assert_eq!(request.kind(), PermissionKind::Camera);
    std::thread::spawn(move || request.run()).join().unwrap();
    assert_eq!(capture.prompts(), 1);
    assert_eq!(
        reg.permission(PermissionKind::Camera).status,
        PermissionStatus::Allowed
    );
    assert!(reg.permission_request(PermissionKind::Camera).is_empty());
}

fn with_input(has_permission: bool) -> (SourceTypes, std::sync::Arc<std::sync::atomic::AtomicU32>) {
    let (mut reg, _, _) = types(TestCaptureFactory::new(), TestAudioBackend::new());
    let prompts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    reg.set_input(rbs_input::InputMonitor::new(Box::new(input::Asking {
        state: Arc::new(std::sync::Mutex::new(if has_permission {
            rbs_input::Permission::NotDetermined
        } else {
            // A backend with no OS permission answers with the port's default.
            rbs_input::Permission::Granted
        })),
        answer: rbs_input::Permission::Denied,
        prompts: prompts.clone(),
        has_permission,
    })));
    (reg, prompts)
}

#[test]
fn input_monitoring_follows_the_input_monitor() {
    let (reg, prompts) = with_input(true);
    let before = reg.permission(PermissionKind::InputMonitoring);
    assert_eq!(before.status, PermissionStatus::NotAsked);
    assert!(before.can_prompt);
    assert!(before.restart, "the grant applies after a restart");
    assert_eq!(
        prompts.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "reading never prompts"
    );
    // Another kind never asks the monitor.
    assert!(reg.permission_request(PermissionKind::Camera).is_empty());

    let after = reg.request_permission(PermissionKind::InputMonitoring);
    assert_eq!(after.status, PermissionStatus::Blocked);
    assert_eq!(prompts.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(
        reg.permission_request(PermissionKind::InputMonitoring)
            .is_empty(),
        "asked once"
    );
}

#[test]
fn a_monitor_without_a_permission_needs_none() {
    let (reg, prompts) = with_input(false);
    let info = reg.permission(PermissionKind::InputMonitoring);
    assert_eq!(info.status, PermissionStatus::NotNeeded);
    assert!(
        reg.permission_request(PermissionKind::InputMonitoring)
            .is_empty()
    );
    assert_eq!(prompts.load(std::sync::atomic::Ordering::Relaxed), 0);
}
