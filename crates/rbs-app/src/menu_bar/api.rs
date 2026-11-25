//! Resolve the menu's contextual targets without using its local dispatch path.

use super::*;
use rbs_api::{ApiError, Command, ErrorKind, Request};
use serde_json::{Value, json};

#[cfg(test)]
mod tests;

pub(crate) enum Resolution {
    Engine(Command),
    Batch(Vec<Command>),
    Ui(Run),
    Picker(AddWhat),
}

pub(crate) fn call(method: &str, params: Value) -> Result<Command, ApiError> {
    let request: Request = serde_json::from_value(json!({"method": method, "params": params}))
        .map_err(|error| ApiError::new(ErrorKind::InvalidParams, error.to_string()))?;
    match request {
        Request::Command(command) => Ok(command),
        _ => Err(ApiError::new(
            ErrorKind::InvalidRequest,
            "not an engine operation",
        )),
    }
}

pub(crate) fn resolve(text: &str, state: &Shown<'_>) -> Result<Resolution, ApiError> {
    let id = CommandId::parse(text)
        .map_err(|error| ApiError::new(ErrorKind::InvalidParams, error.to_string()))?;
    id.resolve()
        .map_err(|error| ApiError::new(ErrorKind::InvalidParams, error.to_string()))?
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "unknown registry id"))?;
    state.enabled(&id).map_err(|reason| {
        let mut data = serde_json::Map::new();
        data.insert("reason".into(), reason.to_string().into());
        ApiError::with_data(ErrorKind::Conflict, "command is disabled", data)
    })?;
    let run = run(text, state).ok_or_else(|| {
        ApiError::new(ErrorKind::InvalidParams, "choose a value for this command")
    })?;
    resolve_run(run, state)
}

fn resolve_run(run: Run, state: &Shown<'_>) -> Result<Resolution, ApiError> {
    use Run::*;
    let scene = state.studio.scene_target(None);
    let main_or_all = |all| if all { "all" } else { "main" };
    let pair = match run {
        AddSource(what) => return Ok(Resolution::Picker(what)),
        UndoEdit(back) => (if back { "edit.undo" } else { "edit.redo" }, json!({})),
        Profile(id) => ("profile.switch", json!({"id": id})),
        Collection(id) => ("collection.switch", json!({"id": id})),
        Scene(id) => {
            if state.studio.studio_mode {
                ("canvas.set_preview", json!({"scene": id}))
            } else {
                ("scene.switch", json!({"scene": id}))
            }
        }
        StudioMode(on) => (
            "canvas.set_preview",
            json!({"scene": if on { scene } else { None }}),
        ),
        Transition => ("canvas.transition", json!({})),
        Rehearse => ("canvas.rehearse", json!({})),
        DirectTransition(kind) => (
            "collection.set_transition",
            json!({"direct_kind": kind.as_str()}),
        ),
        AddScene => ("scene.add", json!({})),
        DuplicateScene(id) => ("scene.duplicate", json!({"scene": id, "sources": "shared"})),
        DuplicateSceneWithCopies(id) => {
            ("scene.duplicate", json!({"scene": id, "sources": "copied"}))
        }
        DuplicateSource(id) => ("source.duplicate", json!({"source": id, "scene": scene})),
        CopyAttributes(id) => ("source.copy_attributes", json!({"source": id})),
        RenameScene(id, Some(name)) => ("scene.rename", json!({"scene": id, "name": name})),
        RenameSource(id, Some(name)) => ("source.rename", json!({"source": id, "name": name})),
        RemoveScene(id) => ("scene.remove", json!({"scene": id})),
        ReloadSource(id) => ("source.reload", json!({"source": id})),
        DeleteSource(id) => ("source.delete", json!({"source": id})),
        RemoveFromScene(id) => ("item.remove", json!({"item": item(state, id)?})),
        MoveSource(id, direction) => (
            "item.move",
            json!({"item": item(state, id)?, "direction": direction}),
        ),
        MoveScene(id, direction) => ("scene.move", json!({"scene": id, "direction": direction})),
        SetSourceActive(id, active) => {
            ("source.set_active", json!({"source": id, "active": active}))
        }
        SourceVisible(id, visible) => (
            "item.set_visible",
            json!({"item": item(state, id)?, "visible": visible}),
        ),
        SourceLocked(id, locked) => (
            "item.set_locked",
            json!({"item": item(state, id)?, "locked": locked}),
        ),
        SourceMuted(id, muted) => ("source.set_muted", json!({"source": id, "muted": muted})),
        SourceMixerHidden(id, hidden) => (
            "source.set_mixer_hidden",
            json!({"source": id, "hidden": hidden}),
        ),
        SourceMonitor(id, mode) => (
            "source.set_monitor",
            json!({"source": id, "monitor": mode.as_str()}),
        ),
        SourceSolo(id, on) => (
            "monitor.set_solo",
            json!({"sources": state.studio.solo_sources(id, on)}),
        ),
        LoudnessShown(on) => ("loudness.set_enabled", json!({"on": on})),
        SceneOwnLevels(id, own) => (
            if own {
                "scene.audio.own"
            } else {
                "scene.audio.shared"
            },
            json!({"scene": id}),
        ),
        ResetSceneLevel(scene, source) => (
            "scene.audio.reset_source",
            json!({"scene": scene, "source": source}),
        ),
        ResetLoudness => ("loudness.reset", json!({})),
        GoLive(all) => ("stream.start", json!({"canvas": main_or_all(all)})),
        AskEndStream(all) => ("stream.stop", json!({"canvas": main_or_all(all)})),
        Record(all) => (
            "record.start",
            json!({"canvas": main_or_all(all), "profile": rbs_app::record_profile::API_PROFILE_ID}),
        ),
        StopRecording(all) => ("record.stop", json!({"canvas": main_or_all(all)})),
        WithVertical(kind, on) => ("output.set_with_vertical", json!({"kind": kind, "on": on})),
        RemoveVertical => ("canvas.remove_vertical", json!({})),
        AutoLayoutVertical(mode) => (
            "canvas.auto_layout",
            json!({"canvas": "vertical", "mode": mode}),
        ),
        VerticalSize(width, height) => (
            "canvas.set_vertical_size",
            json!({"width": width, "height": height}),
        ),
        GoLiveVertical => ("stream.start", json!({"canvas": "vertical"})),
        AskEndVerticalStream => ("stream.stop", json!({"canvas": "vertical"})),
        RecordVertical => (
            "record.start",
            json!({"canvas": "vertical", "profile": rbs_app::record_profile::API_PROFILE_ID}),
        ),
        StopVerticalRecording => ("record.stop", json!({"canvas": "vertical"})),
        VirtualCamera(on) => (if on { "vcam.start" } else { "vcam.stop" }, json!({})),
        Replay(on) => (if on { "replay.arm" } else { "replay.disarm" }, json!({})),
        SaveReplay => ("replay.save", json!({})),
        AddMarker => ("marker.add", json!({})),
        PauseRecording => ("record.pause_toggle", json!({"canvas": "all"})),
        HoldStream => ("stream.hold_toggle", json!({"canvas": "all"})),
        Break => ("output.break_toggle", json!({})),
        AddFilter(source, type_id) => ("filter.add", json!({"source": source, "type": type_id})),
        Transform(source, op, canvas) => return transform(state, source, op, canvas),
        RenameScene(_, None)
        | RenameSource(_, None)
        | SaveWorkspaceAs(None)
        | OpenAttributes(..)
        | SceneTransition(_)
        | AddVertical => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "this command needs input at the window",
            ));
        }
        Arrange(op) => return arrange_call(state, op).map(Resolution::Engine),
        Group(group, op) => {
            return state
                .studio
                .external_group_calls(group, op)
                .map(Resolution::Batch);
        }
        MixerBulk(bulk) => return Ok(Resolution::Batch(state.studio.external_mixer_calls(bulk))),
        ui => return Ok(Resolution::Ui(ui)),
    };
    call(pair.0, pair.1).map(Resolution::Engine)
}

fn item(state: &Shown<'_>, source: i32) -> Result<u32, ApiError> {
    state
        .studio
        .source_facts(Some(&source.to_string()), None)
        .filter(|facts| facts.item > 0)
        .map(|facts| facts.item)
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                "source item is no longer in this scene",
            )
        })
}

fn transform(
    state: &Shown<'_>,
    source: i32,
    op: TransformOp,
    canvas: Option<OutputCanvas>,
) -> Result<Resolution, ApiError> {
    let item = item(state, source)?;
    let canvas = canvas.unwrap_or_else(|| state.studio.props_target(source));
    let canvas = if canvas == OutputCanvas::Main {
        "main"
    } else {
        "vertical"
    };
    use TransformOp::*;
    let (method, mut params) = match op {
        FlipHorizontal => ("item.flip", json!({"axis": "horizontal"})),
        FlipVertical => ("item.flip", json!({"axis": "vertical"})),
        Turn(degrees) => ("item.rotate", json!({"degrees": degrees.round() as i32})),
        SetAngle(degrees) => ("item.set_rotation", json!({"degrees": degrees})),
        Fit(mode) => (
            "item.fit",
            json!({"mode": match mode {
                rbs_scene::FitMode::Fit => "fit",
                rbs_scene::FitMode::Fill => "fill",
                rbs_scene::FitMode::Stretch => "stretch",
            }}),
        ),
        Center(axis) => (
            "item.align",
            json!({"axis": match axis {
                rbs_scene::CenterAxis::Horizontal => "horizontal",
                rbs_scene::CenterAxis::Vertical => "vertical",
                rbs_scene::CenterAxis::Both => "both",
            }}),
        ),
        Reset => ("item.reset_transform", json!({})),
    };
    params["item"] = item.into();
    params["canvas"] = canvas.into();
    call(method, params).map(Resolution::Engine)
}

fn arrange_call(state: &Shown<'_>, op: ArrangeOp) -> Result<Command, ApiError> {
    let refused = |reason: &str| ApiError::new(ErrorKind::Conflict, reason);
    let scene = state.studio.scene_target(None);
    let canvas = state.studio.props_target(state.studio.selected);
    match op {
        ArrangeOp::Group(true) => call(
            "group.create",
            json!({
                "scene": scene, "items": state.studio.group_items().map_err(refused)?,
            }),
        ),
        ArrangeOp::Group(false) => call(
            "group.remove",
            json!({
                "scene": scene, "group": state.studio.ungroup_target().map_err(refused)?,
            }),
        ),
        ArrangeOp::Align(edge) => call(
            "item.align_items",
            json!({
                "items": state.studio.arrange_items(op).map_err(refused)?,
                "canvas": canvas, "edge": edge, "to": "selection",
            }),
        ),
        ArrangeOp::Distribute(axis) => call(
            "item.distribute",
            json!({
                "items": state.studio.arrange_items(op).map_err(refused)?,
                "canvas": canvas, "axis": axis,
            }),
        ),
    }
}
