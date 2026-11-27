//! The audio tracks of a multi-track recording in `record.status` (protocol 1.113,
//! ADR-0170 decision 11): listed only for more than one track, absent otherwise.

use super::*;

pub(super) fn recording(audio_tracks: Vec<AudioTrack>) -> RecordState {
    RecordState::Recording {
        profile: None,
        video_encoder: "VT".into(),
        audio_encoder: Some("AAC".into()),
        tracks: Vec::new(),
        skipped: Vec::new(),
        paused: false,
        paused_ms: 0,
        recorded_ms: 0,
        audio_tracks,
    }
}

#[test]
fn a_multi_track_recording_lists_its_audio_tracks_and_one_track_does_not() {
    let one = serde_json::to_value(recording(Vec::new())).unwrap();
    assert!(one.get("audio_tracks").is_none(), "{one}");
    let two = recording(vec![
        AudioTrack {
            bus: "main".into(),
            encoder: "AAC".into(),
        },
        AudioTrack {
            bus: "Music".into(),
            encoder: "AAC".into(),
        },
    ]);
    let wire = serde_json::to_value(&two).unwrap();
    assert_eq!(
        wire["audio_tracks"],
        json!([{"bus": "main", "encoder": "AAC"}, {"bus": "Music", "encoder": "AAC"}])
    );
    round_trip(&two);
    // A 1.112 server's state reads with no tracks.
    let older: RecordState = serde_json::from_value(json!({"state": "recording",
        "profile": null, "video_encoder": "VT", "audio_encoder": null}))
    .unwrap();
    assert!(
        matches!(older, RecordState::Recording { audio_tracks, .. } if audio_tracks.is_empty())
    );
}
