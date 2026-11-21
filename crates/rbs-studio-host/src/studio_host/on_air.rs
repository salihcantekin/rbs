//! The on-air check of the control API (ADR-0052 decision 4, `not_permitted` with
//! reason `on_air`): while any output runs, a caller without "Allow actions that
//! cannot be undone" may not make an `edit` that changes what is on air.
//!
//! It fails closed: a method is allowed only when it is known not to touch the
//! picture or sound on air, or when every scene, item, source and filter it names is
//! off air (`SceneCollection::on_air`) and it does not default to the program scene.
//! Every other `edit` is refused while an output runs.

use rbs_api::request::Effect;
use rbs_api::{ApiError, Command, ErrorKind};
use rbs_scene::OnAir;
use rbs_scene::model::{FilterId, ItemId, SceneId, SourceId};
use serde_json::Value;

use super::Host;

/// `edit` methods that never change the picture or sound on air: local monitoring,
/// output settings for the next start, new files, markers, the studio-mode preview,
/// and adding an empty scene.
const OFF_AIR: &[&str] = &[
    "settings.set",
    "monitor.set_device",
    "monitor.set_volume",
    "monitor.set_solo",
    "output.set_with_vertical",
    "replay.save",
    "project.export",
    "marker.add",
    "canvas.set_preview",
    "canvas.rehearse",
    "scene.add",
    "source.copy_attributes",
];

/// `edit` methods that act only on the scenes, items, sources and filters they name:
/// allowed when all of them are off air.
const BY_TARGET: &[&str] = &[
    "item.set_style",
    "item.set_crop",
    "item.set_shown_region",
    "item.flip",
    "item.rotate",
    "item.fit",
    "item.align",
    "item.reset_transform",
    "item.set_rotation",
    "item.set_visible",
    "item.set_locked",
    "item.set_show_for",
    "item.show",
    "item.remove",
    "item.move",
    "item.set_box",
    "item.add",
    "source.add",
    "source.add_sample",
    "source.duplicate",
    "source.delete",
    "source.rename",
    "source.set_volume",
    "source.set_muted",
    "source.set_mixer_hidden",
    "source.set_settings",
    "source.set_companion_audio",
    "source.retarget",
    "source.reload",
    "source.set_active",
    "source.set_monitor",
    "source.set_glide",
    "source.set_region",
    "source.set_record_separately",
    "source.set_chrome",
    "source.fit_chrome",
    "source.reset_chrome",
    "filter.add",
    "filter.remove",
    "filter.move",
    "filter.set_enabled",
    "filter.set_settings",
    "scene.remove",
    "scene.rename",
    "scene.set_transition",
    "scene.duplicate",
    "scene.move",
    "scene.audio.set",
    "scene.audio.own",
    "scene.audio.shared",
    "scene.audio.reset_source",
];

/// Methods that place something into a scene and default to the program scene when
/// the call names none.
const PLACES: &[&str] = &[
    "item.add",
    "source.add",
    "source.add_sample",
    "source.duplicate",
];

/// What a call names, read from its wire parameters.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Targets {
    scenes: Vec<u32>,
    items: Vec<u32>,
    sources: Vec<u32>,
    filters: Vec<u32>,
}

impl Targets {
    fn of(params: &Value) -> Self {
        let mut t = Self::default();
        let Some(map) = params.as_object() else {
            return t;
        };
        for (key, list) in [
            ("scene", &mut t.scenes),
            ("item", &mut t.items),
            ("source", &mut t.sources),
            ("filter", &mut t.filters),
        ] {
            if let Some(id) = map.get(key).and_then(id_of) {
                list.push(id);
            }
        }
        t
    }

    fn is_empty(&self) -> bool {
        self.scenes.is_empty()
            && self.items.is_empty()
            && self.sources.is_empty()
            && self.filters.is_empty()
    }
}

fn id_of(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}

/// Whether `command` (an `edit`) may change what is on air in `on_air`, failing
/// closed. `filter_source` finds a filter's source.
pub(super) fn changes_on_air(
    command: &Command,
    on_air: &OnAir,
    filter_source: &dyn Fn(FilterId) -> Option<SourceId>,
) -> bool {
    let method = command.method();
    if OFF_AIR.contains(&method) {
        return false;
    }
    if !BY_TARGET.contains(&method) {
        return true;
    }
    let t = Targets::of(&command.params_json());
    if t.is_empty() || (PLACES.contains(&method) && t.scenes.is_empty()) {
        return true;
    }
    t.scenes
        .iter()
        .any(|s| on_air.scenes.contains(&SceneId::from_raw(*s)))
        || t.items
            .iter()
            .any(|i| on_air.items.contains(&ItemId::from_raw(*i)))
        || t.sources
            .iter()
            .any(|s| on_air.sources.contains(&SourceId::from_raw(*s)))
        || t.filters.iter().any(|f| {
            filter_source(FilterId::from_raw(*f)).is_none_or(|s| on_air.sources.contains(&s))
        })
}

/// The refusal (`not_permitted`, reason `on_air`).
fn refusal(method: &str) -> ApiError {
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), "on_air".into());
    ApiError::with_data(
        ErrorKind::NotPermitted,
        format!(
            "{method} would change what is on air while an output runs, and this token is not allowed actions that cannot be undone"
        ),
        data,
    )
}

impl Host {
    /// A recording, a stream, the virtual camera or the replay buffer runs. Then
    /// every canvas counts as on air (stricter than per canvas, and simple).
    pub(super) fn output_running(&self) -> bool {
        self.outs.iter().any(|o| o.busy())
            || self.vcam.camera.is_some()
            || self.replay_state().armed
    }

    /// Refuses `command` from a caller without the irreversible-actions permission
    /// when it is an `edit` that changes what is on air (ADR-0052 decision 4).
    pub(super) fn check_on_air(
        &self,
        irreversible: bool,
        command: &Command,
    ) -> Result<(), ApiError> {
        if irreversible || command.effect() != Effect::Edit || !self.output_running() {
            return Ok(());
        }
        let on_air = self.collection.on_air();
        let filter_source = |f: FilterId| self.filter_owner(f).ok().map(|(s, _)| s);
        if changes_on_air(command, &on_air, &filter_source) {
            return Err(refusal(command.method()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;
    use rbs_api::request::{METHODS, method_effect};

    #[test]
    fn listed_methods_exist_and_are_edits() {
        for m in OFF_AIR.iter().chain(BY_TARGET).chain(PLACES) {
            assert_eq!(method_effect(m), Some(Effect::Edit), "{m}");
        }
        for m in PLACES {
            assert!(BY_TARGET.contains(m), "{m}");
        }
        assert!(METHODS.iter().any(|m| m.effect == Effect::Edit));
    }

    #[test]
    fn targets_are_read_from_the_wire_parameters() {
        let t = Targets::of(&serde_json::json!({"scene": 3, "source": 7, "name": "x"}));
        assert_eq!(t.scenes, vec![3]);
        assert_eq!(t.sources, vec![7]);
        assert!(t.items.is_empty() && t.filters.is_empty());
    }
}
