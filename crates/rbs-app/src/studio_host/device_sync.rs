//! The sync offset remembered with each audio device (ADR-0126 decisions 7 to 9):
//! the value a source gets when it is added or pointed at a device, the entry a
//! committed change updates, and applying one source's value to the other sources of
//! its device. The file is `device-sync.toml` ([`crate::device_sync_file`]).

use rbs_scene::model::{SettingValue, Source, SourceId};
use rbs_sources::types::keys;

use super::{Host, Refusal};
use crate::device_sync_file::{DeviceEntry, DeviceKey, Origin, Reference, remembers};
use crate::edit_history::EditKind;

/// The device a source's offset is remembered with; `None` for a type that remembers
/// nothing (ADR-0126 decision 7) or a source without `adapter` and `target`.
pub(super) fn device_key(source: &Source) -> Option<DeviceKey> {
    let type_id = source.type_id.as_str();
    if !remembers(type_id) {
        return None;
    }
    key_of(source)
}

/// The `type`, `adapter` and `target` of any source that has them.
fn key_of(source: &Source) -> Option<DeviceKey> {
    let text = |key: &str| match source.settings.get(key) {
        Some(SettingValue::Text(t)) => Some(t.clone()),
        _ => None,
    };
    Some(DeviceKey {
        type_id: source.type_id.as_str().to_owned(),
        adapter: text(keys::ADAPTER)?,
        target: text(keys::TARGET)?,
    })
}

/// A measurement result that was offered for a source: applying exactly this value
/// records the entry as measured, with its reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Offered {
    pub source: SourceId,
    pub offset_ms: i32,
    pub reference: Reference,
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Host {
    /// Writes the remembered offset into source `id` after it was added (`old` is
    /// `None`) or pointed from device `old` at its present device, as part of the
    /// newest undo step, and sends it to the running input (ADR-0126 decision 8). The
    /// value it writes is not a committed change: the file is not touched.
    pub(super) fn fill_device_offset(
        &mut self,
        id: SourceId,
        old: Option<&DeviceKey>,
        name: &str,
        kind: EditKind,
    ) {
        let Some(source) = self.collection.source(id) else {
            return;
        };
        let new = device_key(source);
        if old.is_none() && new.is_none() {
            return;
        }
        let current = source.audio.sync_offset_ms;
        let want = self
            .device_sync
            .retarget_offset_ms(old, new.as_ref(), current);
        if want == current {
            return;
        }
        // The value comes from the file or is 0, both in range; a refusal leaves the
        // source as it was, which is the safe side.
        if let Ok(undo) = self.collection.set_source_sync_offset(id, want) {
            self.history.extend_last(kind, name, vec![undo]);
            self.studio
                .set_audio_offset(id, i64::from(want) * 1_000_000);
        }
    }

    /// Updates the entry of `source`'s device after a committed change of its offset
    /// to `offset_ms` (ADR-0126 decision 9): 0 removes it, a value that was offered
    /// by a measurement is recorded as measured. Saves the file when it changed.
    pub(super) fn remember_device_offset(&mut self, source: SourceId, offset_ms: i32) {
        let offered = self.offered.take();
        let Some(key) = self.collection.source(source).and_then(device_key) else {
            return;
        };
        let changed = if offset_ms == 0 {
            self.device_sync.forget(&key)
        } else {
            let (origin, reference) = match offered {
                Some(o) if o.source == source && o.offset_ms == offset_ms => {
                    (Origin::Measured, Some(o.reference))
                }
                _ => (Origin::Manual, None),
            };
            self.device_sync.set(DeviceEntry {
                key,
                offset_ms,
                origin,
                reference,
                set_at: now_s(),
            })
        };
        if changed && let Some(dir) = &self.config_dir {
            let _ = crate::device_sync_file::save(dir, &self.device_sync);
        }
    }

    /// Records that a measurement offered `offset_ms` for `source` against
    /// `reference`, so applying it is remembered as measured.
    pub(super) fn offer_measured(&mut self, source: SourceId, offset_ms: i32, reference: SourceId) {
        let Some(r) = self.collection.source(reference) else {
            return;
        };
        self.offered = Some(Offered {
            source,
            offset_ms,
            reference: Reference {
                key: key_of(r),
                name: r.name.clone(),
            },
        });
    }

    /// The other sources of the collection on `source`'s device whose offset is not
    /// `source`'s, by name: what "Apply to N other sources" changes.
    pub(super) fn device_peers(&self, source: SourceId) -> Vec<(SourceId, String)> {
        let Some(s) = self.collection.source(source) else {
            return Vec::new();
        };
        let Some(key) = device_key(s) else {
            return Vec::new();
        };
        let offset = s.audio.sync_offset_ms;
        self.collection
            .sources()
            .filter(|o| o.id != source && o.audio.sync_offset_ms != offset)
            .filter(|o| device_key(o).as_ref() == Some(&key))
            .map(|o| (o.id, o.name.clone()))
            .collect()
    }

    /// Gives every other source of `source`'s device `source`'s offset (ADR-0126
    /// decision 9, `sync.apply_to_device_sources`): a live control like the offset
    /// itself, not an undo step. Returns how many changed.
    pub(super) fn apply_to_device_sources(&mut self, source: SourceId) -> Result<usize, Refusal> {
        let s = self
            .collection
            .source(source)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", source.get())))?;
        if device_key(s).is_none() {
            return Err(Refusal::Invalid(format!(
                "{} is not an audio device source.",
                s.name
            )));
        }
        let offset = s.audio.sync_offset_ms;
        let peers = self.device_peers(source);
        for (id, _) in &peers {
            self.collection
                .set_source_sync_offset(*id, offset)
                .map_err(|e| Refusal::of(&e))?;
            self.studio
                .set_audio_offset(*id, i64::from(offset) * 1_000_000);
        }
        Ok(peers.len())
    }

    /// Removes the entry of `key`; `Ok(false)` when there was none. Sources keep
    /// their values.
    pub(super) fn forget_device(&mut self, key: &DeviceKey) -> bool {
        let gone = self.device_sync.forget(key);
        if gone && let Some(dir) = &self.config_dir {
            let _ = crate::device_sync_file::save(dir, &self.device_sync);
        }
        gone
    }

    /// Sends the collection's offsets to the studio for every source whose offset
    /// differs from `before` (after an undo or redo, which can restore an offset
    /// that an add or retarget filled in).
    pub(super) fn resync_audio_offsets(
        &mut self,
        before: &std::collections::BTreeMap<SourceId, i64>,
    ) {
        let after = super::filters::audio_offsets(&self.collection);
        for (id, ns) in &after {
            if before.get(id) != Some(ns) {
                self.studio.set_audio_offset(*id, *ns);
            }
        }
        for id in before.keys().filter(|id| !after.contains_key(id)) {
            self.studio.set_audio_offset(*id, 0);
        }
    }
}
