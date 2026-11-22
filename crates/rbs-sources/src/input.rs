//! The process's input monitor on the source registry (ADR-0165): set by the
//! composition root, lent by the studio to pointer filters, and what the host serves
//! to filters (spec `video-filters.md`, "Pointer types").

use rbs_core::capability::CapabilityValue;

use crate::SourceTypes;

impl SourceTypes {
    /// Sets the process's input monitor (the composition root's, ADR-0165). Pointer
    /// filters are lent it; without one they pass their input through.
    pub fn set_input(&mut self, monitor: rbs_input::InputMonitor) {
        self.input = Some(monitor);
    }

    /// The monitor set with [`Self::set_input`].
    pub fn input(&self) -> Option<&rbs_input::InputMonitor> {
        self.input.as_ref()
    }

    /// The host services filters get here (spec `video-filters.md`, "Pointer types"):
    /// the pointer when the monitor reports pointer positions and buttons.
    pub fn serves(&self) -> rbs_filters::Needs {
        let Some(monitor) = &self.input else {
            return rbs_filters::Needs::NONE;
        };
        let caps = monitor.capabilities();
        let flag = |key: rbs_core::capability::CapabilityKey| {
            matches!(
                caps.get(&key),
                Some(rbs_core::capability::CapabilityValue::Flag(true))
            )
        };
        if flag(rbs_input::caps::POINTER) && flag(rbs_input::caps::BUTTONS) {
            rbs_filters::Needs::POINTER
        } else {
            rbs_filters::Needs::NONE
        }
    }

    /// Whether a source of `type_id` publishes a picture mapping (ADR-0165 decision
    /// 5): it is a capture type and one of its adapters declares `capture.mapping` for
    /// its target kind. Pointer filters draw only on such a source.
    pub fn publishes_mapping(&self, type_id: &rbs_graph::node::NodeTypeId) -> bool {
        let Some(crate::types::SourceClass::Capture(kind)) = crate::types::class_of(type_id) else {
            return false;
        };
        self.capture_candidates(kind).iter().any(|f| {
            matches!(
                self.capture_capabilities(&f.id()).and_then(|c| c.get(&rbs_capture::caps::MAPPING)),
                Some(rbs_core::capability::CapabilityValue::Set(kinds))
                    if kinds.iter().any(|k| k == kind.as_str())
            )
        })
    }

    /// The input monitor's answer for Input monitoring (ADR-0165 decision 7); none
    /// for another kind or without a monitor.
    pub(crate) fn input_answer(
        &self,
        kind: crate::permissions::PermissionKind,
    ) -> Option<crate::permissions::AdapterAnswer> {
        let monitor = self.input_for(kind)?;
        let caps = monitor.capabilities();
        let flag = |key| matches!(caps.get(&key), Some(CapabilityValue::Flag(true)));
        Some(crate::permissions::AdapterAnswer {
            answer: monitor.permission().into(),
            has_permission: flag(rbs_input::caps::PERMISSION),
            can_prompt: flag(rbs_input::caps::PERMISSION_PROMPT),
            restart: flag(rbs_input::caps::PERMISSION_RESTART),
            consent_per_pick: false,
        })
    }

    /// The monitor to ask when Input monitoring is requested: one that was not asked
    /// yet and can prompt.
    pub(crate) fn input_request(
        &self,
        kind: crate::permissions::PermissionKind,
    ) -> Option<rbs_input::InputMonitor> {
        let answer = self.input_answer(kind)?;
        (answer.can_prompt && answer.answer == crate::permissions::Answer::NotDetermined)
            .then(|| self.input_for(kind).cloned())
            .flatten()
    }

    fn input_for(
        &self,
        kind: crate::permissions::PermissionKind,
    ) -> Option<&rbs_input::InputMonitor> {
        (kind == crate::permissions::PermissionKind::InputMonitoring)
            .then_some(self.input.as_ref())
            .flatten()
    }
}
