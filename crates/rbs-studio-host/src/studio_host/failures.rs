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
    match c {
        Command::Api { call, .. } => api_answer_on_panic(call),
        _ => None,
    }
}

/// What the window waits for after its control API `call`, if the call panics: its
/// Record and Go live (ADR-0164 decision 3), one canvas each, and a recording into
/// a named file wait for their start.
pub(super) fn api_answer_on_panic(call: &rbs_api::Command) -> Option<Update> {
    start_refused(call, rbs_crash::CAUGHT_TEXT)
}

/// The update the window's start `call` waits for when it ends unstarted, `why`.
pub(super) fn start_refused(call: &rbs_api::Command, why: &str) -> Option<Update> {
    let why = || why.to_owned();
    let canvas = |t: Option<rbs_api::types::OutputTarget>| t.unwrap_or_default().single();
    let (record, canvas) = match call {
        rbs_api::Command::RecordStart(p) => (true, canvas(p.canvas)?),
        rbs_api::Command::RecordStartToFile(p) => (true, p.canvas.unwrap_or_default()),
        rbs_api::Command::StreamStart(p) => (false, canvas(p.canvas)?),
        _ => return None,
    };
    let vertical = match canvas {
        rbs_api::types::OutputCanvas::Main => false,
        rbs_api::types::OutputCanvas::Vertical => true,
    };
    Some(match (record, vertical) {
        (true, false) => Update::Recording(RecordingUpdate::NotStarted(why())),
        (false, false) => Update::Stream(StreamUpdate::NotStarted(why())),
        (true, true) => Update::VerticalRecording(RecordingUpdate::NotStarted(why())),
        (false, true) => Update::VerticalStream(StreamUpdate::NotStarted(why())),
    })
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
        let unit = Msg::Ui(Command::Break);
        assert_eq!(
            Kind::of(Some(&unit)).map(|k| k.to_string()).as_deref(),
            Some("Break")
        );
        assert!(Kind::of(None).is_none());
    }
}
