//! Generated JSON Schema metadata for built-in control API methods.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
use crate::request::*;
#[cfg(feature = "schema")]
use schemars::generate::{Contract, SchemaSettings};

#[cfg(feature = "schema")]
#[derive(schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyResult {}

#[cfg(feature = "schema")]
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum OutputCommandResult {
    Single(EmptyResult),
    All(crate::types::OutputsResult),
}

#[cfg(feature = "schema")]
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum RecordStartResult {
    Single(RecordStarted),
    All(crate::types::OutputsResult),
}

#[cfg(feature = "schema")]
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum StreamStartResult {
    Single(StreamStarted),
    All(crate::types::OutputsResult),
}

#[cfg(feature = "schema")]
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum DestinationStartResult {
    Existing(EmptyResult),
    Started(StreamStarted),
}

#[cfg(feature = "schema")]
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum GlobalRemoveResult {
    Kept(ItemAdded),
    Removed(ItemRemoved),
}

#[cfg(feature = "schema")]
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum RecordStatusResult {
    Main(crate::types::Snapshot<crate::types::RecordState>),
    Vertical(crate::types::Snapshot<crate::types::CanvasRecordState>),
}

#[cfg(feature = "schema")]
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum StreamStatusResult {
    Main(crate::types::Snapshot<crate::types::StreamState>),
    Vertical(crate::types::Snapshot<crate::types::CanvasStreamState>),
}

/// Request parameters and successful result for a method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodSchemas {
    /// JSON Schema for the request's `params`.
    pub input_schema: Value,
    /// JSON Schema for the successful `result`.
    pub output_schema: Value,
}

/// The checked-in schemas, one embedded document per namespace (ADR-0196). A shard is
/// parsed on the first request that needs it, never at start-up.
const SHARDS: &[(&str, &str)] = &[
    ("addon", include_str!("../schemas/addon.json")),
    ("api", include_str!("../schemas/api.json")),
    ("browser", include_str!("../schemas/browser.json")),
    ("calibration", include_str!("../schemas/calibration.json")),
    ("canvas", include_str!("../schemas/canvas.json")),
    ("collection", include_str!("../schemas/collection.json")),
    ("edit", include_str!("../schemas/edit.json")),
    ("events", include_str!("../schemas/events.json")),
    ("filter", include_str!("../schemas/filter.json")),
    ("global", include_str!("../schemas/global.json")),
    ("group", include_str!("../schemas/group.json")),
    ("import", include_str!("../schemas/import.json")),
    ("item", include_str!("../schemas/item.json")),
    ("look", include_str!("../schemas/look.json")),
    ("loudness", include_str!("../schemas/loudness.json")),
    ("marker", include_str!("../schemas/marker.json")),
    ("media", include_str!("../schemas/media.json")),
    ("monitor", include_str!("../schemas/monitor.json")),
    ("output", include_str!("../schemas/output.json")),
    ("profile", include_str!("../schemas/profile.json")),
    ("project", include_str!("../schemas/project.json")),
    ("protection", include_str!("../schemas/protection.json")),
    ("record", include_str!("../schemas/record.json")),
    ("replay", include_str!("../schemas/replay.json")),
    ("scene", include_str!("../schemas/scene.json")),
    ("session", include_str!("../schemas/session.json")),
    ("settings", include_str!("../schemas/settings.json")),
    ("source", include_str!("../schemas/source.json")),
    ("stream", include_str!("../schemas/stream.json")),
    ("studio", include_str!("../schemas/studio.json")),
    ("sync", include_str!("../schemas/sync.json")),
    ("text", include_str!("../schemas/text.json")),
    ("tokens", include_str!("../schemas/tokens.json")),
    ("vcam", include_str!("../schemas/vcam.json")),
];

type Shard = Result<BTreeMap<String, MethodSchemas>, String>;

static PARSED: [OnceLock<Shard>; SHARDS.len()] = [const { OnceLock::new() }; SHARDS.len()];

/// How many shards have been parsed so far (for tests of the lazy loading).
#[doc(hidden)]
pub fn parsed_shards() -> usize {
    PARSED.iter().filter(|s| s.get().is_some()).count()
}

/// The namespaces that have a shard.
pub fn shard_namespaces() -> impl Iterator<Item = &'static str> {
    SHARDS.iter().map(|(ns, _)| *ns)
}

/// The shard of `namespace`, parsed now when it is the first request for it. `None`
/// when the namespace has no shard; `Err` (cached) when its snapshot is malformed.
pub fn namespace_schemas(
    namespace: &str,
) -> Option<Result<&'static BTreeMap<String, MethodSchemas>, &'static str>> {
    let at = SHARDS.iter().position(|(ns, _)| *ns == namespace)?;
    let shard = PARSED[at].get_or_init(|| {
        serde_json::from_str(SHARDS[at].1).map_err(|_| "invalid built-in method schemas".to_owned())
    });
    Some(shard.as_ref().map_err(String::as_str))
}

/// The schemas of one method, parsing only its namespace's shard.
pub fn method_schemas(method: &str) -> Option<Result<&'static MethodSchemas, &'static str>> {
    let namespace = method.split('.').next().unwrap_or("");
    match namespace_schemas(namespace)? {
        Ok(map) => map.get(method).map(Ok),
        Err(e) => Some(Err(e)),
    }
}

/// Every shard merged. Parses all of them: for tests and tools, not the server path.
pub fn builtin_schemas() -> Result<BTreeMap<String, MethodSchemas>, serde_json::Error> {
    let mut all = BTreeMap::new();
    for (_, text) in SHARDS {
        let shard: BTreeMap<String, MethodSchemas> = serde_json::from_str(text)?;
        all.extend(shard);
    }
    Ok(all)
}

/// Writes one pretty-printed, key-sorted shard file per namespace into `dir`.
#[cfg(feature = "schema")]
pub fn write_shards(dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut by: BTreeMap<String, BTreeMap<String, MethodSchemas>> = BTreeMap::new();
    for (name, pair) in generate_schemas()? {
        let ns = name.split('.').next().unwrap_or("").to_owned();
        by.entry(ns).or_default().insert(name, pair);
    }
    for (ns, map) in by {
        let mut text = serde_json::to_string_pretty(&map)?;
        text.push('\n');
        std::fs::write(dir.join(format!("{ns}.json")), text)?;
    }
    Ok(())
}

/// Derives the currently supported methods' schemas from their Rust types.
#[cfg(feature = "schema")]
pub fn generate_schemas() -> Result<BTreeMap<String, MethodSchemas>, serde_json::Error> {
    let mut schemas = BTreeMap::new();
    for (name, schema) in first_entries()?
        .into_iter()
        .chain(recent_entries()?)
        .chain(second_entries()?)
        .chain(item_entries()?)
        .chain(media_entries()?)
        .chain(arrange_entries()?)
        .chain(scroll_entries()?)
        .chain(group_entries()?)
        .chain(extra_entries()?)
        .chain(transition_entries()?)
        .chain(glide_entries()?)
        .chain(session_entries()?)
        .chain(list_entries()?)
        .chain(attribute_entries()?)
        .chain(status_entries()?)
    {
        if schemas.insert(name, schema).is_some() {
            return Err(serde_json::Error::io(std::io::Error::other(
                "duplicate schema method",
            )));
        }
    }
    Ok(schemas)
}

#[cfg(feature = "schema")]
fn integer_bounds(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let integer = object.get("type").is_some_and(|kind| {
                kind.as_str() == Some("integer")
                    || kind
                        .as_array()
                        .is_some_and(|types| types.iter().any(|t| t.as_str() == Some("integer")))
            });
            if integer {
                let bounds = match object.get("format").and_then(Value::as_str) {
                    Some("uint8") => Some((0.into(), u8::MAX.into())),
                    Some("uint16") => Some((0.into(), u16::MAX.into())),
                    Some("uint32") => Some((0.into(), u32::MAX.into())),
                    Some("uint64") => Some((0.into(), u64::MAX.into())),
                    Some("int8") => Some((i8::MIN.into(), i8::MAX.into())),
                    Some("int16") => Some((i16::MIN.into(), i16::MAX.into())),
                    Some("int32") => Some((i32::MIN.into(), i32::MAX.into())),
                    Some("int64") => Some((i64::MIN.into(), i64::MAX.into())),
                    _ => None,
                };
                if let Some((min, max)) = bounds {
                    object.entry("minimum").or_insert(Value::Number(min));
                    object.entry("maximum").or_insert(Value::Number(max));
                }
            }
            object.values_mut().for_each(integer_bounds);
        }
        Value::Array(values) => values.iter_mut().for_each(integer_bounds),
        _ => {}
    }
}

#[cfg(feature = "schema")]
fn pair<I: schemars::JsonSchema, O: schemars::JsonSchema>(
    _: fn(I) -> WireRequest,
) -> Result<MethodSchemas, serde_json::Error> {
    let input = SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<I>();
    let mut settings = SchemaSettings::draft2020_12();
    settings.contract = Contract::Serialize;
    let output = settings.into_generator().into_root_schema_for::<O>();
    let mut input_schema = serde_json::to_value(input)?;
    let mut output_schema = serde_json::to_value(output)?;
    integer_bounds(&mut input_schema);
    integer_bounds(&mut output_schema);
    Ok(MethodSchemas {
        input_schema,
        output_schema,
    })
}

#[cfg(feature = "schema")]
fn object_union_pair<I: schemars::JsonSchema, O: ObjectUnion>(
    variant: fn(I) -> WireRequest,
) -> Result<MethodSchemas, serde_json::Error> {
    let mut schemas = pair::<I, O>(variant)?;
    schemas.output_schema["type"] = serde_json::json!("object");
    Ok(schemas)
}

#[cfg(feature = "schema")]
trait ObjectUnion: schemars::JsonSchema {}

#[cfg(feature = "schema")]
impl ObjectUnion for OutputCommandResult {}
#[cfg(feature = "schema")]
impl ObjectUnion for RecordStartResult {}
#[cfg(feature = "schema")]
impl ObjectUnion for StreamStartResult {}
#[cfg(feature = "schema")]
impl ObjectUnion for DestinationStartResult {}
#[cfg(feature = "schema")]
impl ObjectUnion for GlobalRemoveResult {}
#[cfg(feature = "schema")]
impl ObjectUnion for RecordStatusResult {}
#[cfg(feature = "schema")]
impl ObjectUnion for StreamStatusResult {}

#[cfg(feature = "schema")]
fn recent_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "record.status".into(),
            object_union_pair::<OnCanvas, RecordStatusResult>(WireRequest::RecordStatus)?,
        ),
        (
            "stream.status".into(),
            object_union_pair::<OnCanvas, StreamStatusResult>(WireRequest::StreamStatus)?,
        ),
        (
            "settings.get".into(),
            pair::<Empty, crate::types::Snapshot<crate::types::Settings>>(
                WireRequest::SettingsGet,
            )?,
        ),
        (
            "settings.set".into(),
            pair::<SettingsPatch, crate::types::Snapshot<crate::types::Settings>>(
                WireRequest::SettingsSet,
            )?,
        ),
        (
            "api.describe".into(),
            pair::<DescribeParams, Description>(WireRequest::Describe)?,
        ),
        (
            "record.estimate".into(),
            pair::<RecordStart, crate::types::EstimateAnswer>(WireRequest::RecordEstimate)?,
        ),
        (
            "calibration.run".into(),
            pair::<CalibrationRun, Empty>(WireRequest::CalibrationRun)?,
        ),
        (
            "calibration.status".into(),
            pair::<Empty, crate::types::Snapshot<crate::types::CalibrationState>>(
                WireRequest::CalibrationStatus,
            )?,
        ),
        (
            "protection.status".into(),
            pair::<Empty, crate::types::Snapshot<crate::types::ProtectionStatus>>(
                WireRequest::ProtectionStatus,
            )?,
        ),
        (
            "studio.get".into(),
            pair::<Empty, crate::types::Snapshot<crate::types::StudioInfo>>(
                WireRequest::StudioGet,
            )?,
        ),
        (
            "source.add".into(),
            pair::<AddSource, SourceAdded>(WireRequest::SourceAdd)?,
        ),
        (
            "global.remove".into(),
            object_union_pair::<RemoveGlobal, GlobalRemoveResult>(WireRequest::GlobalRemove)?,
        ),
        (
            "output.profiles".into(),
            pair::<Empty, ProfileList>(WireRequest::OutputProfiles)?,
        ),
        (
            "record.start".into(),
            object_union_pair::<RecordStart, RecordStartResult>(WireRequest::RecordStart)?,
        ),
        (
            "record.start_to_file".into(),
            pair::<RecordStartToFile, RecordStarted>(WireRequest::RecordStartToFile)?,
        ),
        (
            "record.stop".into(),
            object_union_pair::<OnCanvases, OutputCommandResult>(WireRequest::RecordStop)?,
        ),
        (
            "record.pause".into(),
            object_union_pair::<OnCanvases, OutputCommandResult>(WireRequest::RecordPause)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn first_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "record.resume".into(),
            object_union_pair::<OnCanvases, OutputCommandResult>(WireRequest::RecordResume)?,
        ),
        (
            "record.pause_toggle".into(),
            object_union_pair::<OnCanvases, OutputCommandResult>(WireRequest::RecordPauseToggle)?,
        ),
        (
            "stream.start".into(),
            object_union_pair::<OnCanvases, StreamStartResult>(WireRequest::StreamStart)?,
        ),
        (
            "stream.stop".into(),
            object_union_pair::<OnCanvases, OutputCommandResult>(WireRequest::StreamStop)?,
        ),
        (
            "stream.hold".into(),
            object_union_pair::<StreamHold, OutputCommandResult>(WireRequest::StreamHold)?,
        ),
        (
            "stream.resume".into(),
            object_union_pair::<OnCanvases, OutputCommandResult>(WireRequest::StreamResume)?,
        ),
        (
            "stream.hold_toggle".into(),
            object_union_pair::<StreamHold, OutputCommandResult>(WireRequest::StreamHoldToggle)?,
        ),
        (
            "stream.destination.start".into(),
            object_union_pair::<DestinationRef, DestinationStartResult>(
                WireRequest::StreamDestinationStart,
            )?,
        ),
        (
            "output.break_toggle".into(),
            pair::<Empty, crate::types::OutputsResult>(WireRequest::OutputBreakToggle)?,
        ),
        (
            "scene.add".into(),
            pair::<AddScene, SceneAdded>(WireRequest::SceneAdd)?,
        ),
        (
            "scene.remove".into(),
            pair::<SceneRef, Empty>(WireRequest::SceneRemove)?,
        ),
        (
            "scene.rename".into(),
            pair::<RenameScene, Empty>(WireRequest::SceneRename)?,
        ),
        (
            "scene.audio.get".into(),
            pair::<SceneRef, SceneLevels>(WireRequest::SceneAudioGet)?,
        ),
        (
            "scene.audio.set".into(),
            pair::<SetSceneLevel, SceneLevelSet>(WireRequest::SceneAudioSet)?,
        ),
        (
            "scene.audio.own".into(),
            pair::<SceneRef, Empty>(WireRequest::SceneAudioOwn)?,
        ),
        (
            "scene.audio.shared".into(),
            pair::<SceneRef, Empty>(WireRequest::SceneAudioShared)?,
        ),
        (
            "scene.audio.reset_source".into(),
            pair::<SceneSourceRef, Empty>(WireRequest::SceneAudioResetSource)?,
        ),
        (
            "source.delete".into(),
            pair::<SourceRef, Empty>(WireRequest::SourceDelete)?,
        ),
        (
            "source.rename".into(),
            pair::<RenameSource, Empty>(WireRequest::SourceRename)?,
        ),
        (
            "source.set_volume".into(),
            pair::<SetSourceVolume, Empty>(WireRequest::SourceSetVolume)?,
        ),
        (
            "source.set_muted".into(),
            pair::<SetSourceMuted, Empty>(WireRequest::SourceSetMuted)?,
        ),
        (
            "source.set_mixer_hidden".into(),
            pair::<SetSourceMixerHidden, Empty>(WireRequest::SourceSetMixerHidden)?,
        ),
        (
            "canvas.add_vertical".into(),
            pair::<AddVerticalCanvas, Empty>(WireRequest::CanvasAddVertical)?,
        ),
        (
            "canvas.remove_vertical".into(),
            pair::<Empty, Empty>(WireRequest::CanvasRemoveVertical)?,
        ),
        (
            "canvas.set_vertical_size".into(),
            pair::<SetVerticalSize, Empty>(WireRequest::CanvasSetVerticalSize)?,
        ),
        (
            "canvas.set_master_volume".into(),
            pair::<SetMasterVolume, Empty>(WireRequest::CanvasSetMasterVolume)?,
        ),
        (
            "canvas.set_master_muted".into(),
            pair::<SetMasterMuted, Empty>(WireRequest::CanvasSetMasterMuted)?,
        ),
        (
            "canvas.set_preview".into(),
            pair::<SetPreview, Empty>(WireRequest::CanvasSetPreview)?,
        ),
        (
            "monitor.set_device".into(),
            pair::<SetMonitorDevice, Empty>(WireRequest::MonitorSetDevice)?,
        ),
        (
            "monitor.set_volume".into(),
            pair::<SetMonitorVolume, Empty>(WireRequest::MonitorSetVolume)?,
        ),
        (
            "monitor.set_solo".into(),
            pair::<SetMonitorSolo, Empty>(WireRequest::MonitorSetSolo)?,
        ),
        (
            "loudness.set_enabled".into(),
            pair::<SetLoudnessEnabled, Empty>(WireRequest::LoudnessSetEnabled)?,
        ),
        (
            "loudness.reset".into(),
            pair::<Empty, Empty>(WireRequest::LoudnessReset)?,
        ),
        (
            "item.set_visible".into(),
            pair::<SetItemVisible, Empty>(WireRequest::ItemSetVisible)?,
        ),
        (
            "item.set_locked".into(),
            pair::<SetItemLocked, Empty>(WireRequest::ItemSetLocked)?,
        ),
        (
            "item.set_show_for".into(),
            pair::<SetItemShowFor, Empty>(WireRequest::ItemSetShowFor)?,
        ),
        (
            "item.show".into(),
            pair::<ShowItem, Empty>(WireRequest::ItemShow)?,
        ),
        (
            "calibration.cancel".into(),
            pair::<Empty, Empty>(WireRequest::CalibrationCancel)?,
        ),
        (
            "replay.arm".into(),
            pair::<Empty, Empty>(WireRequest::ReplayArm)?,
        ),
        (
            "replay.disarm".into(),
            pair::<Empty, Empty>(WireRequest::ReplayDisarm)?,
        ),
        (
            "vcam.stop".into(),
            pair::<Empty, Empty>(WireRequest::VcamStop)?,
        ),
        (
            "edit.undo".into(),
            pair::<Empty, Empty>(WireRequest::EditUndo)?,
        ),
        (
            "edit.redo".into(),
            pair::<Empty, Empty>(WireRequest::EditRedo)?,
        ),
        (
            "edit.undo_delete".into(),
            pair::<Empty, Empty>(WireRequest::EditUndoDelete)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn second_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "vcam.start".into(),
            pair::<Empty, Empty>(WireRequest::VcamStart)?,
        ),
        (
            "replay.save".into(),
            pair::<ReplaySave, Empty>(WireRequest::ReplaySave)?,
        ),
        (
            "profile.switch".into(),
            pair::<EntryRef, Empty>(WireRequest::ProfileSwitch)?,
        ),
        (
            "collection.switch".into(),
            pair::<EntryRef, Empty>(WireRequest::CollectionSwitch)?,
        ),
        (
            "source.set_text".into(),
            pair::<SetText, Empty>(WireRequest::SourceSetText)?,
        ),
        (
            "source.set_region".into(),
            pair::<SetSourceRegion, Empty>(WireRequest::SourceSetRegion)?,
        ),
        (
            "source.get_region".into(),
            pair::<SourceRef, SourceRegionInfo>(WireRequest::SourceGetRegion)?,
        ),
        (
            "source.fit_chrome".into(),
            pair::<SourceRef, Empty>(WireRequest::SourceFitChrome)?,
        ),
        (
            "source.reset_chrome".into(),
            pair::<SourceRef, Empty>(WireRequest::SourceResetChrome)?,
        ),
        (
            "source.reload".into(),
            pair::<ReloadSource, Empty>(WireRequest::SourceReload)?,
        ),
        (
            "source.retarget".into(),
            pair::<RetargetSource, Empty>(WireRequest::SourceRetarget)?,
        ),
        (
            "source.get_settings".into(),
            pair::<GetSourceSettings, SourceSettingsInfo>(WireRequest::SourceGetSettings)?,
        ),
        (
            "source.set_settings".into(),
            pair::<SetSourceSettings, Empty>(WireRequest::SourceSetSettings)?,
        ),
        (
            "source.set_sync_offset".into(),
            pair::<SetSourceSyncOffset, Empty>(WireRequest::SourceSetSyncOffset)?,
        ),
        (
            "source.set_active".into(),
            pair::<SetSourceActive, Empty>(WireRequest::SourceSetActive)?,
        ),
        (
            "source.add_sample".into(),
            pair::<AddSample, SourceAdded>(WireRequest::SourceAddSample)?,
        ),
        (
            "sync.measure_start".into(),
            pair::<SyncMeasureStart, SyncMeasureStarted>(WireRequest::SyncMeasureStart)?,
        ),
        (
            "sync.measure_cancel".into(),
            pair::<SyncMeasureCancel, Empty>(WireRequest::SyncMeasureCancel)?,
        ),
        (
            "look.list".into(),
            pair::<Empty, LookList>(WireRequest::LookList)?,
        ),
        (
            "look.apply".into(),
            pair::<ApplyLook, Empty>(WireRequest::LookApply)?,
        ),
        (
            "item.add".into(),
            pair::<AddItem, ItemAdded>(WireRequest::ItemAdd)?,
        ),
        (
            "filter.add".into(),
            pair::<AddFilter, FilterAdded>(WireRequest::FilterAdd)?,
        ),
        (
            "filter.remove".into(),
            pair::<FilterRef, Empty>(WireRequest::FilterRemove)?,
        ),
        (
            "filter.move".into(),
            pair::<MoveFilter, Empty>(WireRequest::FilterMove)?,
        ),
        (
            "filter.set_enabled".into(),
            pair::<SetFilterEnabled, Empty>(WireRequest::FilterSetEnabled)?,
        ),
        (
            "filter.set_settings".into(),
            pair::<SetFilterSettings, Empty>(WireRequest::FilterSetSettings)?,
        ),
        (
            "browser.reload".into(),
            pair::<BrowserReload, Empty>(WireRequest::BrowserReload)?,
        ),
        (
            "browser.run_script".into(),
            pair::<BrowserRunScript, Empty>(WireRequest::BrowserRunScript)?,
        ),
        (
            "browser.forget_sign_in".into(),
            pair::<BrowserForgetSignIn, Empty>(WireRequest::BrowserForgetSignIn)?,
        ),
        (
            "tokens.revoke".into(),
            pair::<TokenRef, Empty>(WireRequest::TokensRevoke)?,
        ),
        (
            "addon.install".into(),
            pair::<AddOnRef, Empty>(WireRequest::AddonInstall)?,
        ),
        (
            "addon.update".into(),
            pair::<AddOnRef, Empty>(WireRequest::AddonUpdate)?,
        ),
        (
            "addon.cancel".into(),
            pair::<AddOnRef, Empty>(WireRequest::AddonCancel)?,
        ),
        (
            "addon.clean".into(),
            pair::<AddOnRef, Empty>(WireRequest::AddonClean)?,
        ),
        (
            "addon.remove".into(),
            pair::<AddOnRef, Empty>(WireRequest::AddonRemove)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn item_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "item.flip".into(),
            pair::<FlipItem, Empty>(WireRequest::ItemFlip)?,
        ),
        (
            "item.rotate".into(),
            pair::<RotateItem, Empty>(WireRequest::ItemRotate)?,
        ),
        (
            "item.fit".into(),
            pair::<FitItem, Empty>(WireRequest::ItemFit)?,
        ),
        (
            "item.align".into(),
            pair::<AlignItem, Empty>(WireRequest::ItemAlign)?,
        ),
        (
            "item.reset_transform".into(),
            pair::<ResetItemTransform, Empty>(WireRequest::ItemResetTransform)?,
        ),
        (
            "item.set_rotation".into(),
            pair::<SetItemRotation, Empty>(WireRequest::ItemSetRotation)?,
        ),
        (
            "item.move".into(),
            pair::<MoveItem, Empty>(WireRequest::ItemMove)?,
        ),
        (
            "scene.move".into(),
            pair::<MoveScene, Empty>(WireRequest::SceneMove)?,
        ),
        (
            "item.fit_text_box".into(),
            pair::<FitTextBox, Empty>(WireRequest::ItemFitTextBox)?,
        ),
        (
            "item.set_filled".into(),
            pair::<SetItemFilled, Empty>(WireRequest::ItemSetFilled)?,
        ),
        (
            "item.set_shown_region".into(),
            pair::<SetItemShownRegion, Empty>(WireRequest::ItemSetShownRegion)?,
        ),
        (
            "collection.set_stinger_file".into(),
            pair::<SetStingerFile, SourceAdded>(WireRequest::CollectionSetStingerFile)?,
        ),
        (
            "canvas.rehearse".into(),
            pair::<Rehearse, Empty>(WireRequest::CanvasRehearse)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn media_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "media.play".into(),
            pair::<MediaSource, MediaStateInfo>(WireRequest::MediaPlay)?,
        ),
        (
            "media.pause".into(),
            pair::<MediaSource, MediaStateInfo>(WireRequest::MediaPause)?,
        ),
        (
            "media.seek".into(),
            pair::<MediaSeek, MediaStateInfo>(WireRequest::MediaSeek)?,
        ),
        (
            "media.restart".into(),
            pair::<MediaSource, MediaStateInfo>(WireRequest::MediaRestart)?,
        ),
        (
            "media.state".into(),
            pair::<MediaSource, MediaStateInfo>(WireRequest::MediaState)?,
        ),
        (
            "media.next".into(),
            pair::<MediaSource, MediaStateInfo>(WireRequest::MediaNext)?,
        ),
        (
            "media.previous".into(),
            pair::<MediaSource, MediaStateInfo>(WireRequest::MediaPrevious)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn arrange_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "item.align_items".into(),
            pair::<AlignItems, Empty>(WireRequest::ItemAlignItems)?,
        ),
        (
            "item.distribute".into(),
            pair::<DistributeItems, Empty>(WireRequest::ItemDistribute)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn group_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "group.create".into(),
            pair::<CreateGroup, GroupCreated>(WireRequest::GroupCreate)?,
        ),
        (
            "group.remove".into(),
            pair::<GroupRef, Empty>(WireRequest::GroupRemove)?,
        ),
        (
            "group.rename".into(),
            pair::<RenameGroup, Empty>(WireRequest::GroupRename)?,
        ),
        (
            "group.list".into(),
            pair::<SceneRef, GroupList>(WireRequest::GroupList)?,
        ),
        (
            "group.get".into(),
            pair::<GetGroup, GroupState>(WireRequest::GroupGet)?,
        ),
        (
            "group.apply".into(),
            pair::<ApplyGroup, Empty>(WireRequest::GroupApply)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn scroll_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "item.set_scroll".into(),
            pair::<SetItemScroll, Empty>(WireRequest::ItemSetScroll)?,
        ),
        (
            "item.scroll_pause".into(),
            pair::<ScrollItem, Empty>(WireRequest::ItemScrollPause)?,
        ),
        (
            "item.scroll_resume".into(),
            pair::<ScrollItem, Empty>(WireRequest::ItemScrollResume)?,
        ),
        (
            "item.scroll_restart".into(),
            pair::<ScrollItem, Empty>(WireRequest::ItemScrollRestart)?,
        ),
        (
            "item.scroll_speed".into(),
            pair::<SetScrollSpeed, Empty>(WireRequest::ItemScrollSpeed)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn extra_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "source.set_companion_audio".into(),
            pair::<SetCompanionAudio, Empty>(WireRequest::SourceSetCompanionAudio)?,
        ),
        (
            "source.set_activity".into(),
            pair::<SetSourceActivity, Empty>(WireRequest::SourceSetActivity)?,
        ),
        (
            "source.get_activity".into(),
            pair::<SourceRef, SourceActivityInfo>(WireRequest::SourceGetActivity)?,
        ),
        (
            "source.set_record_separately".into(),
            pair::<SetRecordSeparately, Empty>(WireRequest::SourceSetRecordSeparately)?,
        ),
        (
            "source.set_chrome".into(),
            pair::<SetSourceChrome, Empty>(WireRequest::SourceSetChrome)?,
        ),
        (
            "source.get_chrome".into(),
            pair::<SourceRef, SourceChromeInfo>(WireRequest::SourceGetChrome)?,
        ),
        (
            "source.set_monitor".into(),
            pair::<SetSourceMonitor, Empty>(WireRequest::SourceSetMonitor)?,
        ),
        (
            "sync.apply_to_device_sources".into(),
            pair::<SyncApplyToDeviceSources, SyncAppliedToDeviceSources>(
                WireRequest::SyncApplyToDeviceSources,
            )?,
        ),
        (
            "item.remove".into(),
            pair::<ItemRef, ItemRemoved>(WireRequest::ItemRemove)?,
        ),
        (
            "filter.add_voice_chain".into(),
            pair::<SourceRef, Empty>(WireRequest::FilterAddVoiceChain)?,
        ),
        (
            "marker.add".into(),
            pair::<MarkerAdd, MarkersAdded>(WireRequest::MarkerAdd)?,
        ),
        (
            "marker.remove".into(),
            pair::<MarkerRemove, Empty>(WireRequest::MarkerRemove)?,
        ),
        (
            "project.export".into(),
            pair::<ProjectExport, ProjectExported>(WireRequest::ProjectExport)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn transition_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "scene.switch".into(),
            pair::<SwitchScene, Empty>(WireRequest::SceneSwitch)?,
        ),
        (
            "canvas.transition".into(),
            pair::<TransitionOnce, Empty>(WireRequest::CanvasTransition)?,
        ),
        (
            "canvas.transition_progress".into(),
            pair::<TransitionProgress, Empty>(WireRequest::CanvasTransitionProgress)?,
        ),
        (
            "item.set_crop".into(),
            pair::<SetItemCrop, Empty>(WireRequest::ItemSetCrop)?,
        ),
        (
            "item.set_box".into(),
            pair::<SetItemBox, BoxPlaced>(WireRequest::ItemSetBox)?,
        ),
        (
            "sync.forget_device".into(),
            pair::<SyncForgetDevice, Empty>(WireRequest::SyncForgetDevice)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn glide_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "collection.set_glide".into(),
            pair::<SetCollectionGlide, Empty>(WireRequest::CollectionSetGlide)?,
        ),
        (
            "source.set_glide".into(),
            pair::<SetSourceGlide, Empty>(WireRequest::SourceSetGlide)?,
        ),
        (
            "scene.set_transition".into(),
            pair::<SetSceneTransition, Empty>(WireRequest::SceneSetTransition)?,
        ),
        (
            "collection.set_transition".into(),
            pair::<SetCollectionTransition, TransitionSet>(WireRequest::CollectionSetTransition)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn session_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    Ok(vec![
        (
            "session.hello".into(),
            pair::<Hello, HelloResult>(WireRequest::Hello)?,
        ),
        (
            "session.authenticate".into(),
            pair::<Authenticate, AuthResult>(WireRequest::Authenticate)?,
        ),
        (
            "events.subscribe".into(),
            pair::<Subscribe, Subscribed>(WireRequest::EventsSubscribe)?,
        ),
        (
            "events.unsubscribe".into(),
            pair::<Unsubscribe, Empty>(WireRequest::EventsUnsubscribe)?,
        ),
        (
            "tokens.list".into(),
            pair::<Empty, TokenList>(WireRequest::TokensList)?,
        ),
        (
            "tokens.create".into(),
            pair::<CreateToken, TokenCreated>(WireRequest::TokensCreate)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn list_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    use crate::types::{AddOnList, Library, TextFonts};
    Ok(vec![
        (
            "profile.list".into(),
            pair::<Empty, Library>(WireRequest::ProfileList)?,
        ),
        (
            "collection.list".into(),
            pair::<Empty, Library>(WireRequest::CollectionList)?,
        ),
        (
            "text.fonts".into(),
            pair::<Empty, TextFonts>(WireRequest::TextFonts)?,
        ),
        (
            "addon.list".into(),
            pair::<Empty, AddOnList>(WireRequest::AddonList)?,
        ),
        (
            "stream.destination.set_enabled".into(),
            pair::<SetDestinationEnabled, Empty>(WireRequest::StreamDestinationSetEnabled)?,
        ),
        (
            "output.set_with_vertical".into(),
            pair::<SetWithVertical, Empty>(WireRequest::OutputSetWithVertical)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn attribute_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    use crate::types::{
        AttributesApplied, AttributesClipboard, SceneDuplicated, SourceDuplicated, TextTimerState,
    };
    Ok(vec![
        (
            "scene.duplicate".into(),
            pair::<DuplicateScene, SceneDuplicated>(WireRequest::SceneDuplicate)?,
        ),
        (
            "source.duplicate".into(),
            pair::<SourceInScene, SourceDuplicated>(WireRequest::SourceDuplicate)?,
        ),
        (
            "source.reset".into(),
            pair::<ResetSource, AttributesApplied>(WireRequest::SourceReset)?,
        ),
        (
            "source.paste_attributes".into(),
            pair::<PasteAttributes, AttributesApplied>(WireRequest::SourcePasteAttributes)?,
        ),
        (
            "source.copy_attributes".into(),
            pair::<SourceInScene, AttributesClipboard>(WireRequest::SourceCopyAttributes)?,
        ),
        (
            "source.attributes_clipboard".into(),
            pair::<Empty, Option<AttributesClipboard>>(WireRequest::SourceAttributesClipboard)?,
        ),
        (
            "text.timer".into(),
            pair::<TextTimer, TextTimerState>(WireRequest::TextTimer)?,
        ),
    ])
}

#[cfg(feature = "schema")]
fn status_entries() -> Result<Vec<(String, MethodSchemas)>, serde_json::Error> {
    use crate::types::{
        LoudnessStatus, MonitorDeviceList, MonitorStatus, ReplayStatus, Snapshot, VcamStatus,
    };
    Ok(vec![
        (
            "stream.destinations".into(),
            pair::<DestinationsOf, crate::types::StreamDestinations>(
                WireRequest::StreamDestinations,
            )?,
        ),
        (
            "stream.destination.stop".into(),
            pair::<DestinationRef, Empty>(WireRequest::StreamDestinationStop)?,
        ),
        (
            "source.list".into(),
            pair::<Empty, crate::types::Snapshot<Vec<crate::types::SourceInfo>>>(
                WireRequest::SourceList,
            )?,
        ),
        (
            "scene.list".into(),
            pair::<Empty, crate::types::Snapshot<crate::types::SceneList>>(WireRequest::SceneList)?,
        ),
        (
            "scene.sources".into(),
            pair::<SceneRef, SceneSourceList>(WireRequest::SceneSources)?,
        ),
        (
            "global.list".into(),
            pair::<Empty, GlobalSourceList>(WireRequest::GlobalList)?,
        ),
        (
            "source.targets".into(),
            pair::<TargetsParams, TargetList>(WireRequest::SourceTargets)?,
        ),
        (
            "settings.recent_targets".into(),
            pair::<TargetsParams, TargetList>(WireRequest::SettingsRecentTargets)?,
        ),
        (
            "item.set_style".into(),
            pair::<SetItemStyle, Empty>(WireRequest::ItemSetStyle)?,
        ),
        (
            "filter.types".into(),
            pair::<FilterTypesParams, crate::types::FilterTypeList>(WireRequest::FilterTypes)?,
        ),
        (
            "import.obs.scan".into(),
            pair::<Empty, crate::types::ObsScan>(WireRequest::ImportObsScan)?,
        ),
        (
            "import.obs.check".into(),
            pair::<ObsImport, crate::types::ObsCheck>(WireRequest::ImportObsCheck)?,
        ),
        (
            "import.obs.run".into(),
            pair::<ObsImport, crate::types::ObsRun>(WireRequest::ImportObsRun)?,
        ),
        (
            "global.add".into(),
            pair::<AddGlobal, ItemAdded>(WireRequest::GlobalAdd)?,
        ),
        (
            "canvas.auto_layout".into(),
            pair::<CanvasAutoLayout, AutoLayoutDone>(WireRequest::CanvasAutoLayout)?,
        ),
        (
            "settings.clear_recent_targets".into(),
            pair::<TargetsParams, Empty>(WireRequest::SettingsClearRecentTargets)?,
        ),
        (
            "sync.devices".into(),
            pair::<Empty, SyncDeviceList>(WireRequest::SyncDevices)?,
        ),
        (
            "browser.input".into(),
            pair::<BrowserInput, Empty>(WireRequest::BrowserInput)?,
        ),
        (
            "monitor.status".into(),
            pair::<Empty, Snapshot<MonitorStatus>>(WireRequest::MonitorStatus)?,
        ),
        (
            "monitor.devices".into(),
            pair::<Empty, MonitorDeviceList>(WireRequest::MonitorDevices)?,
        ),
        (
            "loudness.status".into(),
            pair::<Empty, LoudnessStatus>(WireRequest::LoudnessStatus)?,
        ),
        (
            "replay.status".into(),
            pair::<Empty, Snapshot<ReplayStatus>>(WireRequest::ReplayStatus)?,
        ),
        (
            "vcam.status".into(),
            pair::<Empty, Snapshot<VcamStatus>>(WireRequest::VcamStatus)?,
        ),
    ])
}
