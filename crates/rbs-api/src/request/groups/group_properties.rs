//! Wire types for the group Properties operations `group.get` and `group.apply` and
//! the topics `group.changed`, `group.render_state` and `group.scroll_finished`
//! (protocol 1.111, ADR-0184, ADR-0185).

use serde::{Deserialize, Deserializer, Serialize};

use super::super::{Axis, is_main, is_zero_u64};
use crate::types::serde_defaults::{default_ramp_ms, is_default, is_default_ramp_ms, is_zero_f32};
use crate::types::{OutputCanvas, ScrollDirection, ScrollRepeat, ScrollStart};

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

/// Group-owned Scroll settings. Unlike item Scroll, these have no `on_return`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GroupScroll {
    /// Units per second, 1 to 4000.
    pub speed: f32,
    /// Direction of motion.
    #[serde(default, skip_serializing_if = "is_default")]
    pub direction: ScrollDirection,
    /// Gap between copies in group layout units.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub gap: f32,
    /// Whether to loop or cross once.
    #[serde(default, skip_serializing_if = "is_default")]
    pub repeat: ScrollRepeat,
    /// Where the picture starts.
    #[serde(default, skip_serializing_if = "is_default")]
    pub start: ScrollStart,
    /// Edge fade in group layout units.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub fade: f32,
    /// Ramp duration in milliseconds.
    #[serde(
        default = "default_ramp_ms",
        skip_serializing_if = "is_default_ramp_ms"
    )]
    pub ramp_ms: u32,
}

/// One all-or-none group operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GroupOperation {
    /// Set or clear every member's timed-show duration.
    SetShowFor {
        /// Duration in milliseconds, or null to clear.
        #[serde(deserialize_with = "required_nullable")]
        show_for_ms: Option<u32>,
    },
    /// Start cues for hidden members.
    Show {},
    /// Set all members' visibility.
    SetVisible {
        /// Persistent visibility.
        visible: bool,
    },
    /// Set all members' lock state.
    SetLocked {
        /// Persistent lock state.
        locked: bool,
    },
    /// Remove all members from the scene.
    RemoveMembers {},
    /// Translate all members on the selected canvas.
    Move {
        /// Horizontal offset in canvas pixels.
        dx: f32,
        /// Vertical offset in canvas pixels.
        dy: f32,
    },
    /// Map the group's transform box to a new rectangle.
    Resize {
        /// Left edge in canvas pixels.
        x: f32,
        /// Top edge in canvas pixels.
        y: f32,
        /// Positive width.
        width: f32,
        /// Positive height.
        height: f32,
    },
    /// Mirror the arrangement about its centre.
    Flip {
        /// Axis to mirror.
        axis: Axis,
    },
    /// Set or clear Scroll for the selected canvas.
    SetScroll {
        /// Scroll settings, or null to clear.
        #[serde(deserialize_with = "required_nullable")]
        scroll: Option<GroupScroll>,
    },
    /// Pause the group's live Scroll run.
    ScrollPause {},
    /// Resume the group's live Scroll run.
    ScrollResume {},
    /// Restart the group's live Scroll run.
    ScrollRestart {},
    /// Change the live run's target speed.
    ScrollSpeed {
        /// Speed in group layout units per second.
        speed: f32,
    },
}

impl GroupOperation {
    /// Live actions do not enter undo history and must not carry a gesture.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Show {}
                | Self::ScrollPause {}
                | Self::ScrollResume {}
                | Self::ScrollRestart {}
                | Self::ScrollSpeed { .. }
        )
    }
}

/// `group.apply`: one atomic group edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ApplyGroup {
    /// Scene containing the group.
    pub scene: u32,
    /// Group id in the scene.
    pub group: u32,
    /// Complete expected member item ids.
    pub items: Vec<u32>,
    /// Canvas whose layout is edited.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
    /// The operation to perform.
    pub operation: GroupOperation,
    /// Consecutive saved edits with the same nonzero gesture can coalesce.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub gesture: u64,
}

/// `group.get`: a group's state on one canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GetGroup {
    /// Scene containing the group.
    pub scene: u32,
    /// Group id in the scene.
    pub group: u32,
    /// Canvas whose state is requested.
    #[serde(default, skip_serializing_if = "is_main")]
    pub canvas: OutputCanvas,
}

/// One group member's persistent and live state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupMemberState {
    /// Item id, not source id.
    pub item: u32,
    /// Persistent visibility.
    pub visible: bool,
    /// Persistent lock state.
    pub locked: bool,
    /// Timed-show duration, or null when unset.
    pub show_for_ms: Option<u32>,
    /// Whether this edited-scene item has an active cue.
    pub cued: bool,
    /// Remaining cue time at the read instant, or null.
    pub remaining_ms: Option<u32>,
    /// Whether the direct member can supply a picture.
    pub has_picture: bool,
}

/// A group box in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupBounds {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Positive width.
    pub width: f32,
    /// Positive height.
    pub height: f32,
}

/// Live state of the edited scene's group Scroll run.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupRunState {
    /// Run incarnation, unique for the API server's lifetime.
    pub id: u64,
    /// Whether the run is paused.
    pub paused: bool,
    /// Target speed, including a speed queued while paused.
    pub speed: f32,
}

/// Which rendered occurrence has failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GroupRenderRole {
    /// Program.
    Program,
    /// Preview.
    Preview,
    /// Incoming transition.
    Incoming,
    /// Outgoing transition.
    Outgoing,
    /// Live hold scene.
    Hold,
}

/// Recoverable group-composition failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GroupRenderFailureReason {
    /// Bounds are invalid.
    InvalidBounds,
    /// Bounds exceed a per-texture limit.
    TooLarge,
    /// The studio-wide budget is exhausted.
    Budget,
    /// The intermediate could not be allocated.
    Allocation,
    /// The intermediate could not be rendered.
    Render,
}

/// Current failure for one rendered occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupRenderFailure {
    /// Opaque occurrence identifier.
    pub occurrence: String,
    /// Consumer role.
    pub role: GroupRenderRole,
    /// Reason for the failure.
    pub reason: GroupRenderFailureReason,
}

/// Complete group state for the selected canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupState {
    /// Scene containing the group.
    pub scene: u32,
    /// Group id.
    pub group: u32,
    /// Display name.
    pub name: String,
    /// Current member item ids, top first.
    pub items: Vec<u32>,
    /// Member state in the same order as `items`.
    pub members: Vec<GroupMemberState>,
    /// Canvas whose state is reported.
    pub canvas: OutputCanvas,
    /// Position-and-size union for group transforms.
    pub bounds: Option<GroupBounds>,
    /// Rotated picture union for the Scroll window.
    pub scroll_bounds: Option<GroupBounds>,
    /// Saved group Scroll settings, if present.
    pub scroll: Option<GroupScroll>,
    /// Matching live run, if present.
    pub run: Option<GroupRunState>,
    /// Currently failing rendered occurrences.
    pub render_failures: Vec<GroupRenderFailure>,
}

/// A removed `group.changed` key: the group, its scene or its canvas is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GroupRemoved {
    /// Scene that contained the group.
    pub scene: u32,
    /// Group id.
    pub group: u32,
    /// Canvas of the key.
    pub canvas: OutputCanvas,
    /// Always true.
    pub removed: bool,
}

/// `group.changed` data: the key's `group.get` answer, or its removal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum GroupChanged {
    /// The group is gone from this key.
    Removed(GroupRemoved),
    /// The group's current state.
    State(GroupState),
}

/// `group.render_state` data: the complete current failures of one key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupRenderState {
    /// Scene containing the group.
    pub scene: u32,
    /// Group id.
    pub group: u32,
    /// Canvas of the key.
    pub canvas: OutputCanvas,
    /// Every failing occurrence; empty reports recovery.
    pub failures: Vec<GroupRenderFailure>,
}

/// `group.scroll_finished` data: a group `Once` run ended, once per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupScrollFinished {
    /// The originating scene (a held copy's original).
    pub scene: u32,
    /// Group id.
    pub group: u32,
    /// Canvas of the run.
    pub canvas: OutputCanvas,
    /// The run id `group.get` reported.
    pub run_id: u64,
}
