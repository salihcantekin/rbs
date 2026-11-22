//! The words of Settings › Permissions (ADR-0141 decisions 7 and 10): one row per
//! permission kind and the "who gets these permissions" lines. Pure: the caller passes
//! the statuses, the OS family and this process's identity.

use std::path::Path;

use rbs_sources::permissions::{PermissionInfo, PermissionKind, PermissionStatus};
use rbs_system_probe::app_identity::{AppIdentity, app_name};

/// The OS family whose words and panes the page uses. Only the composition root
/// knows it (P3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// macOS (TCC).
    Mac,
    /// Windows (privacy switches).
    Windows,
    /// Linux and others (desktop portal).
    Other,
}

impl Os {
    /// The OS this build runs on.
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Other
        }
    }
}

/// What a row's button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// No button; the row shows its value.
    None,
    /// **Allow**: show the OS prompt.
    Allow,
    /// **Open system settings** at the kind's privacy pane.
    OpenSettings,
}

impl Action {
    /// The number the page uses (`PermissionRowData.action`).
    pub fn code(self) -> i32 {
        match self {
            Action::None => 0,
            Action::Allow => 1,
            Action::OpenSettings => 2,
        }
    }

    /// The action of a page number.
    pub fn from_code(code: i32) -> Self {
        match code {
            1 => Action::Allow,
            2 => Action::OpenSettings,
            _ => Action::None,
        }
    }
}

/// How a row's status reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Plain.
    Plain,
    /// Something to do.
    Warning,
    /// Refused.
    Problem,
}

/// One row's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The kind's name.
    pub label: &'static str,
    /// The status line under it.
    pub status: String,
    /// How the status reads.
    pub tone: Tone,
    /// The value shown when there is no button.
    pub value: &'static str,
    /// The button.
    pub action: Action,
}

/// The kind's name on the page.
pub fn label(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Camera => "Camera",
        PermissionKind::Microphone => "Microphone",
        PermissionKind::Screen => "Screen and windows",
        PermissionKind::SystemSound => "System and app sound",
        PermissionKind::InputMonitoring => "Input monitoring",
    }
}

/// The number the page uses for a kind (`PermissionRowData.kind`).
pub fn kind_code(kind: PermissionKind) -> i32 {
    match kind {
        PermissionKind::Camera => 0,
        PermissionKind::Microphone => 1,
        PermissionKind::Screen => 2,
        PermissionKind::SystemSound => 3,
        PermissionKind::InputMonitoring => 4,
    }
}

/// The kind of a page number.
pub fn kind_of_code(code: i32) -> Option<PermissionKind> {
    PermissionKind::ALL
        .into_iter()
        .find(|k| kind_code(*k) == code)
}

/// One row. `waiting`: the OS prompt for this kind is showing now. `pane`: the OS
/// privacy pane for this kind can be opened. `starter`: the app macOS most likely
/// gives the permissions to when it is not RBS.
pub fn row(
    kind: PermissionKind,
    info: PermissionInfo,
    waiting: bool,
    pane: bool,
    os: Os,
    starter: Option<&str>,
) -> Row {
    use PermissionStatus as S;
    let open = if pane {
        Action::OpenSettings
    } else {
        Action::None
    };
    let restart = if info.restart {
        " If you just allowed it, quit RBS and open it again."
    } else {
        ""
    };
    let (status, tone, value, action) = if waiting {
        (
            "Waiting for your answer to the system's question.".to_owned(),
            Tone::Warning,
            "Waiting",
            Action::None,
        )
    } else {
        match info.status {
            S::Allowed => (String::new(), Tone::Plain, "Allowed", Action::None),
            S::NotNeeded => (
                "No permission needed on this computer.".to_owned(),
                Tone::Plain,
                "Not needed",
                Action::None,
            ),
            S::AskedEachTime => (
                "The system asks each time you choose a screen or window.".to_owned(),
                Tone::Plain,
                "Asked each time",
                Action::None,
            ),
            S::Managed => (
                "Managed by your organization. Only an administrator can change it.".to_owned(),
                Tone::Problem,
                "Managed",
                Action::None,
            ),
            S::Blocked => (
                format!("Blocked. Allow RBS in the system's privacy settings.{restart}"),
                Tone::Problem,
                "Blocked",
                open,
            ),
            S::NotAsked if info.can_prompt => (
                format!("Not asked yet. The system asks once when you press Allow.{restart}"),
                Tone::Warning,
                "Not asked yet",
                Action::Allow,
            ),
            S::NotAsked => {
                let how = match (os, starter) {
                    (Os::Mac, Some(app)) => {
                        format!(
                            "Not asked yet. Start RBS from RBS.app to be asked, or allow {app}."
                        )
                    }
                    (Os::Mac, None) => {
                        "Not asked yet. Start RBS from RBS.app to be asked.".to_owned()
                    }
                    _ => "Not asked yet.".to_owned(),
                };
                (
                    format!("{how}{restart}"),
                    Tone::Warning,
                    "Not asked yet",
                    open,
                )
            }
            S::Unknown => (
                "The system asks the first time it is used.".to_owned(),
                Tone::Plain,
                "Asked on first use",
                open,
            ),
            S::Unavailable => (String::new(), Tone::Plain, "", Action::None),
        }
    };
    Row {
        label: label(kind),
        status,
        tone,
        value,
        action,
    }
}

/// The fix action of a source that was refused access (ADR-0141 decision 8), in
/// Properties' Source tab, never in the Sources list: `0` none (the prompt is showing),
/// `1` **Allow** (not asked yet and a prompt can show), `2` **Fix permission** (opens
/// Settings › Permissions). `info` is `None` before the statuses were read.
pub fn source_action(info: Option<PermissionInfo>, waiting: bool) -> i32 {
    match info {
        _ if waiting => 0,
        Some(i) if i.status == PermissionStatus::NotAsked && i.can_prompt => 1,
        _ => 2,
    }
}

/// The note of a pointer filter (Click rings, Pointer halo) while RBS may not see
/// the mouse (ADR-0165 decision 7): the text under the filter's title and its action
/// as in [`source_action`]. `None` when nothing stops the filter: Input monitoring is
/// allowed or not needed, there is no input monitor, or the status was not read yet.
pub fn pointer_note(info: Option<PermissionInfo>, waiting: bool) -> Option<(String, i32)> {
    use PermissionStatus as S;
    let i = info?;
    let restart = if i.restart {
        " If you just allowed it, quit RBS and open it again."
    } else {
        ""
    };
    let text = match i.status {
        S::Allowed | S::NotNeeded | S::AskedEachTime | S::Unavailable => return None,
        _ if waiting => "Waiting for your answer to the system's question.".to_owned(),
        S::NotAsked => "RBS can't see clicks until Input monitoring is allowed.".to_owned(),
        S::Managed => {
            "RBS can't see clicks: your organization manages Input monitoring.".to_owned()
        }
        S::Blocked | S::Unknown => {
            format!("RBS can't see clicks: Input monitoring is blocked.{restart}")
        }
    };
    let action = if i.status == S::Managed {
        0
    } else {
        source_action(info, waiting)
    };
    Some((text, action))
}

/// The "Who gets these permissions" card: the value, the path line and a note
/// (with whether the note is a warning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// The app identity, short.
    pub value: String,
    /// Where this copy is.
    pub path: String,
    /// What follows from it.
    pub note: String,
    /// The note is a warning.
    pub warn: bool,
}

/// The holder card for `id` on `os`.
pub fn holder(id: &AppIdentity, os: Os) -> Holder {
    let shown = |p: &Path| p.display().to_string();
    match os {
        Os::Mac => {
            let starter = id.foreign_app().as_deref().and_then(app_name);
            match (&id.bundle, starter) {
                (Some(bundle), None) => {
                    let dev = bundle.components().any(|c| c.as_os_str() == "target");
                    Holder {
                        value: id.bundle_id().unwrap_or_else(|| "RBS".to_owned()),
                        path: shown(bundle),
                        note: if dev {
                            "This build is signed for this computer only, so macOS may ask \
                             again after a rebuild. Another copy of RBS is a different app \
                             to macOS."
                                .to_owned()
                        } else {
                            "macOS gives these permissions to this copy of RBS.".to_owned()
                        },
                        warn: dev,
                    }
                }
                (bundle, Some(app)) => Holder {
                    value: app.clone(),
                    path: shown(bundle.as_deref().unwrap_or(&id.exe)),
                    note: format!(
                        "RBS was started from {app}, so macOS most likely gives these \
                         permissions to {app}, not to RBS."
                    ),
                    warn: true,
                },
                (None, None) => Holder {
                    value: "Plain program".to_owned(),
                    path: shown(&id.exe),
                    note: "RBS runs as a plain program, so macOS gives permissions to the \
                           app that started it."
                        .to_owned(),
                    warn: true,
                },
            }
        }
        Os::Windows => Holder {
            value: "All desktop apps".to_owned(),
            path: shown(&id.exe),
            note: "The Windows camera and microphone switches apply to all desktop apps."
                .to_owned(),
            warn: false,
        },
        Os::Other => Holder {
            value: "RBS".to_owned(),
            path: shown(&id.exe),
            note: "Screens and windows are chosen in the system's dialog each time.".to_owned(),
            warn: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn info(status: PermissionStatus, can_prompt: bool, restart: bool) -> PermissionInfo {
        PermissionInfo {
            status,
            can_prompt,
            restart,
        }
    }

    #[test]
    fn a_promptable_kind_offers_allow() {
        let r = row(
            PermissionKind::Camera,
            info(PermissionStatus::NotAsked, true, false),
            false,
            true,
            Os::Mac,
            None,
        );
        assert_eq!(r.action, Action::Allow);
        assert_eq!(r.tone, Tone::Warning);
    }

    #[test]
    fn a_terminal_started_build_points_to_the_app_bundle_and_the_starter() {
        let r = row(
            PermissionKind::Camera,
            info(PermissionStatus::NotAsked, false, false),
            false,
            true,
            Os::Mac,
            Some("Terminal"),
        );
        assert_eq!(r.action, Action::OpenSettings);
        assert!(r.status.contains("RBS.app") && r.status.contains("Terminal"));
    }

    #[test]
    fn blocked_opens_settings_only_where_a_pane_exists() {
        let blocked = info(PermissionStatus::Blocked, false, false);
        let with = row(
            PermissionKind::Camera,
            blocked,
            false,
            true,
            Os::Windows,
            None,
        );
        let without = row(
            PermissionKind::Camera,
            blocked,
            false,
            false,
            Os::Other,
            None,
        );
        assert_eq!(with.action, Action::OpenSettings);
        assert_eq!(without.action, Action::None);
        assert_eq!(with.tone, Tone::Problem);
    }

    #[test]
    fn restart_kinds_say_so() {
        let r = row(
            PermissionKind::Screen,
            info(PermissionStatus::Blocked, false, true),
            false,
            true,
            Os::Mac,
            None,
        );
        assert!(r.status.contains("open it again"));
    }

    #[test]
    fn managed_and_waiting_have_no_button() {
        for (status, waiting) in [
            (PermissionStatus::Managed, false),
            (PermissionStatus::NotAsked, true),
        ] {
            let r = row(
                PermissionKind::Microphone,
                info(status, true, false),
                waiting,
                true,
                Os::Mac,
                None,
            );
            assert_eq!(r.action, Action::None, "{status:?}");
        }
    }

    #[test]
    fn a_refused_source_offers_allow_only_when_a_prompt_can_show() {
        let not_asked = info(PermissionStatus::NotAsked, true, false);
        assert_eq!(source_action(Some(not_asked), false), 1);
        assert_eq!(source_action(Some(not_asked), true), 0);
        let blocked = info(PermissionStatus::Blocked, false, false);
        assert_eq!(source_action(Some(blocked), false), 2);
        assert_eq!(source_action(None, false), 2);
    }

    #[test]
    fn a_pointer_filter_says_why_it_cannot_see_clicks() {
        use PermissionStatus as S;
        assert_eq!(pointer_note(None, false), None, "not read yet");
        for quiet in [S::Allowed, S::NotNeeded, S::Unavailable] {
            assert_eq!(pointer_note(Some(info(quiet, false, true)), false), None);
        }
        let (text, action) = pointer_note(Some(info(S::NotAsked, true, true)), false).unwrap();
        assert_eq!(action, 1, "Allow");
        assert!(text.starts_with("RBS can't see clicks until"), "{text}");
        let (text, action) = pointer_note(Some(info(S::Blocked, false, true)), false).unwrap();
        assert_eq!(action, 2, "Fix permission");
        assert!(text.ends_with("quit RBS and open it again."), "{text}");
        let (_, action) = pointer_note(Some(info(S::NotAsked, true, true)), true).unwrap();
        assert_eq!(action, 0, "the prompt is showing");
        let (_, action) = pointer_note(Some(info(S::Managed, false, false)), false).unwrap();
        assert_eq!(action, 0, "nothing the user can do");
    }

    #[test]
    fn codes_round_trip() {
        for k in PermissionKind::ALL {
            assert_eq!(kind_of_code(kind_code(k)), Some(k));
        }
        for a in [Action::None, Action::Allow, Action::OpenSettings] {
            assert_eq!(Action::from_code(a.code()), a);
        }
    }

    #[test]
    fn holder_names_the_starter_of_a_plain_program() {
        let id = AppIdentity {
            exe: PathBuf::from("/Users/a/rbs/target/release/rbs"),
            bundle: None,
            plist: None,
            ancestors: vec![
                PathBuf::from("/bin/zsh"),
                PathBuf::from("/Applications/iTerm.app/Contents/MacOS/iTerm2"),
            ],
        };
        let h = holder(&id, Os::Mac);
        assert_eq!(h.value, "iTerm");
        assert!(h.warn && h.note.contains("iTerm"));
    }

    #[test]
    fn holder_of_a_development_bundle_warns_about_rebuilds() {
        let id = AppIdentity {
            exe: PathBuf::from("/Users/a/rbs/target/release/RBS.app/Contents/MacOS/rbs"),
            bundle: Some(PathBuf::from("/Users/a/rbs/target/release/RBS.app")),
            plist: Some("<key>CFBundleIdentifier</key><string>app.rbs.RBS</string>".to_owned()),
            ancestors: vec![PathBuf::from("/sbin/launchd")],
        };
        let h = holder(&id, Os::Mac);
        assert_eq!(h.value, "app.rbs.RBS");
        assert!(h.warn && h.note.contains("rebuild"));
    }
}
