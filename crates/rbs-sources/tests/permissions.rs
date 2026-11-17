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
        assert_eq!(
            reg.permission(kind).status,
            PermissionStatus::NotNeeded,
            "{kind:?}"
        );
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
