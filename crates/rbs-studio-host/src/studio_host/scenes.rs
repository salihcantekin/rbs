//! Scenes: adding, removing and switching them, studio mode, transitions, scene levels and the scene list (ADR-0159 decision 8).

use super::*;

impl Host {
    /// Hands the collection to autosave when it changed since it was last handed, so
    /// a round with no edit neither walks nor copies the collection (audit F-09).
    /// Every edit bumps [`SceneCollection::generation`] and every replacement bumps
    /// the epoch, so an equal stamp means the same value. An edit and its undo give a
    /// new stamp and one more save, which the writer finds unchanged and does not write.
    pub(super) fn publish_save(&mut self) {
        let stamp = self.save_stamp();
        if let Some(save) = &self.autosave
            && self.saved != Some(stamp)
        {
            save.save(&self.collection);
            self.saved = Some(stamp);
        }
    }

    /// Which value of the collection runs: the epoch and the generation.
    pub(super) fn save_stamp(&self) -> (u64, u64) {
        (self.collection_epoch, self.collection.generation())
    }

    /// Runs `next` instead of the collection that runs now (the studio restarts);
    /// on a failure the one before runs again. Always sends [`Update::Canvas`].
    pub(super) fn switch_collection(
        &mut self,
        next: SceneCollection,
        autosave: Option<CollectionSaver>,
        notify: &dyn Fn(Update),
    ) -> Result<(), String> {
        if self.any_recording() {
            let why = "Stop the recording before switching scene collections.".to_owned();
            self.canvas_changed(self.api_canvas(), notify);
            return Err(why);
        }
        if self.any_streaming() {
            let why = "End the stream before switching scene collections.".to_owned();
            self.canvas_changed(self.api_canvas(), notify);
            return Err(why);
        }
        // The pages of the collection before go: their Interact windows close.
        self.close_all_interacts(notify);
        let before = self.api_canvas();
        // Fixing up a missing canvas or scene below is an edit: it is saved.
        let loaded = next.generation();
        let mut next = next;
        let (size, fps) = self
            .collection
            .canvas(self.canvas)
            .map_or((Size::new(1920, 1080), FrameRate::FPS_30), |c| {
                (c.size, c.frame_rate)
            });
        let (canvas, scene) = match shown_scene(&mut next, size, fps) {
            Ok(shown) => shown,
            Err(why) => {
                self.canvas_changed(before, notify);
                return Err(format!("The scene collection cannot be used: {why}"));
            }
        };
        // Whatever the old collection's last change was, it is saved before this one.
        self.publish_save();
        self.save_text_timers();
        self.note_collection();
        let old = (
            std::mem::replace(&mut self.collection, next),
            std::mem::replace(&mut self.canvas, canvas),
            std::mem::replace(&mut self.scene, scene),
        );
        // A copy of the old collection's program never goes on air in the new one.
        let old_held = self.held.take();
        let _ = self.studio.shutdown();
        let result = match self.restart() {
            Ok(()) => {
                self.autosave = autosave;
                // The new collection's timers, not the old one's.
                self.studio.set_text_timers(&[]);
                self.text_timers_restored = false;
                self.saved = Some((self.collection_epoch, loaded));
                self.history.clear();
                // The attribute clipboard belongs to the old collection (ADR-0133).
                if self.attribute_clipboard.take().is_some() {
                    self.clipboard_changed(notify);
                }
                Ok(())
            }
            Err(why) => {
                self.note_collection();
                (self.collection, self.canvas, self.scene) = old;
                self.held = old_held;
                if let Err(again) = self.restart() {
                    self.dead = Some(again);
                }
                Err(format!("The scene collection cannot run: {why}"))
            }
        };
        self.canvas_changed(before, notify);
        result
    }

    /// One place or to an end in the canvas's scene list. `Ok(false)` at that end.
    pub(super) fn move_scene(
        &mut self,
        id: SceneId,
        direction: rbs_api::request::LayerDirection,
    ) -> Result<bool, String> {
        use rbs_api::request::LayerDirection::*;
        let undo = match direction {
            Up => self.collection.move_scene(self.canvas, id, true),
            Down => self.collection.move_scene(self.canvas, id, false),
            Front => self.collection.move_scene_to_end(self.canvas, id, true),
            Back => self.collection.move_scene_to_end(self.canvas, id, false),
        }
        .map_err(|e| edit_error(&e))?;
        let Some(undo) = undo else {
            return Ok(false);
        };
        let name = self
            .collection
            .scene(id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.history.record(EditKind::Order, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Puts scene `id` where scene `to` is in the scene list.
    pub(super) fn move_scene_to(&mut self, id: SceneId, to: SceneId) -> Result<bool, String> {
        let undo = self
            .collection
            .move_scene_to(self.canvas, id, to)
            .map_err(|e| edit_error(&e))?;
        let Some(undo) = undo else {
            return Ok(false);
        };
        let name = self
            .collection
            .scene(id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.history.record(EditKind::Order, &name, 0, vec![undo]);
        Ok(true)
    }

    pub(super) fn set_glide(&mut self, duration_ms: u32, easing: u8) -> Result<bool, Refusal> {
        let glide = rbs_scene::CollectionGlide {
            duration_ms,
            easing: match easing {
                0 => rbs_scene::GlideEasing::Linear,
                _ => rbs_scene::GlideEasing::EaseInOut,
            },
        };
        if self.collection.glide() == glide {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_glide(glide)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Glide, "", 0, vec![undo]);
        Ok(true)
    }

    /// `collection.set_transition` (ADR-0128 decision 13): merges the given parts into
    /// the collection's transition as one undo step.
    pub(super) fn set_transition(
        &mut self,
        p: &rbs_api::request::SetCollectionTransition,
    ) -> Result<bool, Refusal> {
        let mut t = self.collection.transition();
        if let Some(kind) = p.kind {
            t.kind = scene_transition_kind(kind);
        }
        if let Some(kind) = p.direct_kind {
            t.direct = scene_transition_kind(kind);
        }
        if let Some(f) = &p.fade {
            t.fade = rbs_scene::FadeSettings {
                duration_ms: f.duration_ms,
                easing: scene_easing(f.easing),
                via: match &f.via {
                    None => None,
                    Some(c) => Some(parse_rgb(c).ok_or_else(|| {
                        Refusal::Invalid(format!("{c} is not a colour like #1a2b3c."))
                    })?),
                },
            };
        }
        if let Some(s) = p.swipe {
            t.swipe = scene_swipe(s);
        }
        if let Some(s) = p.slide {
            t.slide = scene_swipe(s);
        }
        if let Some(st) = p.stinger {
            t.stinger = self.stinger_settings(st)?;
        }
        self.put_transition(t)
    }

    /// The stinger settings `collection.set_transition` asks for, checked (ADR-0140
    /// decision 9): the source exists and is a media file, and the cut point is within
    /// the file's length when that is known. Placement is the model's check.
    pub(super) fn stinger_settings(
        &self,
        st: api::StingerTransition,
    ) -> Result<rbs_scene::StingerSettings, Refusal> {
        let source = match st.source {
            None => None,
            Some(raw) => {
                let id = SourceId::from_raw(raw);
                let s = self
                    .collection
                    .source(id)
                    .ok_or_else(|| Refusal::NotFound(format!("There is no source {raw}.")))?;
                if s.type_id.as_str() != types::MEDIA_SOURCE {
                    return Err(Refusal::Invalid(format!(
                        "Source {raw} is not a media file, so it cannot be the stinger."
                    )));
                }
                Some(id)
            }
        };
        if let (Some(cut), Some(id)) = (st.cut_ms, source) {
            let length = (self.collection.transition().stinger.source == Some(id))
                .then(|| self.stinger.duration())
                .flatten();
            if length.is_some_and(|l| u128::from(cut) > l.as_millis()) {
                return Err(Refusal::Invalid(format!(
                    "The cut point {cut} ms is past the end of the stinger's file."
                )));
            }
        }
        Ok(rbs_scene::StingerSettings {
            source,
            cut_ms: st.cut_ms,
        })
    }

    /// `scene.set_transition` (ADR-0135 decision 10) and the Scenes panel's
    /// "Transition into this scene": sets or clears a scene's override as one undo
    /// step; `Ok(false)` when it is already set.
    pub(super) fn set_scene_transition(
        &mut self,
        id: SceneId,
        t: Option<rbs_scene::SceneTransition>,
    ) -> Result<bool, Refusal> {
        let scene = self
            .collection
            .scene(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?;
        let name = scene.name.clone();
        let t = t.filter(|t| !t.is_empty());
        if scene.transition_in == t {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_scene_transition(id, t)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::Transition, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Sets the collection's transition as one undo step; `Ok(false)` when it is
    /// already set.
    pub(super) fn put_transition(
        &mut self,
        t: rbs_scene::CollectionTransition,
    ) -> Result<bool, Refusal> {
        if self.collection.transition() == t {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_transition(t)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Transition, "", 0, vec![undo]);
        Ok(true)
    }

    /// Places a source the collection has on top of the program scene (one step).
    pub(super) fn add_existing(&mut self, id: SourceId) -> Result<ItemId, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let name = source.name.clone();
        let kind = SourceKind::of(&source.type_id);
        if self.global_item(id).is_some() {
            return Err(Refusal::Conflict(format!(
                "{name} is a global source: it is already in every scene."
            )));
        }
        // The same box as the source has elsewhere, else the whole canvas.
        let layout = self
            .collection
            .placements_of(id)
            .first()
            .and_then(|(_, item)| self.collection.item(*item))
            .map(|(_, i)| self.shown_layout(i))
            .unwrap_or_else(|| {
                let c = self.collection.canvas(self.canvas);
                let (w, h) = c.map_or((1.0, 1.0), |c| (c.size.width as f32, c.size.height as f32));
                if kind.is_some_and(SourceKind::is_audio) {
                    Layout::new([0.0, 0.0], [1.0, 1.0])
                } else {
                    Layout::new([0.0, 0.0], [w, h])
                }
            });
        let (item, undo) = self
            .collection
            .add_item(
                self.edited(),
                ItemContent::Source(id),
                self.stored_layout(&layout),
                None,
            )
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Add, &name, 0, vec![undo]);
        Ok(item)
    }

    /// The scenes the canvas may show, in panel order.
    pub(super) fn panel_scenes(&self) -> Vec<SceneId> {
        self.collection
            .showable_scenes(self.canvas)
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    /// Adds an empty shared scene (one undo step); it does not go on air.
    pub(super) fn add_scene(&mut self, name: Option<&str>) -> Result<SceneId, Refusal> {
        let name = match name.map(str::trim) {
            Some(n) if !n.is_empty() => n.to_owned(),
            Some(_) => return Err(Refusal::Invalid("A scene needs a name.".to_owned())),
            None => unique_scene_name(&self.collection, "Scene"),
        };
        let (id, undo) = self
            .collection
            .add_scene(&name, SceneScope::Shared)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::AddScene, &name, 0, vec![undo]);
        Ok(id)
    }

    /// Copies a scene (one undo step); it does not go on air.
    pub(super) fn duplicate_scene(&mut self, id: SceneId) -> Result<SceneId, Refusal> {
        let base = self
            .collection
            .scene(id)
            .filter(|_| self.listed(id))
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?
            .name
            .clone();
        let name = unique_scene_name(&self.collection, &base);
        let (new_id, undo) = self
            .collection
            .duplicate_scene(id, &name)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::AddScene, &name, 0, vec![undo]);
        Ok(new_id)
    }

    /// Removes a scene (one undo step). The last scene the canvas may show stays; the
    /// scene on air is first switched away from, outside the undo stack (ADR-0059).
    pub(super) fn remove_scene(&mut self, id: SceneId) -> Result<(), Refusal> {
        let name = self
            .collection
            .scene(id)
            .filter(|_| self.listed(id))
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?
            .name
            .clone();
        let panel = self.panel_scenes();
        let Some(at) = panel.iter().position(|s| *s == id) else {
            return Err(Refusal::NotFound(format!(
                "The canvas cannot show the scene {name:?}."
            )));
        };
        if panel.len() < 2 {
            return Err(Refusal::Conflict(
                "The last scene cannot be removed.".to_owned(),
            ));
        }
        if self.scene == id {
            let next = if at > 0 { panel[at - 1] } else { panel[at + 1] };
            self.switch_scene(next)?;
        }
        // Studio mode keeps a preview scene (ADR-0061): the program takes its place.
        if self.preview() == Some(id) {
            self.collection
                .set_preview(self.canvas, Some(self.scene))
                .map_err(|e| Refusal::of(&e))?;
        }
        let undo = self
            .collection
            .remove_scene(id)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::RemoveScene, &name, 0, vec![undo]);
        Ok(())
    }

    /// Renames a scene (one undo step); `Ok(false)` when it has that name.
    pub(super) fn rename_scene(&mut self, id: SceneId, name: &str) -> Result<bool, Refusal> {
        let old = self
            .collection
            .scene(id)
            .filter(|_| self.listed(id))
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?
            .name
            .clone();
        if old == name.trim() {
            return Ok(false);
        }
        let undo = self
            .collection
            .rename_scene(id, name)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::RenameScene, &old, 0, vec![undo]);
        Ok(true)
    }

    /// Puts a scene on air: a live action, not on the undo stack (ADR-0021).
    /// `Ok(false)` when it is on air already.
    pub(super) fn switch_scene(&mut self, id: SceneId) -> Result<bool, Refusal> {
        self.studio.cancel_manual(self.canvas);
        if !self.listed(id) {
            return Err(Refusal::NotFound(format!(
                "There is no scene {}.",
                id.get()
            )));
        }
        // A newer switch replaces one still held back for its texts.
        self.held_switch = None;
        if self.scene == id {
            return Ok(false);
        }
        if self.studio.text_prepare(self.canvas, id) {
            // Goes on air from the control loop once its texts are uploaded.
            self.held_switch = Some(id);
            return Ok(false);
        }
        self.put_on_air(id)
    }

    pub(super) fn put_on_air(&mut self, id: SceneId) -> Result<bool, Refusal> {
        self.collection
            .set_program(self.canvas, Some(id))
            .map_err(|e| Refusal::of(&e))?;
        self.scene = id;
        rbs_core::info!(rbs_core::log::Area::Studio, "scene {} on air", id.get());
        Ok(true)
    }

    /// Makes a held-back program switch once its texts are ready or late.
    pub(super) fn drive_held_switch(&mut self, notify: &dyn Fn(Update)) {
        let Some(id) = self.held_switch else {
            return;
        };
        if self.studio.text_due() == TextDue::Wait {
            return;
        }
        self.held_switch = None;
        // The scene may have been removed meanwhile; the refusal is then news.
        let done = if self.listed(id) && self.scene != id {
            self.put_on_air(id)
        } else {
            Ok(false)
        };
        self.answer(done, notify);
    }

    /// A scene the Scenes panel lists, by name, for the scene levels methods; a global
    /// scene is never listed (ADR-0065).
    pub(super) fn levels_scene(&self, id: SceneId) -> Result<String, Refusal> {
        self.collection
            .scene(id)
            .filter(|_| self.listed(id))
            .map(|s| s.name.clone())
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))
    }

    /// Turns a scene's Own levels on (ADR-0091 decision 5), copying what every source
    /// it places is heard at now, so nothing jumps. One undo step. `Ok(false)` when it
    /// is in Own levels already.
    pub(super) fn scene_levels_own(&mut self, id: SceneId) -> Result<bool, Refusal> {
        let name = self.levels_scene(id)?;
        if self.collection.scene(id).is_some_and(Scene::own_levels) {
            return Ok(false);
        }
        let levels = self.collection.heard_levels(self.canvas, id);
        let undo = self
            .collection
            .set_scene_own_levels(id, levels)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SceneLevels, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Puts a scene back in Shared levels, discarding its entries (ADR-0091). One undo
    /// step. `Ok(false)` when it is in Shared levels already.
    pub(super) fn scene_levels_shared(&mut self, id: SceneId) -> Result<bool, Refusal> {
        let name = self.levels_scene(id)?;
        if !self.collection.scene(id).is_some_and(Scene::own_levels) {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_scene_shared_levels(id)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SceneLevels, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Removes a scene's entry for one source (ADR-0091): one undo step. `Ok(false)`
    /// when the scene has no entry for it (or is in Shared levels).
    pub(super) fn scene_level_reset(
        &mut self,
        id: SceneId,
        source: SourceId,
    ) -> Result<bool, Refusal> {
        let name = self.levels_scene(id)?;
        if self.collection.source(source).is_none() {
            return Err(Refusal::NotFound(format!(
                "There is no source {}.",
                source.get()
            )));
        }
        let has = self
            .collection
            .scene(id)
            .and_then(|s| s.audio.as_ref())
            .is_some_and(|a| a.level(source).is_some());
        if !has {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_scene_level(id, source, None)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SceneLevels, &name, 0, vec![undo]);
        if self.on_air(id) {
            self.studio.set_mix(&self.collection);
        }
        Ok(true)
    }

    /// A scene's own volume or mute for a source (ADR-0091): a live control, not on
    /// the undo stack, sent to the mixer at once while the scene is on air. A scene in
    /// Shared levels is turned to Own levels first, as one undo step. Answers whether
    /// that happened and whether anything changed.
    pub(super) fn scene_level_set(
        &mut self,
        id: SceneId,
        source: SourceId,
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Result<(bool, bool), Refusal> {
        self.levels_scene(id)?;
        if self.collection.source(source).is_none() {
            return Err(Refusal::NotFound(format!(
                "There is no source {}.",
                source.get()
            )));
        }
        if volume.is_none() && muted.is_none() {
            return Err(Refusal::Invalid(
                "Give a volume, a mute or both.".to_owned(),
            ));
        }
        let level = SceneLevel::new(volume, muted);
        if !level.is_valid() {
            return Err(Refusal::of(&SceneError::InvalidVolume));
        }
        let turned_own = self.scene_levels_own(id)?;
        let now = self
            .collection
            .scene(id)
            .and_then(|s| s.audio.as_ref())
            .and_then(|a| a.level(source).copied());
        let base = now.unwrap_or_default();
        let new = SceneLevel::new(volume.or(base.volume), muted.or(base.muted));
        let changed = now != Some(new);
        if changed {
            // Live: the record is not kept (ADR-0059, ADR-0091 decision 5).
            let _ = self
                .collection
                .set_scene_level(id, source, Some(new))
                .map_err(|e| Refusal::of(&e))?;
        }
        if (changed || turned_own) && self.on_air(id) {
            self.studio.set_mix(&self.collection);
        }
        Ok((turned_own, changed || turned_own))
    }

    /// Whether the window's fader and mute for `source` edit the program scene's level
    /// (ADR-0091 decision 5): that scene keeps its own levels and places the source
    /// (directly, nested, or as a global source of the canvas).
    pub(super) fn levels_take(&self, source: SourceId) -> bool {
        self.collection
            .scene(self.scene)
            .is_some_and(Scene::own_levels)
            && self
                .collection
                .placed_sources(self.canvas, self.scene)
                .contains(&source)
    }

    /// True while `id` is the studio canvas's program scene, whose levels are heard.
    pub(super) fn on_air(&self, id: SceneId) -> bool {
        self.scene == id
    }

    /// The preview scene while the canvas is in studio mode (ADR-0061).
    pub(super) fn preview(&self) -> Option<SceneId> {
        self.collection.canvas(self.canvas).and_then(|c| c.preview)
    }

    /// The scene source and item edits act on: the preview scene in studio mode, else
    /// the program scene (ADR-0061 decision 4).
    pub(super) fn edited(&self) -> SceneId {
        self.into.or(self.preview()).unwrap_or(self.scene)
    }

    /// Runs `f` with source and item additions going to `scene` (an API call's
    /// `scene`, ADR-0061 decision 8); `None` keeps them in the program scene, as in
    /// protocol 1.6.
    pub(super) fn placing_into<T>(
        &mut self,
        scene: Option<u32>,
        f: impl FnOnce(&mut Self) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let scene = match scene {
            None => self.scene,
            Some(raw) => {
                let id = SceneId::from_raw(raw);
                if !self.listed(id) {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no scene {raw}"),
                    ));
                }
                id
            }
        };
        let old = self.into.replace(scene);
        let done = f(self);
        self.into = old;
        done
    }

    /// Enters or leaves studio mode (ADR-0061): a live action. Entering puts the
    /// program scene in preview. `Ok(false)` when it is in that mode already.
    pub(super) fn set_studio_mode(&mut self, on: bool) -> Result<bool, Refusal> {
        if self.preview().is_some() == on {
            return Ok(false);
        }
        let scene = on.then_some(self.scene);
        self.collection
            .set_preview(self.canvas, scene)
            .map_err(|e| Refusal::of(&e))?;
        if !on {
            // Only once the mode has ended, so a refused leave changes nothing
            // (ADR-0151 decision 5).
            self.discard_preview_edits();
            // Leaving studio mode ends a running transition at once (ADR-0128
            // decision 9); the next update binds without it.
            self.studio.end_transitions();
        }
        Ok(true)
    }

    /// Puts the held program scene back as it is on air before studio mode ends
    /// (ADR-0151): edits made in Preview since the hold began never reach the program
    /// without a transition. One undo step, so Undo brings the edits back; nothing
    /// when nothing is held or nothing was edited.
    pub(super) fn discard_preview_edits(&mut self) {
        let Some(held) = self.held.take() else {
            return;
        };
        if held.from != self.scene {
            return;
        }
        let base = held.scene.id.get();
        let items: Option<Vec<SceneItem>> = held
            .scene
            .items
            .into_iter()
            .map(|mut item| {
                item.id = ItemId::from_raw(item.id.get().checked_sub(base)?);
                Some(item)
            })
            .collect();
        let Some(items) = items else {
            return;
        };
        let Some(scene) = self.collection.scene(held.from) else {
            return;
        };
        if scene.items == items {
            return;
        }
        let name = scene.name.clone();
        // A refusal (the copy no longer fits the collection, which the hold already
        // checks) leaves the scene as it is: the edits stay, as before ADR-0151.
        if let Ok(undo) = self.collection.set_scene_items(held.from, items) {
            self.history
                .record(EditKind::DiscardPreview, &name, 0, vec![undo]);
        }
    }

    /// Makes a scene the preview scene (studio mode only; a live action).
    /// `Ok(false)` when it is the preview scene already.
    pub(super) fn set_preview_scene(&mut self, id: SceneId) -> Result<bool, Refusal> {
        self.studio.cancel_manual(self.canvas);
        if !self.listed(id) {
            return Err(Refusal::NotFound(format!(
                "There is no scene {}.",
                id.get()
            )));
        }
        let Some(preview) = self.preview() else {
            return Err(Refusal::Conflict(STUDIO_MODE_OFF.to_owned()));
        };
        if preview == id {
            return Ok(false);
        }
        self.collection
            .set_preview(self.canvas, Some(id))
            .map_err(|e| Refusal::of(&e))?;
        Ok(true)
    }

    /// The transition (a cut, ADR-0061 decision 3): the preview scene goes on air and
    /// stays in preview, so the program is held from now on (ADR-0063); with the swap
    /// setting the old program scene goes to preview instead (ADR-0098). A live action. When both are the same scene,
    /// the edits made since the program was held go on air (ADR-0063); `Ok(false)` when
    /// there is nothing to put on air.
    pub(super) fn transition(&mut self) -> Result<bool, Refusal> {
        let Some(preview) = self.preview() else {
            return Err(Refusal::Conflict(STUDIO_MODE_OFF.to_owned()));
        };
        // A held T-bar is cancelled first (ADR-0135 decision 7).
        if !self.taking {
            self.studio.cancel_manual(self.canvas);
        }
        let program = self.scene;
        if preview == program {
            // `apply` holds the scene as it is now.
            let taken = self.held.take();
            if let Some(h) = &taken {
                self.held_was = Some(h.from);
                self.held_id_was = Some(h.scene.id);
            }
            return Ok(taken.is_some());
        }
        self.collection
            .set_program(self.canvas, Some(preview))
            .map_err(|e| Refusal::of(&e))?;
        self.scene = preview;
        rbs_core::info!(
            rbs_core::log::Area::Studio,
            "transition to scene {}",
            preview.get()
        );
        if self.swap_after_transition {
            let _ = self.collection.set_preview(self.canvas, Some(program));
        }
        Ok(true)
    }

    /// Applies a T-bar value (ADR-0135): at 1 the program change happens now, without
    /// a timed transition; when it changes nothing the manual one is cancelled.
    pub(super) fn drive_progress(&mut self, t: f32, notify: &dyn Fn(Update)) {
        if self.preview().is_none() {
            return;
        }
        self.tbar_zeroed = t <= 0.0;
        match self
            .studio
            .transition_progress(self.canvas, t, &self.adapters.sources)
        {
            Ok(rbs_record::ManualStep::Take) => {
                self.taking = true;
                let changed = self.transition();
                self.taking = false;
                match changed {
                    Ok(true) => self.apply(notify),
                    _ => {
                        self.studio.cancel_manual(self.canvas);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => notify(Update::Error(format!("The T-bar could not move: {e}"))),
        }
    }

    /// Holds the program scene while it is also the preview scene (ADR-0063), and lets
    /// it go otherwise. A copy is taken only when the hold starts, so later edits to
    /// the scene reach the program only through a transition.
    pub(super) fn sync_held(&mut self) {
        self.held = next_held(
            self.held.take(),
            &self.collection,
            self.canvas,
            self.held_id_was,
        );
    }

    /// The collection the studio runs: the user's own, or with the program held
    /// (ADR-0063) a run collection with the copy on air. A copy that no longer fits
    /// (a duplicate name, say) is taken again from the scene as it is now; if even a
    /// fresh copy does not fit, nothing is held.
    pub(super) fn run_collection(&mut self) -> Option<SceneCollection> {
        if let Some(h) = &self.held {
            self.held_was = Some(h.from);
            self.held_id_was = Some(h.scene.id);
        }
        self.sync_held();
        let held = self.held.as_ref()?;
        if let Some(run) = run_copy(&self.collection, self.canvas, held) {
            return Some(run);
        }
        // A fresh copy of the same scene, still on air; whatever is previewed, the hold
        // goes on (ADR-0098).
        // The same id as the copy it replaces: the hold goes on, no transition.
        let after = (held.scene.id.get() == HELD_IDS_NEXT).then(|| SceneId::from_raw(HELD_IDS));
        self.held = copy_of(&self.collection, held.from, after);
        let run = run_copy(&self.collection, self.canvas, self.held.as_ref()?);
        if run.is_none() {
            self.held = None;
        }
        run
    }

    /// Keeps a program scene on the canvas and [`Host::scene`] on it: after an undo or
    /// redo removed the scene on air, the first scene the canvas may show goes on air
    /// (a new one if there is none), outside the undo stack (ADR-0059).
    pub(super) fn keep_program(&mut self) {
        let program = self
            .collection
            .canvas(self.canvas)
            .and_then(|c| c.program)
            .filter(|s| self.collection.scene(*s).is_some());
        let scene = match program {
            Some(s) => s,
            None => {
                let first = self.panel_scenes().first().copied();
                let scene = match first {
                    Some(s) => s,
                    None => {
                        let name = unique_scene_name(&self.collection, "Scene");
                        match self.collection.add_scene(&name, SceneScope::Shared) {
                            Ok((s, _)) => s,
                            Err(_) => return,
                        }
                    }
                };
                if self
                    .collection
                    .set_program(self.canvas, Some(scene))
                    .is_err()
                {
                    return;
                }
                scene
            }
        };
        self.scene = scene;
    }

    /// The scenes and sources as the UI and `scene.list` show them.
    pub(super) fn scene_list(&self) -> SceneList {
        let mut sources: Vec<SourceEntry> = self
            .collection
            .sources()
            .map(|s| SourceEntry {
                id: s.id,
                name: s.name.clone(),
                kind: SourceKind::of(&s.type_id),
                global: self.global_item(s.id).is_some(),
                glide: s.glide,
                record_separately: s.record_separately,
            })
            .collect();
        sources.sort_by_key(|s| s.name.to_lowercase());
        let rows = |canvas: Option<CanvasId>| -> Vec<SceneRow> {
            canvas
                .map(|c| {
                    self.collection
                        .showable_scenes(c)
                        .into_iter()
                        .map(|s| SceneRow {
                            id: s.id,
                            name: s.name.clone(),
                            own_audio: s.own_levels(),
                            transition_in: s.transition_in,
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let scenes = rows(Some(self.canvas));
        let hold_scenes = [scenes.clone(), rows(self.vertical.map(|v| v.canvas))];
        SceneList {
            scenes,
            hold_scenes,
            program: self.scene,
            preview: self.preview(),
            master: self
                .collection
                .canvas(self.canvas)
                .map(|c| c.master)
                .unwrap_or_default(),
            glide: self.collection.glide(),
            transition: self.collection.transition(),
            stinger_cut_found_ms: self.stinger.cut_found_ms(),
            stinger_length_ms: self
                .stinger_length()
                .map(|l| u32::try_from(l.as_millis()).unwrap_or(u32::MAX)),
            stinger_looking: self.stinger.looking(),
            stinger_alpha_unsupported: self.stinger.alpha_unsupported(),
            sources,
        }
    }

    pub(super) fn name_of(&self, id: SourceId) -> String {
        self.collection
            .source(id)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    pub(super) fn publish_transitions(&mut self) {
        for e in self.studio.take_transition_events() {
            let canvas = if e.canvas == self.canvas {
                OutputCanvas::Main
            } else if self.vertical.map(|v| v.canvas) == Some(e.canvas) {
                OutputCanvas::Vertical
            } else {
                continue;
            };
            // The held copy (ADR-0063) is named by its real scene.
            let real = |id: SceneId, held: Option<SceneId>| {
                if id.get() >= HELD_IDS { held } else { Some(id) }
            };
            let now_held = self.held.as_ref().map(|h| h.from);
            let (Some(from), Some(to)) = (
                e.from.and_then(|id| real(id, self.held_was.or(now_held))),
                e.to.and_then(|id| real(id, now_held.or(self.held_was))),
            ) else {
                continue;
            };
            let state = api::TransitionStateChanged {
                canvas,
                state: match e.phase {
                    TransitionPhase::Started => api::TransitionPhase::Started,
                    TransitionPhase::Holding => api::TransitionPhase::Holding,
                    _ => api::TransitionPhase::Ended,
                },
                kind: api_transition_kind(e.kind),
                duration_ms: u32::try_from(e.duration.as_millis()).unwrap_or(u32::MAX),
                from: from.get(),
                to: to.get(),
                at: e.at.as_nanos(),
                manual: e.manual,
                t: e.progress,
            };
            self.events
                .changed(canvas_key(Topic::TransitionState, canvas), || {
                    EventPayload::TransitionState(state)
                });
        }
    }

    /// The window commands of this concern, routed by `Host::command`.
    pub(super) fn scene_command(&mut self, c: Command, notify: &dyn Fn(Update)) {
        match c {
            Command::RenameSource { id, name } => {
                let done = self.rename_source(id, &name);
                self.answer(done, notify);
            }
            Command::SetGlide {
                duration_ms,
                easing,
            } => {
                let done = self.set_glide(duration_ms, easing);
                self.answer(done, notify);
            }
            Command::SetTransition(t) => {
                let done = self.put_transition(t);
                self.answer(done, notify);
            }
            Command::SetSourceActivity { id, run, on_return } => {
                let done = self.set_source_choice(id, run, on_return);
                self.answer(done, notify);
            }
            Command::SetSourceGlide {
                id,
                custom,
                duration_ms,
                easing,
            } => {
                let done = self.set_source_glide(id, custom, duration_ms, easing);
                self.answer(done, notify);
            }
            Command::AddExistingSource(id) => {
                let done = self.add_existing(id).map(|_| true);
                self.answer(done, notify);
            }
            Command::AddScene(name) => {
                let done = self.add_scene(name.as_deref()).map(|_| true);
                self.answer(done, notify);
            }
            Command::DuplicateScene(id) => {
                let done = self.duplicate_scene(id).map(|_| true);
                self.answer(done, notify);
            }
            Command::DuplicateSceneWithCopies(id) => {
                let p = rbs_api::request::DuplicateScene {
                    scene: id.get(),
                    sources: rbs_api::types::DuplicateSources::Copied,
                };
                let done = self.api_scene_duplicate(p, notify);
                self.answer_api(done, notify);
            }
            Command::DuplicateSource(id) => {
                let done = self.api_source_duplicate(in_scene(id), notify);
                self.answer_api(done, notify);
            }
            Command::CopyAttributes(id) => {
                let done = self.api_copy_attributes(in_scene(id), notify);
                self.answer_api(done, notify);
            }
            Command::OpenAttributes { source, paste } => {
                match self.attributes_view(source, paste) {
                    Ok(view) => notify(Update::Attributes(view)),
                    Err(e) => notify(Update::Error(e.text().to_owned())),
                }
            }
            Command::ResetSource { source, groups } => {
                let p = rbs_api::request::ResetSource {
                    source: source.get(),
                    scene: None,
                    canvas: OutputCanvas::default(),
                    groups: Some(groups.iter().map(|g| g.id().to_owned()).collect()),
                };
                let done = self.api_source_reset(p, notify);
                self.answer_api(done, notify);
            }
            Command::PasteAttributes {
                items,
                groups,
                append,
            } => {
                let p = rbs_api::request::PasteAttributes {
                    items: items.iter().map(|i| i.get()).collect(),
                    groups: groups.iter().map(|g| g.id().to_owned()).collect(),
                    filters: if append {
                        rbs_api::types::FilterPasteMode::Append
                    } else {
                        rbs_api::types::FilterPasteMode::Replace
                    },
                    canvas: OutputCanvas::default(),
                };
                let done = self.api_paste_attributes(p, notify);
                self.answer_api(done, notify);
            }
            Command::RemoveScene(id) => {
                let done = self.remove_scene(id).map(|()| true);
                self.answer(done, notify);
            }
            Command::RenameScene { id, name } => {
                let done = self.rename_scene(id, &name);
                self.answer(done, notify);
            }
            Command::SetSceneTransition { id, transition } => {
                let done = self.set_scene_transition(id, transition);
                self.answer(done, notify);
            }
            Command::SwitchScene(id) => {
                let done = if self.preview().is_some() {
                    self.set_preview_scene(id)
                } else {
                    self.switch_scene(id)
                };
                self.answer(done, notify);
            }
            Command::SetStudioMode(on) => {
                let done = self.set_studio_mode(on);
                self.answer(done, notify);
            }
            Command::Transition => {
                let done = self.transition();
                self.answer(done, notify);
            }
            Command::TransitionProgress(t) => {
                if self.preview().is_some() {
                    self.pending_progress = Some(t.clamp(0.0, 1.0));
                }
            }
            Command::Rehearse => {
                if self.preview().is_some()
                    && let Err(e) = self.studio.rehearse(&self.adapters.sources)
                {
                    notify(Update::Error(format!("The rehearsal could not start: {e}")));
                }
            }
            Command::SetSwapAfterTransition(on) => self.swap_after_transition = on,
            // Routed elsewhere by `Host::command`.
            _ => {}
        }
    }
}
