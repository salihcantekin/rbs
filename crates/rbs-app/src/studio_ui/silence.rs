//! The question before a silent recording or stream: Record and Go live check once,
//! when pressed, whether a microphone is heard in the program scene, and ask first
//! when none is (UI spec, "Silent output warning"; `app.toml` `warn_silent`).
//!
//! The check reads the program scene's rows the Mixer already has and the master's
//! mute and volume. It runs only on a press and on an output's start, never per frame
//! or per audio block (P13).

use rbs_app::studio_host::{SourceKind, SourceRow, SourceStatus};
use rbs_scene::model::MasterAudio;

use super::unavailable_text;

/// What one microphone of the program scene does to the mix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicState {
    /// It is heard.
    Heard,
    /// The user deactivated it: the device is closed.
    Deactivated,
    /// Its device is missing, refused or failed; the reason in words.
    Unavailable(String),
    /// Hidden in the scene: not heard while hidden.
    Hidden,
    /// Muted, or its fader is all the way down.
    Muted,
    /// Its device is not open.
    NotRunning,
}

/// One microphone of the program scene: its name and state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicFact {
    /// The source's name.
    pub name: String,
    /// What it does to the mix.
    pub state: MicState,
}

/// The microphones of the program scene, in list order.
pub fn mics(rows: &[SourceRow]) -> Vec<MicFact> {
    rows.iter()
        .filter(|r| r.kind == Some(SourceKind::AudioInput))
        .map(|r| MicFact {
            name: r.name.clone(),
            state: state_of(r),
        })
        .collect()
}

fn state_of(r: &SourceRow) -> MicState {
    let heard = r.heard.unwrap_or(r.audio);
    if !r.active {
        return MicState::Deactivated;
    }
    match &r.status {
        SourceStatus::Unavailable { fault, .. } => {
            return MicState::Unavailable(unavailable_text(*fault, r.kind).to_owned());
        }
        SourceStatus::Failed(_) => return MicState::NotRunning,
        _ => {}
    }
    if !r.visible {
        MicState::Hidden
    } else if heard.muted || heard.volume <= 0.0 {
        MicState::Muted
    } else if r.meter.is_none() {
        MicState::NotRunning
    } else {
        MicState::Heard
    }
}

/// Why the output would carry no voice, in words for the question's first line;
/// `None` while a microphone is heard.
pub fn reason(mics: &[MicFact], master: MasterAudio) -> Option<String> {
    if master.muted || master.volume <= 0.0 {
        return Some("The master is muted.".to_owned());
    }
    if mics.iter().any(|m| m.state == MicState::Heard) {
        return None;
    }
    Some(match mics {
        [] => "No microphone is in this scene.".to_owned(),
        [one] => one_reason(one),
        many if many.iter().all(|m| m.state == MicState::Muted) => {
            format!("{} are muted.", names(many))
        }
        many => format!(
            "None of the {} microphones in this scene is heard.",
            many.len()
        ),
    })
}

fn one_reason(m: &MicFact) -> String {
    let n = &m.name;
    match &m.state {
        MicState::Heard => String::new(),
        MicState::Deactivated => format!("{n} is deactivated."),
        MicState::Unavailable(why) => format!("{n}: {why}."),
        MicState::Hidden => format!("{n} is hidden in this scene."),
        MicState::Muted => format!("{n} is muted."),
        MicState::NotRunning => format!("{n} is not running."),
    }
}

/// "A and B", "A, B and C".
fn names(mics: &[MicFact]) -> String {
    match mics {
        [] => String::new(),
        [one] => one.name.clone(),
        [head @ .., last] => {
            let head: Vec<&str> = head.iter().map(|m| m.name.as_str()).collect();
            format!("{} and {}", head.join(", "), last.name)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn mic(name: &str, state: MicState) -> MicFact {
        MicFact {
            name: name.to_owned(),
            state,
        }
    }

    fn on() -> MasterAudio {
        MasterAudio::default()
    }

    #[test]
    fn a_heard_microphone_asks_nothing() {
        let m = [mic("Mic", MicState::Muted), mic("Headset", MicState::Heard)];
        assert_eq!(reason(&m, on()), None);
    }

    #[test]
    fn no_microphone_is_said() {
        assert_eq!(
            reason(&[], on()).as_deref(),
            Some("No microphone is in this scene.")
        );
    }

    #[test]
    fn a_muted_master_wins_over_a_heard_microphone() {
        let master = MasterAudio::new(1.0, true);
        let m = [mic("Mic", MicState::Heard)];
        assert_eq!(reason(&m, master).as_deref(), Some("The master is muted."));
        let silent = MasterAudio::new(0.0, false);
        assert!(reason(&m, silent).is_some());
    }

    #[test]
    fn one_microphone_says_its_own_reason() {
        let cases = [
            (MicState::Muted, "Mic is muted."),
            (MicState::Hidden, "Mic is hidden in this scene."),
            (MicState::Deactivated, "Mic is deactivated."),
            (MicState::NotRunning, "Mic is not running."),
            (
                MicState::Unavailable("Microphone not found".into()),
                "Mic: Microphone not found.",
            ),
        ];
        for (state, text) in cases {
            assert_eq!(reason(&[mic("Mic", state)], on()).as_deref(), Some(text));
        }
    }

    #[test]
    fn several_muted_microphones_are_named() {
        let m = [
            mic("Mic", MicState::Muted),
            mic("Headset", MicState::Muted),
            mic("Cam mic", MicState::Muted),
        ];
        assert_eq!(
            reason(&m, on()).as_deref(),
            Some("Mic, Headset and Cam mic are muted.")
        );
    }

    #[test]
    fn several_microphones_for_different_reasons_are_counted() {
        let m = [mic("Mic", MicState::Muted), mic("USB", MicState::Hidden)];
        assert_eq!(
            reason(&m, on()).as_deref(),
            Some("None of the 2 microphones in this scene is heard.")
        );
    }
}
