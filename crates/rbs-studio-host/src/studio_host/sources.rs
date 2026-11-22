//! Sources: adding, sharing (global sources), editing, moving and deleting them (ADR-0159 decision 8).

use super::*;

impl Host {
    /// Adds a source of `kind` for `target` to the program scene, on top.
    /// Puts a target first among the recent ones of its source type and saves the list
    /// (a small file, written only when the list changes). An empty `name` is looked up
    /// in the listed targets; a target that is not listed is not remembered.
    pub(super) fn remember_target(
        &mut self,
        kind: SourceKind,
        adapter: &str,
        target: &str,
        name: &str,
    ) {
        let name = if name.is_empty() {
            match self
                .targets(kind)
                .into_iter()
                .find(|t| t.adapter == adapter && t.target == target)
            {
                Some(t) => t.name,
                None => return,
            }
        } else {
            name.to_owned()
        };
        let changed = self
            .recent
            .push(kind.type_id().as_str(), adapter, target, &name);
        if changed && let Some(dir) = &self.config_dir {
            // A list that cannot be saved is still kept for this run.
            let _ = crate::recent_targets::save(dir, &self.recent);
        }
    }

    /// The recent targets of `kind` that still exist, newest first.
    pub(super) fn recent_targets(&self, kind: SourceKind) -> Vec<Target> {
        let mut list: Vec<Target> = self
            .targets(kind)
            .into_iter()
            .filter(|t| t.recent.is_some())
            .collect();
        list.sort_by_key(|t| t.recent);
        list
    }

    /// Forgets the recent targets of `kind` and saves the list.
    pub(super) fn clear_recent_targets(&mut self, kind: SourceKind) {
        if self.recent.clear(kind.type_id().as_str())
            && let Some(dir) = &self.config_dir
        {
            let _ = crate::recent_targets::save(dir, &self.recent);
        }
    }

    pub(super) fn add(&mut self, kind: SourceKind, target: &Target) -> Result<SourceId, Refusal> {
        // A screen or window is as large as fits with its own aspect ratio, centred
        // (a 21:9 display is not squeezed into a 16:9 canvas); audio has no picture,
        // the item only places it in the scene.
        let (settings, layout) = self.new_source(kind, target)?;
        let name = unique_name(&self.collection, &target.name);
        let id = self.add_named(kind, &name, settings, layout)?;
        self.remember_target(kind, &target.adapter, &target.target, &target.name);
        // Hints are not a step of their own: undoing the add removes the source. The
        // source was just added, so the write cannot fail; a lost hint only costs a
        // later Reload by hints.
        if let Some(source) = self.collection.source(id) {
            let settings = source.settings.clone();
            let _ = self
                .collection
                .set_source_settings_and_hints(id, settings, hints_of(target));
        }
        Ok(id)
    }

    /// Settings and layout a new source of `kind` capturing `target` gets.
    pub(super) fn new_source(
        &self,
        kind: SourceKind,
        target: &Target,
    ) -> Result<(Settings, Layout), Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::Failed("the canvas is gone".to_owned()))?;
        let (w, h) = (canvas.size.width, canvas.size.height);
        let mut settings = Settings::new();
        settings.insert(
            keys::ADAPTER.into(),
            SettingValue::Text(target.adapter.clone()),
        );
        settings.insert(
            keys::TARGET.into(),
            SettingValue::Text(target.target.clone()),
        );
        let layout = match kind {
            SourceKind::Screen | SourceKind::Window => {
                // The option defaults Reset source uses too (ADR-0133 decision 2).
                settings.extend(self.option_defaults(&kind.type_id()));
                match target.size {
                    Some((sw, sh)) => Layout::fit(w, h, sw, sh),
                    None => Layout::new([0.0, 0.0], [w as f32, h as f32]),
                }
            }
            SourceKind::Camera => {
                settings.extend(self.option_defaults(&kind.type_id()));
                camera_layout(w, h)
            }
            _ => Layout::new([0.0, 0.0], [1.0, 1.0]),
        };
        Ok((settings, layout))
    }

    /// The layout a new source of `kind` gets, with no target known.
    pub(super) fn default_layout(&self, kind: Option<SourceKind>) -> Layout {
        let (w, h) = self
            .collection
            .canvas(self.canvas)
            .map_or((1, 1), |c| (c.size.width, c.size.height));
        match kind {
            Some(SourceKind::Camera) => camera_layout(w, h),
            Some(k) if k.is_audio() => Layout::new([0.0, 0.0], [1.0, 1.0]),
            _ => Layout::new([0.0, 0.0], [w as f32, h as f32]),
        }
    }

    /// The canvas's global scene.
    pub(super) fn globals(&self) -> Option<SceneId> {
        self.collection.globals_of(self.canvas)
    }

    /// A scene the UI and the API list: it exists and is not a global scene.
    pub(super) fn listed(&self, id: SceneId) -> bool {
        self.collection.scene(id).is_some() && !self.collection.is_global_scene(id)
    }

    /// The global item of a source on the studio's canvas.
    pub(super) fn global_item(&self, id: SourceId) -> Option<ItemId> {
        let scene = self.collection.scene(self.globals()?)?;
        scene
            .items
            .iter()
            .find(|i| i.content == ItemContent::Source(id))
            .map(|i| i.id)
    }

    /// The canvas's global scene, made (with its records) when it has none yet.
    pub(super) fn ensure_globals(&mut self, records: &mut Vec<Undo>) -> Result<SceneId, Refusal> {
        if let Some(g) = self.globals() {
            return Ok(g);
        }
        let name = unique_scene_name(&self.collection, "Global sources");
        let (id, undo) = self
            .collection
            .add_scene(&name, SceneScope::Canvas(self.canvas))
            .map_err(|e| Refusal::of(&e))?;
        records.push(undo);
        let undo = self
            .collection
            .set_globals(self.canvas, Some(id))
            .map_err(|e| Refusal::of(&e))?;
        records.push(undo);
        Ok(id)
    }

    /// Puts back what `records` did, newest first, after a step failed half-way.
    pub(super) fn roll_back(&mut self, records: Vec<Undo>) {
        for r in records.into_iter().rev() {
            let _ = self.collection.undo(r);
        }
    }

    /// Scenes reachable from `scene` through nesting, itself included.
    pub(super) fn reachable(&self, scene: SceneId) -> std::collections::BTreeSet<SceneId> {
        let mut seen = std::collections::BTreeSet::new();
        let mut stack = vec![scene];
        while let Some(s) = stack.pop() {
            if !seen.insert(s) {
                continue;
            }
            if let Some(sc) = self.collection.scene(s) {
                stack.extend(sc.items.iter().filter_map(|i| match i.content {
                    ItemContent::Scene(n) => Some(n),
                    _ => None,
                }));
            }
        }
        seen
    }

    /// Remove from scene (ADR-0065 decision 7, ADR-0142): the item goes, with the
    /// companion items its scene no longer needs; the source too when it was its last
    /// placement. The rule is the model's (`SceneCollection::remove_from_scene`); the
    /// history entry carries the source's name, Delete when it was deleted, else
    /// Remove. Returns the name and what was removed.
    pub(super) fn remove_from_scene(&mut self, item: ItemId) -> Result<(String, Removed), Refusal> {
        let gone = || Refusal::NotFound(format!("There is no item {}.", item.get()));
        let (_, it) = self.collection.item(item).ok_or_else(gone)?;
        let ItemContent::Source(source) = it.content else {
            return Err(Refusal::Invalid(
                "Only a source can be removed from a scene here.".to_owned(),
            ));
        };
        let name = self.name_of(source);
        let (removed, undo) = self
            .collection
            .remove_from_scene(item)
            .map_err(|e| Refusal::of(&e))?;
        let kind = if removed.source_deleted {
            EditKind::Delete
        } else {
            EditKind::Remove
        };
        self.edits.history.record(kind, &name, 0, vec![undo]);
        Ok((name, removed))
    }

    /// Makes a source global on the studio's canvas (ADR-0065 decision 5): one item in
    /// the global scene, its items in the canvas's scenes removed; one undo step.
    pub(super) fn make_global(&mut self, id: SourceId) -> Result<ItemId, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let name = source.name.clone();
        let kind = SourceKind::of(&source.type_id);
        if self.global_item(id).is_some() {
            return Err(Refusal::Conflict(format!(
                "{name} is already a global source."
            )));
        }
        let globals = self.globals();
        let leaving: Vec<(SceneId, ItemId)> = self
            .collection
            .placements_of(id)
            .into_iter()
            .filter(|(scene, _)| {
                Some(*scene) != globals
                    && self
                        .collection
                        .showable_scenes(self.canvas)
                        .into_iter()
                        .any(|s| s.id == *scene)
            })
            .collect();
        // No other canvas may lose a source it shows.
        for c in self.collection.canvases().filter(|c| c.id != self.canvas) {
            for shown in [c.program, c.preview].into_iter().flatten() {
                let reach = self.reachable(shown);
                if leaving.iter().any(|(s, _)| reach.contains(s)) {
                    return Err(Refusal::Conflict(format!(
                        "{name} is on air on {}.",
                        c.name
                    )));
                }
            }
        }
        let edited = self.edited();
        let layout = leaving
            .iter()
            .find(|(s, _)| *s == edited)
            .or_else(|| leaving.first())
            .and_then(|(_, item)| self.collection.item(*item))
            .map(|(_, i)| self.shown_layout(i))
            .unwrap_or_else(|| self.default_layout(kind));
        let mut records = Vec::new();
        let done = (|| {
            let g = self.ensure_globals(&mut records)?;
            for (_, item) in &leaving {
                let undo = self
                    .collection
                    .remove_item(*item)
                    .map_err(|e| Refusal::of(&e))?;
                records.push(undo);
            }
            let (item, undo) = self
                .collection
                .add_item(
                    g,
                    ItemContent::Source(id),
                    self.stored_layout(&layout),
                    None,
                )
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            Ok(item)
        })();
        match done {
            Ok(item) => {
                self.edits
                    .history
                    .record(EditKind::Global, &name, 0, records);
                Ok(item)
            }
            Err(e) => {
                self.roll_back(records);
                Err(e)
            }
        }
    }

    /// Takes a source out of the global sources: with `keep` its item moves into the
    /// edited scene with its layout (answers the new item), else it is removed from the
    /// global scene as Remove from scene does (answers what was removed). One undo step.
    pub(super) fn unmake_global(
        &mut self,
        id: SourceId,
        keep: bool,
    ) -> Result<Result<ItemId, Removed>, Refusal> {
        let name = self.name_of(id);
        let Some(item) = self.global_item(id) else {
            return Err(if self.collection.source(id).is_none() {
                Refusal::NotFound(format!("There is no source {}.", id.get()))
            } else {
                Refusal::Invalid(format!("{name} is not a global source."))
            });
        };
        if !keep {
            let (_, removed) = self.remove_from_scene(item)?;
            return Ok(Err(removed));
        }
        let layout = self
            .collection
            .item(item)
            .map(|(_, i)| self.shown_layout(i))
            .unwrap_or_else(|| self.default_layout(None));
        let mut records = Vec::new();
        let done = (|| {
            let undo = self
                .collection
                .remove_item(item)
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            let (new, undo) = self
                .collection
                .add_item(
                    self.edited(),
                    ItemContent::Source(id),
                    self.stored_layout(&layout),
                    None,
                )
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            Ok(new)
        })();
        match done {
            Ok(new) => {
                self.edits
                    .history
                    .record(EditKind::Global, &name, 0, records);
                Ok(Ok(new))
            }
            Err(e) => {
                self.roll_back(records);
                Err(e)
            }
        }
    }

    /// Adds a new source as a global source (Settings › Global sources, `source.add`
    /// with `global`); one undo step.
    pub(super) fn add_global(
        &mut self,
        kind: SourceKind,
        target: &Target,
    ) -> Result<SourceId, Refusal> {
        let (settings, layout) = self.new_source(kind, target)?;
        self.add_global_with(&target.name, kind, settings, layout)
    }

    /// [`Host::add_global`] with the settings and layout given.
    pub(super) fn add_global_with(
        &mut self,
        base: &str,
        kind: SourceKind,
        settings: Settings,
        layout: Layout,
    ) -> Result<SourceId, Refusal> {
        let name = unique_name(&self.collection, base);
        let mut records = Vec::new();
        let done = (|| {
            let g = self.ensure_globals(&mut records)?;
            let (id, undo) = self
                .collection
                .add_source(&name, kind.type_id(), settings)
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            let (_, undo) = self
                .collection
                .add_item(
                    g,
                    ItemContent::Source(id),
                    self.stored_layout(&layout),
                    None,
                )
                .map_err(|e| Refusal::of(&e))?;
            records.push(undo);
            Ok(id)
        })();
        match done {
            Ok(id) => {
                self.edits.history.record(EditKind::Add, &name, 0, records);
                self.fill_device_offset(id, None, &name, EditKind::Add);
                Ok(id)
            }
            Err(e) => {
                self.roll_back(records);
                Err(e)
            }
        }
    }

    /// The canvas's global sources, top first (ADR-0065).
    pub(super) fn global_rows(&self) -> Vec<SourceRow> {
        match self.globals() {
            Some(g) => self.rows_of(g),
            None => Vec::new(),
        }
    }

    pub(super) fn add_named(
        &mut self,
        kind: SourceKind,
        name: &str,
        settings: Settings,
        layout: Layout,
    ) -> Result<SourceId, Refusal> {
        let (id, source) = self
            .collection
            .add_source(name, kind.type_id(), settings)
            .map_err(|e| Refusal::Failed(format!("{e:?}")))?;
        match self.collection.add_item(
            self.edited(),
            ItemContent::Source(id),
            self.stored_layout(&layout),
            None,
        ) {
            Ok((_, item)) => {
                self.edits
                    .history
                    .record(EditKind::Add, name, 0, vec![source, item]);
                self.fill_device_offset(id, None, name, EditKind::Add);
                Ok(id)
            }
            Err(e) => {
                let _ = self.collection.undo(source);
                Err(Refusal::Failed(format!("{e:?}")))
            }
        }
    }

    /// Adds the "Capture audio too" companion of a picture source (ADR-0096): a Display
    /// gets the system sound without RBS's own sound, a Window its application's
    /// sound. The companion joins the picture's last history step when `join` is set
    /// (adding both is one undo step), else it is a step of its own. It goes in the
    /// scene of the picture's first item, named after the picture.
    pub(super) fn add_companion(
        &mut self,
        picture: SourceId,
        join: bool,
    ) -> Result<SourceId, Refusal> {
        let source = self
            .collection
            .source(picture)
            .ok_or_else(|| Refusal::NotFound("This source is not in the scene any more.".into()))?
            .clone();
        if self
            .collection
            .sources()
            .any(|s| s.companion_of() == Some(picture))
        {
            return Err(Refusal::Conflict(format!(
                "{} already has its sound.",
                source.name
            )));
        }
        let scene = self
            .collection
            .placements_of(picture)
            .first()
            .map(|(scene, _)| *scene)
            .ok_or_else(|| Refusal::Invalid("The source is not placed in any scene.".into()))?;
        let (kind, target, name, exclude) = match SourceKind::of(&source.type_id) {
            Some(SourceKind::Screen) => {
                if !self.adapters.sources.can_exclude_own_sound() {
                    return Err(Refusal::Failed(
                        "This system cannot leave RBS's own sound out of the system sound.".into(),
                    ));
                }
                let target = self
                    .targets(SourceKind::SystemAudio)
                    .into_iter()
                    .find(|t| t.is_default)
                    .ok_or_else(|| {
                        Refusal::Failed("There is no sound output to capture.".into())
                    })?;
                (
                    SourceKind::SystemAudio,
                    target,
                    "System sound".to_owned(),
                    true,
                )
            }
            Some(SourceKind::Window) => {
                let app = source.hints.get(HINT_APP).cloned().ok_or_else(|| {
                    Refusal::Failed("This window's application is not known.".into())
                })?;
                let target = self
                    .targets(SourceKind::ApplicationAudio)
                    .into_iter()
                    .find(|t| t.name == app)
                    .ok_or_else(|| {
                        Refusal::Failed(format!("{app} is not playing sound that can be captured."))
                    })?;
                (
                    SourceKind::ApplicationAudio,
                    target,
                    format!("{} sound", source.name),
                    false,
                )
            }
            Some(SourceKind::Camera) => {
                let target = self.camera_microphone(&source).ok_or_else(|| {
                    Refusal::Failed("No microphone belongs with this camera.".into())
                })?;
                (
                    SourceKind::AudioInput,
                    target,
                    format!("{} mic", source.name),
                    false,
                )
            }
            _ => {
                return Err(Refusal::Invalid(
                    "Only a Display, a Window or a Camera can capture audio too.".into(),
                ));
            }
        };
        let (mut settings, layout) = self.new_source(kind, &target)?;
        if exclude {
            settings.insert(keys::EXCLUDE_OWN_PROCESS.into(), SettingValue::Bool(true));
        }
        let name = unique_name(&self.collection, &name);
        let (id, undo_source) = self
            .collection
            .add_source(&name, kind.type_id(), settings.clone())
            .map_err(|e| Refusal::of(&e))?;
        let mut hints = hints_of(&target);
        hints.insert(COMPANION_OF_HINT.to_owned(), picture.get().to_string());
        let hinted = self
            .collection
            .set_source_settings_and_hints(id, settings, hints);
        let placed = hinted.and_then(|undo_hints| {
            self.collection
                .add_item(
                    scene,
                    ItemContent::Source(id),
                    self.stored_layout(&layout),
                    None,
                )
                .map(|(_, undo_item)| (undo_hints, undo_item))
        });
        match placed {
            Ok((undo_hints, undo_item)) => {
                let records = vec![undo_source, undo_hints, undo_item];
                if join {
                    self.edits
                        .history
                        .extend_last(EditKind::Add, &name, records);
                } else {
                    self.edits.history.record(EditKind::Add, &name, 0, records);
                }
                self.fill_device_offset(id, None, &name, EditKind::Add);
                Ok(id)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// Removes the companion of `picture` as one undo step.
    pub(super) fn remove_companion(&mut self, picture: SourceId) -> Result<(), Refusal> {
        let id = self
            .collection
            .sources()
            .find(|s| s.companion_of() == Some(picture))
            .map(|s| s.id)
            .ok_or_else(|| Refusal::NotFound("This source has no companion sound.".into()))?;
        self.delete(id).map(|_| ())
    }

    /// Adds a sample covering the canvas: a background at the bottom of the scene
    /// being edited, a frame on top (ADR-0074). `frame-thin` and `frame-thick` are a
    /// transparent colour with a border, so the thickness stays editable.
    pub(super) fn add_sample(&mut self, id: &str) -> Result<SourceId, Refusal> {
        let sample = rbs_sources::still::SAMPLES
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no sample picture {id:?}.")))?;
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let mut layout = Layout::new(
            [0.0, 0.0],
            [canvas.size.width as f32, canvas.size.height as f32],
        );
        let name = unique_name(&self.collection, sample.name);
        let (type_id, settings) =
            if let Some((width, radius)) = rbs_sources::still::plain_frame(sample.id) {
                layout = layout.with_style(
                    ItemStyle::new(Shape::Rectangle { radius })
                        .with_border(Some(Border::new(width, [255, 255, 255, 255]))),
                );
                let mut settings = Settings::new();
                settings.insert(
                    keys::COLOR.into(),
                    SettingValue::Text("#00000000".to_owned()),
                );
                // The border is drawn in the item's box, so the picture itself stays tiny.
                settings.insert(keys::WIDTH.into(), SettingValue::Int(2));
                settings.insert(keys::HEIGHT.into(), SettingValue::Int(2));
                (SourceKind::Color.type_id(), settings)
            } else {
                let mut settings = Settings::new();
                settings.insert(
                    keys::SAMPLE.into(),
                    SettingValue::Text(sample.id.to_owned()),
                );
                (SourceKind::Image.type_id(), settings)
            };
        let (source, undo_source) = self
            .collection
            .add_source(&name, type_id, settings)
            .map_err(|e| Refusal::of(&e))?;
        let index = (sample.kind == rbs_sources::still::SampleKind::Background).then_some(0);
        match self.collection.add_item(
            self.edited(),
            ItemContent::Source(source),
            self.stored_layout(&layout),
            index,
        ) {
            Ok((_, undo_item)) => {
                self.edits
                    .history
                    .record(EditKind::Add, &name, 0, vec![undo_source, undo_item]);
                Ok(source)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// Adds a colour shape, centred on the canvas being edited. The picture is a tiny
    /// texture; the shape is the item's outline, so the colour fills the box.
    pub(super) fn add_stamp(&mut self, kind: u8, name: Option<&str>) -> Result<SourceId, Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let short = (canvas.size.width.min(canvas.size.height) as f32 * 0.28).clamp(48.0, 480.0);
        let (base, w, h, shape) = match kind {
            0 => ("Circle", short, short, Shape::Circle),
            1 => ("Square", short, short, Shape::Rectangle { radius: 0.0 }),
            2 => (
                "Rectangle",
                short * 1.6,
                short,
                Shape::Rectangle { radius: 0.0 },
            ),
            3 => (
                "Rounded rectangle",
                short * 1.6,
                short,
                Shape::Rectangle { radius: 32.0 },
            ),
            4 => ("Ellipse", short * 1.6, short, Shape::Ellipse),
            5 => ("Triangle", short, short, Shape::Triangle),
            6 => ("Arrow", short * 1.8, short * 0.9, Shape::Arrow),
            _ => return Err(Refusal::Invalid("There is no shape like that.".into())),
        };
        let layout = Layout::new(
            [
                ((canvas.size.width as f32 - w) * 0.5).max(0.0),
                ((canvas.size.height as f32 - h) * 0.5).max(0.0),
            ],
            [w, h],
        )
        .with_style(ItemStyle::new(shape));
        let mut settings = Settings::new();
        settings.insert(keys::COLOR.into(), SettingValue::Text("#2f6fed".to_owned()));
        // The shape is the item's box. The texture only carries the fill.
        settings.insert(keys::WIDTH.into(), SettingValue::Int(2));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(2));
        self.add_still(
            name.unwrap_or(base),
            SourceKind::Color,
            settings,
            layout,
            None,
        )
    }

    /// Adds a PNG or JPEG file as an image source, fitted inside half the canvas and centred.
    pub(super) fn add_image_file(
        &mut self,
        path: &std::path::Path,
        name: Option<&str>,
    ) -> Result<SourceId, Refusal> {
        let (pw, ph, _) = rbs_sources::still::decode_picture(path).map_err(Refusal::Invalid)?;
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let max_w = canvas.size.width as f32 * 0.5;
        let max_h = canvas.size.height as f32 * 0.5;
        let scale = (max_w / pw as f32).min(max_h / ph as f32);
        let w = (pw as f32 * scale).max(1.0);
        let h = (ph as f32 * scale).max(1.0);
        let layout = Layout::new(
            [
                ((canvas.size.width as f32 - w) * 0.5).max(0.0),
                ((canvas.size.height as f32 - h) * 0.5).max(0.0),
            ],
            [w, h],
        );
        let mut settings = Settings::new();
        settings.insert(
            keys::PATH.into(),
            SettingValue::Text(path.display().to_string()),
        );
        let base = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Image");
        self.add_still(
            name.unwrap_or(base),
            SourceKind::Image,
            settings,
            layout,
            None,
        )
    }

    /// Adds a local media file as a source, centred at half the canvas.
    pub(super) fn add_browser_page(
        &mut self,
        url: &str,
        name: Option<&str>,
    ) -> Result<SourceId, Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".into()))?;
        let (w, h) = attributes::browser_size(canvas.size.width, canvas.size.height);
        let mut settings = Settings::new();
        // An empty address adds a page that waits for one (Add source › Browser:
        // Properties opens on its address); it starts nothing until then.
        if !url.trim().is_empty() {
            settings.insert(keys::URL.into(), SettingValue::Text(url.trim().to_owned()));
        }
        settings.insert(keys::WIDTH.into(), SettingValue::Int(w));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(h));
        rbs_sources::validate_settings(&SourceKind::Browser.type_id(), &settings)
            .map_err(|error| Refusal::Invalid(error.to_string()))?;
        let layout = Layout::fit(canvas.size.width, canvas.size.height, w as u32, h as u32);
        self.add_still(
            name.unwrap_or("Page"),
            SourceKind::Browser,
            settings,
            layout,
            None,
        )
    }

    pub(super) fn add_media_file(
        &mut self,
        path: &std::path::Path,
        name: Option<&str>,
    ) -> Result<SourceId, Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let w = canvas.size.width as f32 * 0.5;
        let h = canvas.size.height as f32 * 0.5;
        let layout = Layout::new(
            [
                ((canvas.size.width as f32 - w) * 0.5).max(0.0),
                ((canvas.size.height as f32 - h) * 0.5).max(0.0),
            ],
            [w, h],
        );
        let mut settings = Settings::new();
        settings.insert(
            keys::PATH.into(),
            SettingValue::Text(path.display().to_string()),
        );
        let base = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Media");
        self.add_placed(SourceKind::Media, settings, name.unwrap_or(base), &layout)
    }

    /// A new source of `kind` with `settings`, named after `base` (made unique) and
    /// placed in the edited scene with `layout`: one undo step.
    pub(super) fn add_placed(
        &mut self,
        kind: SourceKind,
        settings: Settings,
        base: &str,
        layout: &Layout,
    ) -> Result<SourceId, Refusal> {
        rbs_sources::validate_settings(&kind.type_id(), &settings)
            .map_err(|error| Refusal::Invalid(error.to_string()))?;
        let name = unique_name(&self.collection, base);
        let (source, undo_source) = self
            .collection
            .add_source(&name, kind.type_id(), settings)
            .map_err(|e| Refusal::of(&e))?;
        match self.collection.add_item(
            self.edited(),
            ItemContent::Source(source),
            self.stored_layout(layout),
            None,
        ) {
            Ok((_, undo_item)) => {
                self.edits
                    .history
                    .record(EditKind::Add, &name, 0, vec![undo_source, undo_item]);
                Ok(source)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// A new media source for `path` that no scene places, made the stinger with the
    /// transition set to Stinger (ADR-0140 decisions 1 and 10): one undo step.
    pub(super) fn set_stinger_file(&mut self, path: &std::path::Path) -> Result<SourceId, Refusal> {
        let mut settings = Settings::new();
        settings.insert(
            keys::PATH.into(),
            SettingValue::Text(path.display().to_string()),
        );
        rbs_sources::validate_settings(&SourceKind::Media.type_id(), &settings)
            .map_err(|error| Refusal::Invalid(error.to_string()))?;
        let base = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Stinger");
        let name = unique_name(&self.collection, base);
        let (source, undo_source) = self
            .collection
            .add_source(&name, SourceKind::Media.type_id(), settings)
            .map_err(|e| Refusal::of(&e))?;
        let mut t = self.collection.transition();
        t.kind = rbs_scene::TransitionKind::Stinger;
        t.stinger = rbs_scene::StingerSettings {
            source: Some(source),
            cut_ms: None,
        };
        match self.collection.set_transition(t) {
            Ok(undo_transition) => {
                self.edits.history.record(
                    EditKind::Transition,
                    &name,
                    0,
                    vec![undo_source, undo_transition],
                );
                Ok(source)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// Settings and layout of a new text source showing `text` (ADR-0129 decision 4).
    /// The box is the default 640 x 160, centred, so the add does not wait on the
    /// renderer. [`text_measure::begin`] patches the size when the measure arrives
    /// and the text and the box are still this default.
    pub(super) fn new_text(&self, text: &str) -> Result<(Settings, Layout), Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".into()))?;
        let (cw, ch) = (canvas.size.width, canvas.size.height);
        let [dw, dh] = rbs_sources::text::DEFAULT_BOX;
        let [w, h] = [dw.clamp(1, cw.max(1)), dh.clamp(1, ch.max(1))];
        let mut settings = Settings::new();
        settings.insert(keys::TEXT.into(), SettingValue::Text(text.to_owned()));
        settings.insert(keys::WIDTH.into(), SettingValue::Int(i64::from(w)));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(i64::from(h)));
        rbs_sources::validate_settings(&SourceKind::Text.type_id(), &settings)
            .map_err(|e| Refusal::Invalid(e.to_string()))?;
        let layout = Layout::new(
            [(cw as f32 - w as f32) * 0.5, (ch as f32 - h as f32) * 0.5],
            [w as f32, h as f32],
        );
        Ok((settings, layout))
    }

    /// Adds a text source showing `text` to the scene being edited (or as a global
    /// source); one undo step.
    pub(super) fn add_text(
        &mut self,
        name: Option<&str>,
        text: &str,
        global: bool,
    ) -> Result<SourceId, Refusal> {
        let (settings, layout) = self.new_text(text)?;
        let base = name.filter(|n| !n.trim().is_empty()).unwrap_or("Text");
        let id = if global {
            self.add_global_with(base, SourceKind::Text, settings, layout)
        } else {
            self.add_still(base, SourceKind::Text, settings, layout, None)
        }?;
        text_measure::begin(self, id);
        Ok(id)
    }

    /// Replaces a text source's `text` only (`source.set_text`); one undo step.
    pub(super) fn set_source_text(&mut self, id: SourceId, text: &str) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if SourceKind::of(&source.type_id) != Some(SourceKind::Text) {
            return Err(Refusal::Invalid(format!(
                "{} is not a text source.",
                source.name
            )));
        }
        let mut patch = std::collections::BTreeMap::new();
        patch.insert(
            keys::TEXT.to_owned(),
            serde_json::Value::String(text.to_owned()),
        );
        self.patch_source_settings(id, &patch)
    }

    /// The shared end of adding a still source: one source, one item, one undo step.
    pub(super) fn add_still(
        &mut self,
        base: &str,
        kind: SourceKind,
        settings: Settings,
        layout: Layout,
        index: Option<usize>,
    ) -> Result<SourceId, Refusal> {
        let name = unique_name(&self.collection, base);
        let (source, undo_source) = self
            .collection
            .add_source(&name, kind.type_id(), settings)
            .map_err(|e| Refusal::of(&e))?;
        match self.collection.add_item(
            self.edited(),
            ItemContent::Source(source),
            self.stored_layout(&layout),
            index,
        ) {
            Ok((_, undo_item)) => {
                self.edits
                    .history
                    .record(EditKind::Add, &name, 0, vec![undo_source, undo_item]);
                Ok(source)
            }
            Err(e) => {
                let _ = self.collection.undo(undo_source);
                Err(Refusal::of(&e))
            }
        }
    }

    /// Turns a colour source's fill on or off. Off clears the inside and, when the
    /// shape has no border, adds one in the fill colour so the shape stays visible.
    /// Both changes are one undo step. The stored colour is kept. `item` names the
    /// source and, on canvas `on`, the style that gets the border.
    pub(super) fn set_item_filled(
        &mut self,
        item: ItemId,
        on: bool,
        canvas: OutputCanvas,
    ) -> Result<bool, Refusal> {
        let (id, shown, own) = self.crop_target_on(item, canvas)?;
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if SourceKind::of(&source.type_id) != Some(SourceKind::Color) {
            return Err(Refusal::Invalid("Fill is for a colour source.".into()));
        }
        let was_on = !matches!(
            source.settings.get(keys::FILLED),
            Some(SettingValue::Bool(false))
        );
        if was_on == on {
            return Ok(false);
        }
        let name = source.name.clone();
        let type_id = source.type_id.clone();
        let stroke = {
            let text = match source.settings.get(keys::COLOR) {
                Some(SettingValue::Text(t)) => t.as_str(),
                _ => "",
            };
            let mut color = rbs_sources::still::parse_color(text).unwrap_or([255, 255, 255, 255]);
            color[3] = 255;
            color
        };
        let mut settings = source.settings.clone();
        if on {
            settings.remove(keys::FILLED);
        } else {
            settings.insert(keys::FILLED.to_owned(), SettingValue::Bool(false));
        }
        rbs_sources::validate_settings(&type_id, &settings)
            .map_err(|e| Refusal::Invalid(e.to_string()))?;
        let placed = (!on).then_some((item, shown, own));
        let undo_settings = self
            .collection
            .set_source_settings(id, settings)
            .map_err(|e| Refusal::of(&e))?;
        let mut records = vec![undo_settings];
        let mut kind = EditKind::SourceSettings;
        if let Some((item, mut layout, own)) = placed
            && layout.style.border.is_none()
        {
            layout.style = layout.style.with_border(Some(Border::new(4.0, stroke)));
            if !layout.style.is_valid() {
                let _ = self.collection.undo(records.remove(0));
                return Err(Refusal::Invalid(
                    "The outline could not be turned on.".into(),
                ));
            }
            match self.save_live_layout(item, own, layout) {
                Ok(undo) => {
                    records.push(undo);
                    kind = EditKind::Style;
                }
                Err(e) => {
                    let _ = self.collection.undo(records.remove(0));
                    return Err(Refusal::of(&e));
                }
            }
        }
        self.edits.history.record(kind, &name, 0, records);
        Ok(true)
    }

    /// Deletes a source from every scene; returns its name.
    pub(super) fn delete(&mut self, id: SourceId) -> Result<String, Refusal> {
        let name = self
            .collection
            .source(id)
            .map(|s| s.name.clone())
            .ok_or_else(|| {
                Refusal::NotFound("This source is not in the scene any more.".to_owned())
            })?;
        let undo = self
            .collection
            .delete_source(id)
            .map_err(|e| Refusal::Failed(format!("{e:?}")))?;
        self.edits
            .history
            .record(EditKind::Delete, &name, 0, vec![undo]);
        Ok(name)
    }

    /// Puts `item` where picture layer `to` of the same scene is (a row dragged onto
    /// another). One undo step.
    pub(super) fn move_item_to(&mut self, item: ItemId, to: ItemId) -> Result<bool, Refusal> {
        let gone = |i: ItemId| Refusal::NotFound(format!("There is no item {}.", i.get()));
        let (scene, moved) = self.collection.item(item).ok_or_else(|| gone(item))?;
        let ItemContent::Source(id) = moved.content else {
            return Err(Refusal::Invalid("Only a source is moved this way.".into()));
        };
        let (other_scene, other) = self.collection.item(to).ok_or_else(|| gone(to))?;
        if other_scene != scene || !self.has_picture(other) {
            return Err(Refusal::Invalid(
                "The place is a picture layer of the same scene.".into(),
            ));
        }
        if item == to {
            return Ok(false);
        }
        let index = self
            .collection
            .scene(scene)
            .and_then(|s| s.items.iter().position(|i| i.id == to))
            .ok_or_else(|| gone(to))?;
        let undo = self
            .collection
            .move_item(item, index)
            .map_err(|e| Refusal::of(&e))?;
        let name = self.name_of(id);
        self.edits
            .history
            .record(EditKind::Order, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Releases the device (`active` false) or keeps it open. `Ok(false)` when it
    /// already is. Hiding a source does not call this.
    pub(super) fn set_source_active(
        &mut self,
        id: SourceId,
        active: bool,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if source.standby() == !active {
            return Ok(false);
        }
        let mut hints = source.hints.clone();
        if active {
            hints.remove(STANDBY_HINT);
        } else {
            hints.insert(STANDBY_HINT.to_owned(), "1".to_owned());
        }
        let settings = source.settings.clone();
        let name = source.name.clone();
        let undo = self
            .collection
            .set_source_settings_and_hints(id, settings, hints)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::SourceSettings, &name, 0, vec![undo]);
        Ok(true)
    }

    pub(super) fn move_source(
        &mut self,
        id: SourceId,
        direction: rbs_api::request::LayerDirection,
    ) -> Result<bool, Refusal> {
        use rbs_api::request::LayerDirection::*;
        let (index, item) = self.item_of(id)?;
        let item = item.id;
        let Some(scene) = self.scene_of(item).and_then(|s| self.collection.scene(s)) else {
            return Ok(false);
        };
        let pictures: Vec<usize> = scene
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| self.has_picture(it))
            .map(|(i, _)| i)
            .collect();
        if !pictures.contains(&index) {
            return Ok(false);
        }
        let to = match direction {
            Up => pictures.iter().copied().find(|i| *i > index),
            Down => pictures.iter().copied().rfind(|i| *i < index),
            Front => pictures.last().copied().filter(|i| *i != index),
            Back => pictures.first().copied().filter(|i| *i != index),
        };
        let Some(to) = to else { return Ok(false) };
        let undo = self
            .collection
            .move_item(item, to)
            .map_err(|e| Refusal::of(&e))?;
        let name = self.name_of(id);
        self.edits
            .history
            .record(EditKind::Order, &name, 0, vec![undo]);
        Ok(true)
    }

    /// A source's volume or mute (ADR-0059): a live control, not on the undo stack,
    /// sent to the mixer without rebinding. `Ok(false)` when nothing changed.
    pub(super) fn set_source_audio(
        &mut self,
        id: SourceId,
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Result<bool, Refusal> {
        let now = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .audio;
        let mut changed = false;
        if let Some(v) = volume.filter(|v| *v != now.volume) {
            self.collection
                .set_source_volume(id, v)
                .map_err(|e| Refusal::of(&e))?;
            changed = true;
        }
        if let Some(m) = muted.filter(|m| *m != now.muted) {
            self.collection
                .set_source_muted(id, m)
                .map_err(|e| Refusal::of(&e))?;
            changed = true;
        }
        if changed {
            self.studio.set_mix(&self.collection);
        }
        Ok(changed)
    }

    /// The canvas's master volume or mute (ADR-0081): a live control, not on the undo
    /// stack, sent to the mixer without rebinding. `Ok(false)` when nothing changed.
    pub(super) fn set_master(
        &mut self,
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Result<bool, Refusal> {
        let canvas = self.canvas;
        let now = self
            .collection
            .canvas(canvas)
            .ok_or_else(|| Refusal::NotFound("There is no canvas.".to_owned()))?
            .master;
        let mut changed = false;
        if let Some(v) = volume.filter(|v| *v != now.volume) {
            self.collection
                .set_canvas_master_volume(canvas, v)
                .map_err(|e| Refusal::of(&e))?;
            changed = true;
        }
        if let Some(m) = muted.filter(|m| *m != now.muted) {
            self.collection
                .set_canvas_master_muted(canvas, m)
                .map_err(|e| Refusal::of(&e))?;
            changed = true;
        }
        if changed {
            self.studio.set_mix(&self.collection);
        }
        Ok(changed)
    }

    /// Hides a source from the Mixer or shows it (ADR-0080): a view flag, not on the
    /// undo stack, and the mix is untouched. `Ok(false)` when nothing changed.
    pub(super) fn set_source_mixer_hidden(
        &mut self,
        id: SourceId,
        hidden: bool,
    ) -> Result<bool, Refusal> {
        let now = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .audio;
        if now.mixer_hidden == hidden {
            return Ok(false);
        }
        self.collection
            .set_source_mixer_hidden(id, hidden)
            .map_err(|e| Refusal::of(&e))?;
        Ok(true)
    }

    /// Renames a source (one undo step); `Ok(false)` when it has that name.
    /// Points a source at `target`: its `adapter` and `target` settings change, nothing
    /// else, so name, layout, filters and every scene's item stay. The binder replaces
    /// the node (break before make). One undo step; the same target records nothing.
    pub(super) fn retarget_source(
        &mut self,
        id: SourceId,
        target: &Target,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let name = source.name.clone();
        let kind = SourceKind::of(&source.type_id);
        let old_device = device_sync::device_key(source);
        let mut settings = source.settings.clone();
        let same = |key: &str, value: &str| matches!(settings.get(key), Some(SettingValue::Text(t)) if t == value);
        if same(keys::ADAPTER, &target.adapter) && same(keys::TARGET, &target.target) {
            return Ok(false);
        }
        settings.insert(
            keys::ADAPTER.into(),
            SettingValue::Text(target.adapter.clone()),
        );
        settings.insert(
            keys::TARGET.into(),
            SettingValue::Text(target.target.clone()),
        );
        let hints = hints_of(target);
        let undo = self
            .collection
            .set_source_settings_and_hints(id, settings, hints)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::Retarget, &name, 0, vec![undo]);
        self.fill_device_offset(id, old_device.as_ref(), &name, EditKind::Retarget);
        if let Some(kind) = kind {
            self.remember_target(kind, &target.adapter, &target.target, &target.name);
        }
        Ok(true)
    }

    /// The target `adapter` / `target` as this host lists it for `id`'s kind (no OS
    /// picker entry): `not_found` for an unknown source or a target not listed.
    pub(super) fn listed_target(
        &self,
        id: SourceId,
        adapter: &str,
        target: &str,
    ) -> Result<Target, ApiError> {
        let source = self.collection.source(id).ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("there is no source {}", id.get()),
            )
        })?;
        SourceKind::of(&source.type_id)
            .and_then(|kind| {
                self.targets(kind)
                    .into_iter()
                    .find(|t| !t.picker && t.adapter == adapter && t.target == target)
            })
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("{adapter} does not list the target {target} for this source"),
                )
            })
    }

    pub(super) fn rename_source(&mut self, id: SourceId, name: &str) -> Result<bool, Refusal> {
        let old = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .name
            .clone();
        if old == name.trim() {
            return Ok(false);
        }
        let undo = self
            .collection
            .rename_source(id, name)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::RenameSource, &old, 0, vec![undo]);
        Ok(true)
    }

    pub(super) fn set_source_glide(
        &mut self,
        id: SourceId,
        custom: bool,
        duration_ms: u32,
        easing: u8,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let name = source.name.clone();
        let glide = custom.then_some(rbs_scene::SourceGlide {
            duration_ms,
            easing: match easing {
                0 => rbs_scene::GlideEasing::Linear,
                _ => rbs_scene::GlideEasing::EaseInOut,
            },
        });
        if source.glide == glide {
            return Ok(false);
        }
        let undo = self
            .collection
            .set_source_glide(id, glide)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::Glide, &name, 0, vec![undo]);
        Ok(true)
    }

    /// The window commands of this concern, routed by `Host::command`.
    pub(super) fn source_command(&mut self, c: Command, notify: &dyn Fn(Update)) {
        match c {
            Command::PickSource { kind, .. } if self.prompt_first(kind).is_some() => {
                if let Some(p) = self.prompt_first(kind) {
                    self.ask_permission(p, Some(c), notify);
                }
            }
            Command::PickSource { kind, adapter } => self.pick(kind, &adapter),
            Command::ReadPermissions => self.send_permissions(notify),
            Command::RequestPermission(kind) => self.ask_permission(kind, None, notify),
            Command::SetSourceVolume { id, volume } => {
                let call = if self.levels_take(id) {
                    rbs_api::Command::SceneAudioSet(rbs_api::request::SetSceneLevel {
                        scene: self.scene.get(),
                        source: id.get(),
                        volume: Some(volume),
                        muted: None,
                    })
                } else {
                    rbs_api::Command::SourceSetVolume(rbs_api::request::SetSourceVolume {
                        source: id.get(),
                        volume,
                    })
                };
                self.run_ui_api(call, notify);
            }
            Command::SetSourceMuted { id, muted } => {
                let call = if self.levels_take(id) {
                    rbs_api::Command::SceneAudioSet(rbs_api::request::SetSceneLevel {
                        scene: self.scene.get(),
                        source: id.get(),
                        volume: None,
                        muted: Some(muted),
                    })
                } else {
                    rbs_api::Command::SourceSetMuted(rbs_api::request::SetSourceMuted {
                        source: id.get(),
                        muted,
                    })
                };
                self.run_ui_api(call, notify);
            }
            Command::SetMetersUi(on) => {
                self.meters_ui = on;
                self.sync_meters();
            }
            Command::EnsureEncoders => {
                self.ensure_encoders();
                self.publish_encoders(notify);
            }
            Command::SetSceneOwnLevels { scene, own } => {
                let scene = rbs_api::request::SceneRef { scene: scene.get() };
                self.run_ui_api(
                    if own {
                        rbs_api::Command::SceneAudioOwn(scene)
                    } else {
                        rbs_api::Command::SceneAudioShared(scene)
                    },
                    notify,
                );
            }
            Command::ResetSceneLevel { scene, source } => {
                self.run_ui_api(
                    rbs_api::Command::SceneAudioResetSource(rbs_api::request::SceneSourceRef {
                        scene: scene.get(),
                        source: source.get(),
                    }),
                    notify,
                );
            }
            Command::SetSourceMixerHidden { id, hidden } => {
                self.run_ui_api(
                    rbs_api::Command::SourceSetMixerHidden(
                        rbs_api::request::SetSourceMixerHidden {
                            source: id.get(),
                            hidden,
                        },
                    ),
                    notify,
                );
            }
            // Routed elsewhere by `Host::command`.
            _ => {}
        }
    }
}
