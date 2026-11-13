//! Whether Record and Go live also start and stop the vertical canvas's outputs
//! (ADR-0111). The choice is kept in the output profile's own `[outputs-vertical]`
//! section, apart from `recording-vertical` and `stream-vertical`, so writing it never
//! counts as saving vertical encoding settings.

use rbs_api::types::{OutputKind, OutputTarget, VerticalWithMain};
use serde::{Deserialize, Serialize};

/// The output profile section that holds [`VerticalFlags`].
pub const SECTION: &str = "outputs-vertical";

/// The section's own format version; a higher one was written by a newer RBS.
pub const FORMAT_VERSION: u32 = 1;

/// The with-main choice of each output kind (ADR-0111 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerticalFlags {
    /// The section's format version.
    pub format_version: u32,
    /// Record also starts and stops the vertical recording.
    pub record_with_main: bool,
    /// Go live also starts and stops the vertical stream.
    pub stream_with_main: bool,
}

impl Default for VerticalFlags {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            record_with_main: true,
            stream_with_main: true,
        }
    }
}

impl VerticalFlags {
    /// The flags a saved section gives: the defaults when it is absent, does not load,
    /// or was written by a newer RBS.
    pub fn from_section(saved: Result<Option<VerticalFlags>, String>) -> Self {
        match saved {
            Ok(Some(f)) if f.format_version <= FORMAT_VERSION => f,
            _ => Self::default(),
        }
    }

    /// The flag of `kind`.
    pub fn with_main(self, kind: OutputKind) -> bool {
        match kind {
            OutputKind::Record => self.record_with_main,
            OutputKind::Stream => self.stream_with_main,
        }
    }

    /// The flags with `kind` set to `on`.
    pub fn set(mut self, kind: OutputKind, on: bool) -> Self {
        match kind {
            OutputKind::Record => self.record_with_main = on,
            OutputKind::Stream => self.stream_with_main = on,
        }
        self.format_version = FORMAT_VERSION;
        self
    }

    /// The control API view (`settings.get`, protocol 1.42).
    pub fn to_api(self) -> VerticalWithMain {
        VerticalWithMain {
            record_with_main: self.record_with_main,
            stream_with_main: self.stream_with_main,
        }
    }
}

/// The canvases `record.toggle` or `stream.toggle` act on (ADR-0111 decision 5): every
/// canvas while the kind's flag is on and a vertical canvas exists, else the main one.
pub fn resolve_toggle(kind: OutputKind, flags: VerticalFlags, has_vertical: bool) -> OutputTarget {
    if has_vertical && flags.with_main(kind) {
        OutputTarget::All
    } else {
        OutputTarget::Main
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toggles_act_on_both_canvases_only_with_the_flag_and_a_vertical_canvas() {
        let on = VerticalFlags::default();
        let off = on.set(OutputKind::Record, false);
        for kind in [OutputKind::Record, OutputKind::Stream] {
            assert_eq!(resolve_toggle(kind, on, true), OutputTarget::All);
            assert_eq!(resolve_toggle(kind, on, false), OutputTarget::Main);
        }
        assert_eq!(
            resolve_toggle(OutputKind::Record, off, true),
            OutputTarget::Main
        );
        assert_eq!(
            resolve_toggle(OutputKind::Stream, off, true),
            OutputTarget::All
        );
    }

    #[test]
    fn a_missing_broken_or_newer_section_reads_as_the_defaults() {
        let d = VerticalFlags::default();
        assert!(d.record_with_main && d.stream_with_main);
        assert_eq!(VerticalFlags::from_section(Ok(None)), d);
        assert_eq!(VerticalFlags::from_section(Err("bad".into())), d);
        let newer = VerticalFlags {
            format_version: FORMAT_VERSION + 1,
            record_with_main: false,
            stream_with_main: false,
        };
        assert_eq!(VerticalFlags::from_section(Ok(Some(newer))), d);
        let saved = d.set(OutputKind::Stream, false);
        assert_eq!(VerticalFlags::from_section(Ok(Some(saved))), saved);
    }

    #[test]
    fn an_absent_key_reads_as_its_default() {
        let f: VerticalFlags = toml::from_str("record_with_main = false").unwrap_or_default();
        assert!(!f.record_with_main);
        assert!(f.stream_with_main);
        assert_eq!(f.format_version, FORMAT_VERSION);
    }
}
