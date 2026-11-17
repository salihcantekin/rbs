//! OS permissions as the user sees them (ADR-0141): four kinds, one status each.
//!
//! The capture and audio input ports answer per adapter and per port kind
//! (`permission`, fast, never prompts). This module maps the user's four kinds to
//! port kinds and folds every serving adapter's answer and capabilities into one
//! [`PermissionInfo`]. The fold is pure ([`fold`]); [`SourceTypes::permission`] and
//! [`SourceTypes::request_permission`] gather the answers. Nothing is cached, polled
//! or started.
//!
//! [`SourceTypes::permission`]: crate::SourceTypes::permission
//! [`SourceTypes::request_permission`]: crate::SourceTypes::request_permission

use rbs_audio_io::EndpointKind;
use rbs_capture::TargetKind;
use rbs_graph::node::NodeTypeId;

use crate::types::{SourceClass, class_of};

/// What the user gives permission for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionKind {
    /// Cameras.
    Camera,
    /// Microphones and other audio inputs.
    Microphone,
    /// Screens and windows.
    Screen,
    /// The sound of the computer or of one application.
    SystemSound,
}

impl PermissionKind {
    /// Every kind, in the order a page lists them.
    pub const ALL: [PermissionKind; 4] = [
        PermissionKind::Camera,
        PermissionKind::Microphone,
        PermissionKind::Screen,
        PermissionKind::SystemSound,
    ];

    /// The capture port kinds this kind covers.
    pub fn target_kinds(self) -> &'static [TargetKind] {
        match self {
            PermissionKind::Camera => &[TargetKind::Camera],
            PermissionKind::Screen => &[TargetKind::Display, TargetKind::Window],
            PermissionKind::Microphone | PermissionKind::SystemSound => &[],
        }
    }

    /// The audio input port kinds this kind covers.
    pub fn endpoint_kinds(self) -> &'static [EndpointKind] {
        match self {
            PermissionKind::Microphone => &[EndpointKind::Input],
            PermissionKind::SystemSound => {
                &[EndpointKind::OutputLoopback, EndpointKind::Application]
            }
            PermissionKind::Camera | PermissionKind::Screen => &[],
        }
    }

    /// The kind a source type needs, if any.
    pub fn of_type(type_id: &NodeTypeId) -> Option<Self> {
        match class_of(type_id)? {
            SourceClass::Capture(kind) => Some(Self::of_target(kind)),
            SourceClass::Audio(kind) => Self::of_endpoint(kind),
            _ => None,
        }
    }

    /// The kind of a capture port kind.
    pub fn of_target(kind: TargetKind) -> Self {
        match kind {
            TargetKind::Camera => PermissionKind::Camera,
            _ => PermissionKind::Screen,
        }
    }

    /// The kind of an audio port kind; `None` for `Output`, which is not captured.
    pub fn of_endpoint(kind: EndpointKind) -> Option<Self> {
        match kind {
            EndpointKind::Input => Some(PermissionKind::Microphone),
            EndpointKind::OutputLoopback | EndpointKind::Application => {
                Some(PermissionKind::SystemSound)
            }
            _ => None,
        }
    }
}

/// One status per kind, in words for the user (glossary, ADR-0141 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionStatus {
    /// "Managed by your organization": forbidden, and the user cannot change it.
    Managed,
    /// "Blocked": refused; only the OS settings can change it.
    Blocked,
    /// "Not asked yet": the OS asks when RBS requests it.
    NotAsked,
    /// The OS asks the first time the kind is used; it cannot be read before.
    Unknown,
    /// "Asked each time": the OS asks in its own dialog on every pick.
    AskedEachTime,
    /// "Allowed".
    Allowed,
    /// "No permission needed on this computer".
    NotNeeded,
    /// No installed adapter serves the kind; a page leaves the row out.
    Unavailable,
}

/// One adapter's permission answer for one port kind, in the port-neutral form the
/// fold reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Answer {
    /// `Permission::Granted`.
    Granted,
    /// `Permission::Denied`.
    Denied,
    /// `Permission::NotDetermined`.
    NotDetermined,
    /// `Permission::Restricted`.
    Restricted,
    /// `Permission::Unknown`, and any value a newer port adds.
    Unknown,
}

impl From<rbs_capture::Permission> for Answer {
    fn from(p: rbs_capture::Permission) -> Self {
        use rbs_capture::Permission as P;
        match p {
            P::Granted => Answer::Granted,
            P::Denied => Answer::Denied,
            P::NotDetermined => Answer::NotDetermined,
            P::Restricted => Answer::Restricted,
            _ => Answer::Unknown,
        }
    }
}

impl From<rbs_audio_io::Permission> for Answer {
    fn from(p: rbs_audio_io::Permission) -> Self {
        use rbs_audio_io::Permission as P;
        match p {
            P::Granted => Answer::Granted,
            P::Denied => Answer::Denied,
            P::NotDetermined => Answer::NotDetermined,
            P::Restricted => Answer::Restricted,
            _ => Answer::Unknown,
        }
    }
}

/// What one adapter says about one port kind: its answer and its capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AdapterAnswer {
    /// `permission(kind)`.
    pub answer: Answer,
    /// The OS has a user permission for this kind (`*.permission.<kind>`).
    pub has_permission: bool,
    /// `request_permission` can show a prompt now (`*.permission.<kind>.prompt`).
    pub can_prompt: bool,
    /// A grant applies after a restart (`capture.permission.<kind>.restart`).
    pub restart: bool,
    /// The OS asks on every pick of this kind (`capture.pick.consent` and the kind is
    /// picked, not listed).
    pub consent_per_pick: bool,
}

impl AdapterAnswer {
    /// An adapter without a permission model: `Granted`, no capability.
    pub const NONE: AdapterAnswer = AdapterAnswer {
        answer: Answer::Granted,
        has_permission: false,
        can_prompt: false,
        restart: false,
        consent_per_pick: false,
    };
}

/// A kind's status and what the user can do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PermissionInfo {
    /// The folded status.
    pub status: PermissionStatus,
    /// Some adapter that says "not asked" can show a prompt now: offer **Allow**.
    pub can_prompt: bool,
    /// Some adapter applies a grant only after a restart.
    pub restart: bool,
}

/// Folds every serving adapter's answer into one status (ADR-0141 decision 1), in
/// this precedence: Managed, Blocked, NotAsked, Unknown, AskedEachTime, Allowed,
/// NotNeeded; no answers at all is Unavailable.
pub fn fold(answers: &[AdapterAnswer]) -> PermissionInfo {
    let any = |f: &dyn Fn(&AdapterAnswer) -> bool| answers.iter().any(f);
    let status = if answers.is_empty() {
        PermissionStatus::Unavailable
    } else if any(&|a| a.answer == Answer::Restricted) {
        PermissionStatus::Managed
    } else if any(&|a| a.answer == Answer::Denied) {
        PermissionStatus::Blocked
    } else if any(&|a| a.answer == Answer::NotDetermined) {
        PermissionStatus::NotAsked
    } else if any(&|a| a.answer == Answer::Unknown) {
        PermissionStatus::Unknown
    } else if answers.iter().all(|a| a.consent_per_pick) {
        PermissionStatus::AskedEachTime
    } else if any(&|a| a.has_permission) {
        PermissionStatus::Allowed
    } else {
        PermissionStatus::NotNeeded
    };
    PermissionInfo {
        status,
        can_prompt: status == PermissionStatus::NotAsked
            && any(&|a| a.answer == Answer::NotDetermined && a.can_prompt),
        restart: any(&|a| a.restart),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWERS: [Answer; 5] = [
        Answer::Granted,
        Answer::Denied,
        Answer::NotDetermined,
        Answer::Restricted,
        Answer::Unknown,
    ];

    /// Every adapter answer: 5 answers x 16 flag combinations.
    fn every_answer() -> Vec<AdapterAnswer> {
        let mut all = Vec::new();
        for answer in ANSWERS {
            for bits in 0..16u8 {
                all.push(AdapterAnswer {
                    answer,
                    has_permission: bits & 1 != 0,
                    can_prompt: bits & 2 != 0,
                    restart: bits & 4 != 0,
                    consent_per_pick: bits & 8 != 0,
                });
            }
        }
        all
    }

    fn rank(s: PermissionStatus) -> u8 {
        use PermissionStatus as S;
        match s {
            S::Managed => 0,
            S::Blocked => 1,
            S::NotAsked => 2,
            S::Unknown => 3,
            S::AskedEachTime => 4,
            S::Allowed => 5,
            S::NotNeeded => 6,
            S::Unavailable => 7,
        }
    }

    /// The status of one adapter alone, by the rules of decision 1.
    fn single(a: &AdapterAnswer) -> PermissionStatus {
        use PermissionStatus as S;
        match a.answer {
            Answer::Restricted => S::Managed,
            Answer::Denied => S::Blocked,
            Answer::NotDetermined => S::NotAsked,
            Answer::Unknown => S::Unknown,
            Answer::Granted if a.consent_per_pick => S::AskedEachTime,
            Answer::Granted if a.has_permission => S::Allowed,
            Answer::Granted => S::NotNeeded,
        }
    }

    #[test]
    fn permission_fold_follows_precedence() {
        let all = every_answer();
        for a in &all {
            assert_eq!(fold(&[*a]).status, single(a), "{a:?}");
            for b in &all {
                let got = fold(&[*a, *b]).status;
                // Two adapters: the worse of the two by precedence, except that
                // "asked each time" needs both, and then falls to Allowed or
                // NotNeeded by the permission flags.
                let (sa, sb) = (single(a), single(b));
                let expected = if rank(sa) < 4 || rank(sb) < 4 {
                    if rank(sa) <= rank(sb) { sa } else { sb }
                } else if sa == PermissionStatus::AskedEachTime
                    && sb == PermissionStatus::AskedEachTime
                {
                    PermissionStatus::AskedEachTime
                } else if a.has_permission || b.has_permission {
                    PermissionStatus::Allowed
                } else {
                    PermissionStatus::NotNeeded
                };
                assert_eq!(got, expected, "{a:?} + {b:?}");
                assert_eq!(fold(&[*b, *a]).status, got, "order must not matter");
            }
        }
    }

    #[test]
    fn allow_is_offered_only_for_a_promptable_not_asked() {
        for a in every_answer() {
            let info = fold(&[a]);
            assert_eq!(
                info.can_prompt,
                a.answer == Answer::NotDetermined && a.can_prompt,
                "{a:?}"
            );
            assert_eq!(info.restart, a.restart);
        }
    }

    #[test]
    fn no_adapter_is_unavailable() {
        let info = fold(&[]);
        assert_eq!(info.status, PermissionStatus::Unavailable);
        assert!(!info.can_prompt && !info.restart);
    }

    #[test]
    fn source_types_map_to_their_kind() {
        use crate::types as t;
        let cases = [
            (t::CAMERA_SOURCE, Some(PermissionKind::Camera)),
            (t::DISPLAY_SOURCE, Some(PermissionKind::Screen)),
            (t::WINDOW_SOURCE, Some(PermissionKind::Screen)),
            (t::AUDIO_INPUT_SOURCE, Some(PermissionKind::Microphone)),
            (t::AUDIO_OUTPUT_SOURCE, Some(PermissionKind::SystemSound)),
            (
                t::APPLICATION_AUDIO_SOURCE,
                Some(PermissionKind::SystemSound),
            ),
            (t::IMAGE_SOURCE, None),
        ];
        for (id, kind) in cases {
            assert_eq!(
                PermissionKind::of_type(&NodeTypeId::new_static(id)),
                kind,
                "{id}"
            );
        }
    }
}
