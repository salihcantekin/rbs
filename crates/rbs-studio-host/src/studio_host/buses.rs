//! The main canvas's audio buses through the control API (protocol 1.115, ADR-0097
//! decisions 12 and 13, ADR-0170 decision 9): adding, removing and renaming a bus as
//! undo steps, its live volume and mute, each source's sends, the checks that keep a
//! used bus, the loaded output profile's references that follow a rename, and
//! `bus.changed`. Control thread only: nothing here runs per frame.

use rbs_api::request::{BusList, BusRow, SendLevel};
use rbs_scene::model::{BusId, MAIN_BUS_NAME, MAX_BUSES, Sends};

use super::*;
use crate::edit_history::EditKind;

/// What was last published about the buses, and the profiles a rename changed that
/// the window has not been sent yet.
#[derive(Debug, Default)]
pub(super) struct BusEvents {
    /// The `bus.list` answer last published as `bus.changed`.
    sent: Option<BusList>,
    /// The recording, stream, vertical recording and vertical stream settings, in
    /// that order, changed by a rename and not yet sent to the window to save.
    due: [bool; 4],
}

/// A conflict whose `data.reason` says why (and `data.uses` for a used bus).
fn conflict(reason: &str, message: String, uses: Option<&[String]>) -> ApiError {
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), reason.into());
    if let Some(uses) = uses {
        data.insert("uses".into(), uses.into());
    }
    ApiError::with_data(ErrorKind::Conflict, message, data)
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidParams, message)
}

/// A bus name from a request: trimmed, not empty and not `main`.
fn new_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("a bus needs a name"));
    }
    if name == MAIN_BUS_NAME {
        return Err(invalid("main is the master; it cannot be added or renamed"));
    }
    Ok(name)
}

fn scene_refusal(e: &rbs_scene::SceneError) -> ApiError {
    refused(&Refusal::of(e))
}

impl Host {
    /// The `bus.list` answer: the main canvas's buses other than `main`, in order.
    pub(super) fn bus_list(&self) -> BusList {
        BusList {
            buses: self
                .collection
                .buses(self.canvas)
                .unwrap_or(&[])
                .iter()
                .map(|b| BusRow {
                    name: b.name.clone(),
                    volume: b.level.volume,
                    muted: b.level.muted,
                })
                .collect(),
        }
    }

    /// A source's stored send levels on the main canvas, by bus name.
    pub(super) fn row_sends(&self, source: SourceId) -> Vec<(String, f32)> {
        let sends = self.collection.sends(self.canvas, source);
        if sends.is_default() {
            return Vec::new();
        }
        sends
            .stored()
            .filter_map(|(bus, level)| {
                let name = self.collection.bus_name(self.canvas, bus)?;
                Some((name.to_owned(), level))
            })
            .collect()
    }

    /// Whether a row sent last has other sends than the collection now: a send change
    /// is a live level edit, which does not build the rows again by itself.
    pub(super) fn sends_moved(&self) -> bool {
        self.rows
            .iter()
            .chain(self.heard.iter())
            .chain(self.globals_shown.iter())
            .any(|row| {
                let now = self.collection.sends(self.canvas, row.id);
                let mut stored = now.stored().filter_map(|(bus, level)| {
                    Some((self.collection.bus_name(self.canvas, bus)?, level))
                });
                let same = row
                    .sends
                    .iter()
                    .all(|(name, level)| stored.next() == Some((name.as_str(), *level)));
                !same || stored.next().is_some()
            })
    }

    /// The loaded profile's recording settings of each canvas: the vertical canvas's
    /// only when it has its own.
    fn record_profiles(&self) -> [(Option<&RecordProfile>, &str); 2] {
        [
            (Some(&self.recording.profile), "recording track"),
            (
                self.recording.vertical_profile.as_ref(),
                "vertical recording track",
            ),
        ]
    }

    /// What keeps the bus called `name` from being removed (`data.uses`): the loaded
    /// profile's streams and recording tracks that name it, and running outputs that
    /// read it. Empty when nothing does.
    pub(super) fn bus_uses(&self, name: &str) -> Vec<String> {
        let mut uses = Vec::new();
        let streams = [
            (self.streaming.stream_profile.as_ref(), "stream"),
            (
                self.streaming.vertical_stream_profile.as_ref(),
                "vertical stream",
            ),
        ];
        for (profile, what) in streams {
            if profile.is_some_and(|p| p.audio_bus.trim() == name) {
                uses.push(what.to_owned());
            }
        }
        for (profile, what) in self.record_profiles() {
            for (i, track) in profile
                .iter()
                .flat_map(|p| p.audio_tracks.iter())
                .enumerate()
            {
                if track.trim() == name {
                    uses.push(format!("{what} {}", i + 1));
                }
            }
        }
        let reads = |id: &OutputId| self.studio.output_reads_bus(*id, name);
        if self
            .outs
            .iter()
            .any(|o| o.recording.as_ref().is_some_and(|(_, id)| reads(id)))
        {
            uses.push("running recording".to_owned());
        }
        if self.outs.iter().any(|o| {
            o.streaming.as_ref().is_some_and(|(_, _, id)| reads(id))
                || o.destinations
                    .iter()
                    .any(|d| d.output.as_ref().is_some_and(reads))
        }) {
            uses.push("running stream".to_owned());
        }
        uses
    }

    /// Whether the loaded profile names `name` anywhere (a stream's bus or a track).
    fn profile_names(&self, name: &str) -> bool {
        let streams = [
            self.streaming.stream_profile.as_ref(),
            self.streaming.vertical_stream_profile.as_ref(),
        ];
        streams
            .into_iter()
            .flatten()
            .any(|p| p.audio_bus.trim() == name)
            || self
                .record_profiles()
                .into_iter()
                .filter_map(|(p, _)| p)
                .any(|p| p.audio_tracks.iter().any(|t| t.trim() == name))
    }

    /// Rewrites the loaded profile's references to `old` as `new` (both canvases);
    /// the changed settings go to the window to save with the next publish.
    fn rewrite_bus_refs(&mut self, old: &str, new: &str) {
        let renamed = |n: &mut String| {
            let hit = n.trim() == old;
            if hit {
                new.clone_into(n);
            }
            hit
        };
        if let Some(p) = &mut self.streaming.stream_profile {
            self.bus_events.due[1] |= renamed(&mut p.audio_bus);
        }
        if let Some(p) = &mut self.streaming.vertical_stream_profile {
            self.bus_events.due[3] |= renamed(&mut p.audio_bus);
        }
        for t in &mut self.recording.profile.audio_tracks {
            self.bus_events.due[0] |= renamed(t);
        }
        if let Some(p) = &mut self.recording.vertical_profile {
            for t in &mut p.audio_tracks {
                self.bus_events.due[2] |= renamed(t);
            }
        }
    }

    /// `bus.add`: one undo step.
    pub(super) fn api_add_bus(&mut self, name: &str) -> Result<(), ApiError> {
        let name = new_name(name)?;
        let buses = self
            .collection
            .buses(self.canvas)
            .map_err(|e| scene_refusal(&e))?;
        if buses.iter().any(|b| b.name == name) {
            return Err(conflict(
                "name_taken",
                format!("there is a bus called {name:?}"),
                None,
            ));
        }
        if buses.len() + 1 >= MAX_BUSES {
            return Err(conflict(
                "too_many_buses",
                format!("the canvas has {MAX_BUSES} buses with main"),
                None,
            ));
        }
        let (_, undo) = self
            .collection
            .add_bus(self.canvas, name)
            .map_err(|e| scene_refusal(&e))?;
        self.edits
            .history
            .record(EditKind::AddBus, name, 0, vec![undo]);
        Ok(())
    }

    /// The bus called `name` (trimmed), not `main`.
    fn named_bus(&self, name: &str) -> Result<(BusId, String), ApiError> {
        let name = name.trim();
        if name == MAIN_BUS_NAME {
            return Err(invalid("main is the master: use canvas.set_master_volume"));
        }
        self.collection
            .bus_by_name(self.canvas, name)
            .map(|id| (id, name.to_owned()))
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("there is no bus {name:?}")))
    }

    /// `bus.remove`: one undo step, refused while the bus is in use.
    pub(super) fn api_remove_bus(&mut self, name: &str) -> Result<(), ApiError> {
        let (id, name) = self.named_bus(name)?;
        let uses = self.bus_uses(&name);
        if !uses.is_empty() {
            return Err(conflict(
                "bus_in_use",
                format!("the bus {name:?} is in use: {}", uses.join(", ")),
                Some(&uses),
            ));
        }
        let undo = self
            .collection
            .remove_bus(self.canvas, id)
            .map_err(|e| scene_refusal(&e))?;
        self.edits
            .history
            .record(EditKind::RemoveBus, &name, 0, vec![undo]);
        Ok(())
    }

    /// `bus.set`: `(renamed, level changed)`. All checks come first, so a refused
    /// request changes nothing.
    pub(super) fn api_set_bus(
        &mut self,
        p: &rbs_api::request::SetBus,
    ) -> Result<(bool, bool), ApiError> {
        if p.rename.is_none() && p.volume.is_none() && p.muted.is_none() {
            return Err(invalid("bus.set needs rename, volume or muted"));
        }
        let (id, name) = self.named_bus(&p.name)?;
        if let Some(v) = p.volume
            && !Sends::level_is_valid(v)
        {
            return Err(invalid("volume must be a number from 0 to 1"));
        }
        let rename = match &p.rename {
            Some(to) => {
                let to = new_name(to)?;
                if to == name {
                    None
                } else if self.collection.bus_by_name(self.canvas, to).is_some() {
                    return Err(conflict(
                        "name_taken",
                        format!("there is a bus called {to:?}"),
                        None,
                    ));
                } else if self.profile_names(to) {
                    return Err(conflict(
                        "name_in_profile",
                        format!("the output settings already name {to:?}; change them first"),
                        None,
                    ));
                } else {
                    Some(to.to_owned())
                }
            }
            None => None,
        };
        // The rename first: it is the part a scene error could still refuse, so a
        // refused request changes nothing.
        let renamed = match &rename {
            Some(to) => {
                let undo = self
                    .collection
                    .rename_bus(self.canvas, id, to)
                    .map_err(|e| scene_refusal(&e))?;
                self.edits
                    .history
                    .record(EditKind::RenameBus, &name, 0, vec![undo]);
                self.rewrite_bus_refs(&name, to);
                true
            }
            None => false,
        };
        let level = self
            .collection
            .buses(self.canvas)
            .ok()
            .and_then(|b| b.iter().find(|b| b.id == id))
            .map(|b| b.level)
            .unwrap_or_default();
        let mut live = false;
        if let Some(v) = p.volume.filter(|v| *v != level.volume) {
            self.collection
                .set_bus_volume(self.canvas, id, v)
                .map_err(|e| scene_refusal(&e))?;
            live = true;
        }
        if let Some(m) = p.muted.filter(|m| *m != level.muted) {
            self.collection
                .set_bus_muted(self.canvas, id, m)
                .map_err(|e| scene_refusal(&e))?;
            live = true;
        }
        if live {
            self.studio.set_mix(&self.collection);
        }
        Ok((renamed, live))
    }

    /// `source.set_sends`: live, all or none; `Ok(false)` when nothing changed.
    pub(super) fn api_set_sends(
        &mut self,
        source: SourceId,
        sends: &[SendLevel],
    ) -> Result<bool, ApiError> {
        if sends.is_empty() {
            return Err(invalid("sends is empty"));
        }
        if self.collection.source(source).is_none() {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("there is no source {}", source.get()),
            ));
        }
        let mut resolved: Vec<(BusId, f32)> = Vec::with_capacity(sends.len());
        for s in sends {
            if !Sends::level_is_valid(s.level) {
                return Err(invalid("a send level must be a number from 0 to 1"));
            }
            let name = s.bus.trim();
            let bus = self
                .collection
                .bus_by_name(self.canvas, name)
                .ok_or_else(|| {
                    ApiError::new(ErrorKind::NotFound, format!("there is no bus {name:?}"))
                })?;
            if resolved.iter().any(|(b, _)| *b == bus) {
                return Err(invalid(format!("the bus {name:?} is named twice")));
            }
            resolved.push((bus, s.level));
        }
        let now = self.collection.sends(self.canvas, source);
        let mut changed = false;
        for (bus, level) in resolved {
            if now.level(bus) != level {
                self.collection
                    .set_send(self.canvas, source, bus, level)
                    .map_err(|e| scene_refusal(&e))?;
                changed = true;
            }
        }
        if changed {
            self.studio.set_mix(&self.collection);
        }
        Ok(changed)
    }

    /// Before an undo or redo commits `next`: refuses one that deletes a bus in use
    /// or whose rename would repeat a recording track, and gives the renames whose
    /// references to rewrite, as `(old, new)`.
    pub(super) fn bus_step_check(
        &self,
        next: &SceneCollection,
    ) -> Result<Vec<(String, String)>, String> {
        let now = self.collection.buses(self.canvas).unwrap_or(&[]);
        let then = next.buses(self.canvas).unwrap_or(&[]);
        // A name now taken cannot come back: every bus name change is a step of
        // this linear history, and the collection's validation refuses a repeated
        // name anyway (`HistoryError::Refused`).
        let mut renames = Vec::new();
        for b in now {
            match then.iter().find(|t| t.id == b.id) {
                None => {
                    let uses = self.bus_uses(&b.name);
                    if !uses.is_empty() {
                        return Err(format!(
                            "The bus {:?} is in use ({}), so this step cannot remove it.",
                            b.name,
                            uses.join(", ")
                        ));
                    }
                }
                Some(t) if t.name != b.name => renames.push((b.name.clone(), t.name.clone())),
                Some(_) => {}
            }
        }
        for (old, new) in &renames {
            let repeats = self
                .record_profiles()
                .into_iter()
                .filter_map(|(p, _)| p)
                .any(|p| {
                    p.audio_tracks.iter().any(|t| t.trim() == old)
                        && p.audio_tracks.iter().any(|t| t.trim() == new)
                });
            if repeats {
                return Err(format!(
                    "The recording settings name both {old:?} and {new:?}, so this step cannot rename the bus."
                ));
            }
        }
        Ok(renames)
    }

    /// After an undo or redo committed: the references follow its renames.
    pub(super) fn bus_step_done(&mut self, renames: &[(String, String)]) {
        for (old, new) in renames {
            self.rewrite_bus_refs(old, new);
        }
    }

    /// Publishes `bus.changed` when the buses changed, and sends the window the
    /// settings a rename changed, to save.
    pub(super) fn publish_buses(&mut self, notify: &dyn Fn(Update)) {
        // Compared in place, so an unchanged round allocates nothing (F-10).
        let buses = self.collection.buses(self.canvas).unwrap_or(&[]);
        let same = self.bus_events.sent.as_ref().is_some_and(|sent| {
            sent.buses.len() == buses.len()
                && sent.buses.iter().zip(buses).all(|(r, b)| {
                    r.name == b.name && r.volume == b.level.volume && r.muted == b.level.muted
                })
        });
        if !same {
            let now = self.bus_list();
            self.bus_events.sent = Some(now.clone());
            self.events.changed(StateKey::topic(Topic::BusChanged), || {
                EventPayload::BusChanged(now)
            });
        }
        let due = std::mem::take(&mut self.bus_events.due);
        if due == [false; 4] {
            return;
        }
        if due[0] {
            notify(Update::Profile(self.recording.profile.clone()));
        }
        if due[1]
            && let Some(p) = &self.streaming.stream_profile
        {
            notify(Update::StreamProfile(p.clone()));
        }
        if due[2] {
            notify(Update::VerticalProfile(
                self.recording.vertical_profile.clone(),
            ));
        }
        if due[3]
            && let Some(p) = &self.streaming.vertical_stream_profile
        {
            notify(Update::VerticalStreamProfile(p.clone()));
        }
        self.settings_changed();
        self.availability_changed();
    }
}
