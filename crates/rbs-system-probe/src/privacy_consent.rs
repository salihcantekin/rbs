//! Windows camera and microphone privacy switches (ADR-0141 decision 6).
//!
//! Desktop apps get no prompt on Windows; a blocked device fails with
//! `E_ACCESSDENIED` on open. The switches are stored in the registry: the policy
//! `HKLM\SOFTWARE\Policies\Microsoft\Windows\AppPrivacy` (`LetAppsAccessCamera` /
//! `LetAppsAccessMicrophone`, 2 = force deny), the device-wide switch
//! `HKLM\...\CapabilityAccessManager\ConsentStore\<store>` and the user's switches
//! `HKCU\...\ConsentStore\<store>` and its `NonPackaged` subkey (desktop apps), each
//! with `Value` = `Allow` or `Deny`. The layout is undocumented, so anything missing
//! or unreadable counts as allowed, which keeps today's behaviour: `open` still
//! reports the refusal.

/// A device kind with a privacy switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentKind {
    /// Cameras (`webcam`).
    Camera,
    /// Microphones (`microphone`).
    Microphone,
}

impl ConsentKind {
    /// The consent store name.
    pub fn store(self) -> &'static str {
        match self {
            ConsentKind::Camera => "webcam",
            ConsentKind::Microphone => "microphone",
        }
    }

    /// The policy value name under `AppPrivacy`.
    pub fn policy(self) -> &'static str {
        match self {
            ConsentKind::Camera => "LetAppsAccessCamera",
            ConsentKind::Microphone => "LetAppsAccessMicrophone",
        }
    }
}

/// What the switches say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// Nothing refuses (or nothing could be read).
    Allowed,
    /// A switch the user can turn on refuses.
    Denied,
    /// A policy forces deny; the user cannot change it.
    Restricted,
}

/// The raw values read for one kind; `None` where a value is missing or unreadable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsentValues {
    /// `AppPrivacy\LetAppsAccess*` (0 user decides, 1 force allow, 2 force deny).
    pub policy: Option<u32>,
    /// `HKLM ...\ConsentStore\<store>\Value`.
    pub machine: Option<String>,
    /// `HKCU ...\ConsentStore\<store>\Value`.
    pub user: Option<String>,
    /// `HKCU ...\ConsentStore\<store>\NonPackaged\Value`.
    pub desktop_apps: Option<String>,
}

/// Folds the values: policy force-deny is [`Consent::Restricted`]; policy force-allow
/// is [`Consent::Allowed`]; otherwise `Deny` at any level is [`Consent::Denied`].
pub fn consent_from(values: &ConsentValues) -> Consent {
    match values.policy {
        Some(2) => return Consent::Restricted,
        Some(1) => return Consent::Allowed,
        _ => {}
    }
    let deny = |v: &Option<String>| v.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("Deny"));
    if deny(&values.machine) || deny(&values.user) || deny(&values.desktop_apps) {
        Consent::Denied
    } else {
        Consent::Allowed
    }
}

/// Reads and folds the switches for `kind`. Four registry reads, no prompt. Off
/// Windows, always [`Consent::Allowed`].
pub fn consent(kind: ConsentKind) -> Consent {
    consent_from(&read(kind))
}

#[cfg(windows)]
fn read(kind: ConsentKind) -> ConsentValues {
    crate::windows::consent_values(kind)
}

#[cfg(not(windows))]
fn read(_kind: ConsentKind) -> ConsentValues {
    ConsentValues::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    #[test]
    fn nothing_read_is_allowed() {
        assert_eq!(consent_from(&ConsentValues::default()), Consent::Allowed);
    }

    #[test]
    fn a_deny_at_any_level_is_denied() {
        for v in [
            ConsentValues {
                machine: s("Deny"),
                ..Default::default()
            },
            ConsentValues {
                user: s("Deny"),
                machine: s("Allow"),
                ..Default::default()
            },
            ConsentValues {
                desktop_apps: s("deny"),
                user: s("Allow"),
                ..Default::default()
            },
        ] {
            assert_eq!(consent_from(&v), Consent::Denied, "{v:?}");
        }
    }

    #[test]
    fn policy_wins_over_switches() {
        let v = ConsentValues {
            policy: Some(2),
            user: s("Allow"),
            ..Default::default()
        };
        assert_eq!(consent_from(&v), Consent::Restricted);
        let v = ConsentValues {
            policy: Some(1),
            user: s("Deny"),
            ..Default::default()
        };
        assert_eq!(consent_from(&v), Consent::Allowed);
        let v = ConsentValues {
            policy: Some(0),
            user: s("Deny"),
            ..Default::default()
        };
        assert_eq!(consent_from(&v), Consent::Denied);
    }

    #[test]
    fn unknown_words_are_allowed() {
        let v = ConsentValues {
            user: s("Prompt"),
            ..Default::default()
        };
        assert_eq!(consent_from(&v), Consent::Allowed);
    }
}
