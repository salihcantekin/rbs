//! A recording profile with several audio tracks (protocol 1.113, ADR-0170 decision
//! 7): a format that carries one refuses the start before anything opens.

use super::*;

#[test]
fn a_wav_recording_with_two_tracks_is_refused_with_tracks_unsupported() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-tracks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx) = host(gpu, &clock);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let mut profile = rbs_studio_host::record_profile::RecordProfile::new();
    profile.folder = dir.clone();
    profile.format = rbs_studio_host::record_profile::Format::Wav;
    profile.audio_tracks = vec!["main".into(), "Music".into()];
    assert_eq!(profile.problem(), None);
    host.send(Command::SetProfile(profile));
    let mut api = ApiClient::attach(&host);

    let refused = api.call("record.start", serde_json::json!({"profile": "default"}));
    assert_eq!(error_kind(&refused), "failed", "{refused}");
    assert_eq!(refused["error"]["data"]["reason"], "tracks_unsupported");
    assert_eq!(refused["error"]["data"]["allowed"], 1);
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle", "{status}");
    // Refused before anything opens: nothing is written in the folder.
    let written = std::fs::read_dir(&dir).map_or(0, Iterator::count);
    assert_eq!(written, 0, "{dir:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
