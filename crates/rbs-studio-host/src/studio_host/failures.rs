//! How the control thread answers and logs what went wrong: refused commands, caught
//! panics and a studio that stopped (ADR-0055, the diagnostics log spec's `studio`
//! row, ADR-0158). Lines name a command by its kind only, never its payload.

use std::fmt::{self, Write};

use rbs_api::{ApiError, ErrorKind};
use rbs_core::log::Area;

use super::{Command, Msg, RecordingUpdate, Refusal, StreamUpdate, Update};

/// A command's kind: its variant name, read from its `Debug` text up to the first
/// bracket or space, so the payload is never formatted.
#[derive(Clone, Copy)]
pub(super) struct Kind {
    name: [u8; 48],
    len: usize,
}

impl Kind {
    /// The kind of a window command; `None` for other messages.
    pub(super) fn of(msg: Option<&Msg>) -> Option<Self> {
        let Some(Msg::Ui(c)) = msg else {
            return None;
        };
        let mut kind = Self {
            name: [0; 48],
            len: 0,
        };
        // The writer stops the formatting at the first bracket or space (an error).
        let _ = write!(kind, "{c:?}");
        rbs_core::trace!(Area::Studio, "command {kind}");
        Some(kind)
    }
}

impl Write for Kind {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if matches!(b, b'(' | b' ' | b'{') || self.len == self.name.len() {
                return Err(fmt::Error);
            }
            self.name[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(std::str::from_utf8(&self.name[..self.len]).unwrap_or("?"))
    }
}

/// The line, if any, for an update the control thread sends while it handles `kind`.
pub(super) fn log_update(kind: Option<Kind>, u: &Update) {
    match (kind, u) {
        (Some(kind), Update::Error(_)) => {
            rbs_core::warn!(Area::Studio, "command {kind} refused");
        }
        (_, Update::Failed(why)) => {
            rbs_core::error!(Area::Studio, "the studio stopped: {why}");
        }
        // Sent only by fault-injection builds (tests, ADR-0055).
        #[cfg(feature = "fault-injection")]
        (_, Update::Recovered { stopped }) => {
            rbs_core::info!(
                Area::Studio,
                "the studio recovered after a caught panic ({stopped} untracked outputs stopped)"
            );
        }
        _ => {}
    }
}

/// A refused control API request, by method and error kind.
pub(super) fn log_api_refusal(method: &str, e: &ApiError) {
    rbs_core::warn!(Area::Studio, "{method} refused ({:?})", e.kind);
}

/// What the UI waits for after `c`, if `c` panics before answering.
pub(super) fn answer_on_panic(c: &Command) -> Option<Update> {
    let why = || rbs_crash::CAUGHT_TEXT.to_owned();
    match c {
        Command::StartRecording(_) => Some(Update::Recording(RecordingUpdate::NotStarted(why()))),
        Command::StartStream(_) => Some(Update::Stream(StreamUpdate::NotStarted(why()))),
        Command::StartVerticalRecording(_) => Some(Update::VerticalRecording(
            RecordingUpdate::NotStarted(why()),
        )),
        Command::StartVerticalStream(_) => {
            Some(Update::VerticalStream(StreamUpdate::NotStarted(why())))
        }
        _ => None,
    }
}

/// A refusal as a control API error.
pub(super) fn refused(e: &Refusal) -> ApiError {
    ApiError::new(
        match e {
            Refusal::NotFound(_) => ErrorKind::NotFound,
            Refusal::Invalid(_) => ErrorKind::InvalidParams,
            Refusal::Conflict(_) => ErrorKind::Conflict,
            Refusal::Failed(_) => ErrorKind::Failed,
        },
        e.text(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_is_the_variant_name_without_its_payload() {
        let msg = Msg::Ui(Command::SetStudioMode(true));
        let kind = Kind::of(Some(&msg)).map(|k| k.to_string());
        assert_eq!(kind.as_deref(), Some("SetStudioMode"));
        let unit = Msg::Ui(Command::Rehearse);
        assert_eq!(
            Kind::of(Some(&unit)).map(|k| k.to_string()).as_deref(),
            Some("Rehearse")
        );
        assert!(Kind::of(None).is_none());
    }
}
