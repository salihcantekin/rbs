//! The media methods on the wire (protocol 1.83, ADR-0108; slide shows 1.102,
//! ADR-0171).

use super::*;

/// Every `media.*` request, for `every_request`.
pub(super) fn requests() -> Vec<Request> {
    let c = Request::Command;
    let one = MediaSource { source: 3 };
    vec![
        c(Command::MediaPlay(one)),
        c(Command::MediaPause(one)),
        c(Command::MediaSeek(MediaSeek {
            source: 3,
            position_ms: 1_500,
        })),
        c(Command::MediaRestart(one)),
        c(Command::MediaState(one)),
        c(Command::MediaNext(one)),
        c(Command::MediaPrevious(one)),
    ]
}

#[test]
fn a_media_state_round_trips_and_an_unknown_state_is_kept_as_unknown() {
    let info = MediaStateInfo {
        source: 3,
        state: MediaPlayState::Cued,
        cursor_ms: 1_500,
        duration_ms: Some(10_000),
        index: None,
        count: None,
    };
    round_trip(&info);
    assert_eq!(
        serde_json::to_value(info).unwrap(),
        json!({"source": 3, "state": "cued", "cursor_ms": 1500, "duration_ms": 10000})
    );
    let newer: MediaStateInfo =
        serde_json::from_value(json!({"source": 3, "state": "buffering", "cursor_ms": 0})).unwrap();
    assert_eq!(newer.state, MediaPlayState::Unknown);
    assert_eq!(newer.duration_ms, None);
}

#[test]
fn a_slide_show_state_carries_its_index_and_count() {
    let info = MediaStateInfo {
        source: 4,
        state: MediaPlayState::Playing,
        cursor_ms: 250,
        duration_ms: None,
        index: Some(2),
        count: Some(24),
    };
    round_trip(&info);
    assert_eq!(
        serde_json::to_value(info).unwrap(),
        json!({"source": 4, "state": "playing", "cursor_ms": 250, "index": 2, "count": 24})
    );
    assert_eq!(
        serde_json::to_value(SourceKind::Slideshow).unwrap(),
        json!("slideshow")
    );
}
