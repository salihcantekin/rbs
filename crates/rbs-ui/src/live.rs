//! The live indicator (UI rules U4 and U21).
//!
//! It is not a panel and cannot be hidden. It always shows, in text and not
//! colour alone, whether RBS is live and recording, how long, and the names of
//! the active outputs, and whether it sends to the virtual camera (ADR-0066
//! decision 12). This module turns engine status into the parts the
//! shell shows; the shell supplies the translated words.

/// Live and recording state as the engine reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveStatus {
    /// Streaming to at least one output.
    pub live: bool,
    /// Recording.
    pub recording: bool,
    /// Seconds since going live, or since recording started when only recording.
    pub elapsed_secs: u64,
    /// Display names of the active outputs.
    pub outputs: Vec<String>,
    /// Sending the program to the virtual camera: apps may show it to other people.
    /// It has no duration, so it needs no tick.
    pub camera: bool,
}

/// What the indicator shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveState {
    /// Neither live nor recording.
    Off,
    /// Live, not recording.
    Live,
    /// Recording, not live.
    Recording,
    /// Live and recording.
    LiveAndRecording,
}

impl LiveStatus {
    /// The state to show.
    pub fn state(&self) -> LiveState {
        match (self.live, self.recording) {
            (false, false) => LiveState::Off,
            (true, false) => LiveState::Live,
            (false, true) => LiveState::Recording,
            (true, true) => LiveState::LiveAndRecording,
        }
    }

    /// Is RBS live or recording? Then the indicator needs a 1 Hz tick for the
    /// duration; otherwise it needs none.
    pub fn is_active(&self) -> bool {
        self.live || self.recording
    }

    /// Is the indicator shown: live, recording or sending to the virtual camera.
    pub fn is_shown(&self) -> bool {
        self.is_active() || self.camera
    }

    /// Duration as `H:MM:SS` (hours grow past 9 as needed).
    pub fn duration_text(&self) -> String {
        let s = self.elapsed_secs;
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    }

    /// Output names joined with `, `, in the order the engine reports them.
    pub fn outputs_text(&self) -> String {
        self.outputs.join(", ")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn states_and_text() {
        let mut s = LiveStatus::default();
        assert_eq!(s.state(), LiveState::Off);
        assert!(!s.is_active());
        s.live = true;
        s.elapsed_secs = 3 * 3600 + 7 * 60 + 9;
        s.outputs = vec!["Twitch".into(), "Recording".into()];
        assert_eq!(s.state(), LiveState::Live);
        assert_eq!(s.duration_text(), "3:07:09");
        assert_eq!(s.outputs_text(), "Twitch, Recording");
        s.recording = true;
        assert_eq!(s.state(), LiveState::LiveAndRecording);
        s.live = false;
        assert_eq!(s.state(), LiveState::Recording);
        s.elapsed_secs = 11 * 3600;
        assert_eq!(s.duration_text(), "11:00:00");
    }

    #[test]
    fn the_camera_shows_without_a_tick() {
        let s = LiveStatus {
            camera: true,
            ..LiveStatus::default()
        };
        assert_eq!(s.state(), LiveState::Off);
        assert!(s.is_shown());
        assert!(!s.is_active(), "no duration, so no 1 Hz tick");
    }
}
