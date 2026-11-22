//! Buses and sends of a canvas in the document (format 29, ADR-0097 decision 9,
//! ADR-0170).
//!
//! ```toml
//! [[canvases.buses]]
//! id = 1
//! name = "VOD safe"
//! master = { volume = 0.5 }          # optional, like the canvas master
//! [[canvases.buses.filters]]          # optional, like source filters
//! [[canvases.sends]]                  # only levels that differ from the default
//! source = 3
//! bus = 1                             # 0 is `main`
//! level = 0.8
//! ```

use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{Bus, BusId, Canvas, Filter, FilterId, MasterAudio, SourceId};
use serde::{Deserialize, Serialize};

use super::{FilterDoc, MasterDoc, later, settings, settings_doc, wide};
use crate::document::BodyError;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BusDoc {
    id: u32,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    master: Option<MasterDoc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    filters: Vec<FilterDoc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SendDoc {
    source: u32,
    bus: u32,
    level: f64,
}

/// The canvas's buses for the document.
pub(super) fn bus_docs(c: &Canvas) -> Result<Vec<BusDoc>, BodyError> {
    c.buses
        .iter()
        .map(|b| {
            Ok(BusDoc {
                id: b.id.get(),
                name: b.name.clone(),
                master: (!b.level.is_default()).then(|| MasterDoc {
                    volume: wide(b.level.volume),
                    muted: b.level.muted,
                }),
                filters: b
                    .filters
                    .iter()
                    .map(|f| {
                        Ok(FilterDoc {
                            id: f.id.get(),
                            type_id: f.type_id.as_str().to_owned(),
                            enabled: f.enabled,
                            settings: settings_doc(&f.settings)?,
                        })
                    })
                    .collect::<Result<_, BodyError>>()?,
            })
        })
        .collect()
}

/// The canvas's stored send levels for the document, by source then bus.
pub(super) fn send_docs(c: &Canvas) -> Vec<SendDoc> {
    c.sends
        .iter()
        .flat_map(|(source, sends)| {
            sends.stored().map(move |(bus, level)| SendDoc {
                source: source.get(),
                bus: bus.get(),
                level: wide(level),
            })
        })
        .collect()
}

/// Reads a canvas's buses and sends; the collection validates them. A send listed
/// twice for the same source and bus is refused.
pub(super) fn read(
    canvas: &mut Canvas,
    buses: Vec<BusDoc>,
    sends: Vec<SendDoc>,
) -> Result<(), BodyError> {
    canvas.buses = buses
        .into_iter()
        .map(|b| {
            let mut bus = Bus::new(BusId::from_raw(b.id), &b.name);
            if let Some(m) = b.master {
                bus.level = MasterAudio::new(m.volume as f32, m.muted);
            }
            bus.filters = b
                .filters
                .into_iter()
                .map(|f| {
                    let mut filter = Filter::new(
                        FilterId::from_raw(f.id),
                        NodeTypeId::new(f.type_id),
                        settings(f.settings),
                    );
                    filter.enabled = f.enabled;
                    filter
                })
                .collect();
            bus
        })
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    for s in sends {
        if !seen.insert((s.source, s.bus)) {
            return Err(BodyError::Value(format!(
                "canvas {:?}: the send from source {} to bus {} is listed twice",
                canvas.name, s.source, s.bus
            )));
        }
        canvas
            .sends
            .entry(SourceId::from_raw(s.source))
            .or_default()
            .set(BusId::from_raw(s.bus), s.level as f32);
    }
    Ok(())
}

/// A collection that has, or ever had, a bus is written as format 29.
pub(super) fn needs_29(col: &SceneCollection) -> bool {
    col.id_counters().bus > 0
        || col
            .canvases()
            .any(|c| !c.buses.is_empty() || !c.sends.is_empty())
}

/// An older file that carries format 29 keys was not written by a build that knew them.
pub(super) fn refuse_before_29(from: u32, body: &toml::Table) -> Result<(), BodyError> {
    let canvas_keys = body
        .get("canvases")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .any(|c| c.contains_key("buses") || c.contains_key("sends"));
    let counter = body
        .get("next")
        .and_then(toml::Value::as_table)
        .is_some_and(|n| n.contains_key("bus"));
    if from < 29 && (canvas_keys || counter) {
        return Err(later(from, "canvas `buses`, `sends` or `next.bus`", 29));
    }
    Ok(())
}
