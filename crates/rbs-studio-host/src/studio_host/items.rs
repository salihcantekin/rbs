//! Scene items: visibility, locking, boxes, styles, scrolling, crops, transforms and the display chrome fit (ADR-0159 decision 8).

use super::*;

impl Host {
    /// The program scene's item that places `id`, and its index (0 is the bottom).
    pub(super) fn item_of(&self, id: SourceId) -> Result<(usize, &SceneItem), Refusal> {
        let gone = || Refusal::NotFound("This source is not in the scene any more.".to_owned());
        let find = |scene: SceneId| {
            self.collection.scene(scene).and_then(|s| {
                s.items
                    .iter()
                    .enumerate()
                    .find(|(_, i)| i.content == ItemContent::Source(id))
            })
        };
        // A global source's item is in the canvas's global scene (ADR-0065).
        find(self.edited())
            .or_else(|| self.globals().and_then(find))
            .ok_or_else(gone)
    }

    /// True when the item shows a picture (a source that is not sound only).
    pub(super) fn has_picture(&self, item: &SceneItem) -> bool {
        match item.content {
            ItemContent::Source(id) => self
                .collection
                .source(id)
                .and_then(|s| SourceKind::of(&s.type_id))
                .is_some_and(|k| !k.is_audio()),
            _ => true,
        }
    }

    /// Locks or unlocks an item (ADR-0059); `Ok(false)` when it already is.
    pub(super) fn set_item_locked(&mut self, item: ItemId, locked: bool) -> Result<bool, Refusal> {
        let (_, found) = self
            .collection
            .item(item)
            .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", item.get())))?;
        if found.locked == locked {
            return Ok(false);
        }
        let name = self.item_name(found);
        let undo = self
            .collection
            .set_item_locked(item, locked)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::Lock, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Shows or hides an item; `Ok(false)` when it already is.
    pub(super) fn set_item_visible(
        &mut self,
        item: ItemId,
        visible: bool,
    ) -> Result<bool, Refusal> {
        let (_, found) = self
            .collection
            .item(item)
            .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", item.get())))?;
        if found.visible == visible {
            return Ok(false);
        }
        let name = self.item_name(found);
        let undo = self
            .collection
            .set_item_visible(item, visible)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::Visibility, &name, 0, vec![undo]);
        // Opening the eye drops a running cue so the deadline cannot hide it later
        // (ADR-0105). Hiding does not start one.
        if visible {
            self.studio.drop_cue(item);
        }
        Ok(true)
    }

    /// Sets or clears `show_for_ms` (ADR-0105). One undo step. `Ok(false)` when the
    /// item already has that duration.
    pub(super) fn set_item_show_for(
        &mut self,
        item: ItemId,
        show_for_ms: Option<u32>,
    ) -> Result<bool, Refusal> {
        let (_, found) = self
            .collection
            .item(item)
            .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", item.get())))?;
        if found.show_for_ms == show_for_ms {
            return Ok(false);
        }
        let name = self.item_name(found);
        let undo = self
            .collection
            .set_item_show_for(item, show_for_ms)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::ShowFor, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Shows a hidden item until `now + show_for_ms` (ADR-0105). Live: no undo, and
    /// `visible` is not written. `Ok(false)` when the item is already visible or has
    /// no duration. Unknown items are `Err`.
    pub(super) fn show_item(&mut self, item: ItemId) -> Result<bool, Refusal> {
        let (_, found) = self
            .collection
            .item(item)
            .ok_or_else(|| Refusal::NotFound(format!("There is no item {}.", item.get())))?;
        Ok(self.studio.show_cue(item, found.visible, found.show_for_ms))
    }

    /// The name of what an item places.
    pub(super) fn item_name(&self, item: &SceneItem) -> String {
        match item.content {
            ItemContent::Source(id) => self.name_of(id),
            ItemContent::Scene(id) => self
                .collection
                .scene(id)
                .map(|s| s.name.clone())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// Places the edited scene's item for `id` using the visible picture size
    /// (the region and the item's own crop) in its own box: same centre, area,
    /// rotation and flips, the visible picture's aspect (ADR-0178). `None` when the
    /// size is unknown or nothing changes.
    pub(super) fn fit_item_to_visible(&mut self, id: SourceId) -> Result<Option<Undo>, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let picture = self.sizes.get(&id).copied().ok_or_else(|| {
            Refusal::Invalid("This source has no picture yet, so its size is not known.".into())
        })?;
        let region = source.region;
        let item = self.item_of(id)?.1.id;
        let (_, old, own) = self.crop_target(item).map_err(|e| match e {
            CropRefusal::NotFound(t) => Refusal::NotFound(t),
            CropRefusal::Invalid(t) => Refusal::Invalid(t),
            CropRefusal::Conflict(t) => Refusal::Conflict(t),
            CropRefusal::NoPicture | CropRefusal::Failed(_) => Refusal::Failed(e.text()),
        })?;
        let layout = visible_size(picture, region, old.crop)
            .and_then(|visible| old.fitted_in_box(visible))
            .ok_or_else(|| Refusal::Invalid("The region leaves no picture.".into()))?;
        if layout == old {
            return Ok(None);
        }
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        Ok(Some(undo))
    }

    /// Writes display chrome and the region those insets describe (ADR-0104).
    /// The item box stays. `None` clears chrome and the region.
    pub(super) fn set_display_chrome(
        &mut self,
        id: SourceId,
        chrome: Option<DisplayChrome>,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if source.type_id.as_str() != rbs_scene::DISPLAY_SOURCE_TYPE {
            return Err(Refusal::Invalid(
                "Only a display source can hide the taskbar or Dock.".into(),
            ));
        }
        if let Some(chrome) = chrome
            && let Some(picture) = self.sizes.get(&id).copied()
            && chrome.region().size(picture).is_none()
        {
            return Err(Refusal::Invalid("The region leaves no picture.".into()));
        }
        // Chrome equal but region cleared (e.g. Apply after chrome wrote insets) still
        // needs a write so the cut comes back (ADR-0104).
        let region_matches = match chrome {
            None => source.region.is_none(),
            Some(c) => source.region == Some(c.region()),
        };
        if source.chrome == chrome && region_matches {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_display_chrome(id, chrome)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::Region, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Places the source's item with the stored chrome fill (ADR-0104). The region
    /// and the capture node stay.
    pub(super) fn fit_display_chrome(&mut self, id: SourceId) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let chrome = source
            .chrome
            .ok_or_else(|| Refusal::Invalid("Chrome is off.".into()))?;
        let name = source.name.clone();
        let picture = self.sizes.get(&id).copied().ok_or_else(|| {
            Refusal::Invalid("This source has no picture yet, so its size is not known.".into())
        })?;
        let item = self.item_of(id)?.1.id;
        let (_, old, own) = self.crop_target(item).map_err(|e| match e {
            CropRefusal::NotFound(t) => Refusal::NotFound(t),
            CropRefusal::Invalid(t) => Refusal::Invalid(t),
            CropRefusal::Conflict(t) => Refusal::Conflict(t),
            CropRefusal::NoPicture | CropRefusal::Failed(_) => Refusal::Failed(e.text()),
        })?;
        let canvas = self
            .collection
            .canvas(self.canvas)
            .map(|c| (c.size.width, c.size.height))
            .ok_or_else(|| Refusal::Failed("the canvas is gone".into()))?;
        let layout = place_chrome(chrome, canvas, picture, old)
            .ok_or_else(|| Refusal::Invalid("The region leaves no picture.".into()))?;
        if layout == old {
            return Ok(false);
        }
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::Transform, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Copies chrome insets from the display's work area. A no-op, with no undo
    /// step, when the adapter did not report `work_rect` (ADR-0104).
    pub(super) fn reset_display_chrome(&mut self, id: SourceId) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if source.type_id.as_str() != rbs_scene::DISPLAY_SOURCE_TYPE {
            return Err(Refusal::Invalid(
                "Only a display source can hide the taskbar or Dock.".into(),
            ));
        }
        let fill = source.chrome.map(|c| c.fill).unwrap_or(ChromeFill::Cover);
        let Some((work, picture)) = self.display_work(id) else {
            return Ok(false);
        };
        let Some(chrome) = DisplayChrome::from_work(
            picture,
            Some((work.x, work.y, work.width, work.height)),
            fill,
        ) else {
            return Ok(false);
        };
        if source.chrome == Some(chrome) {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_display_chrome(id, Some(chrome))
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::Region, &name, 0, vec![undo]);
        Ok(true)
    }

    /// The listed display's work area and the picture size it belongs to.
    pub(super) fn display_work(&self, id: SourceId) -> Option<(rbs_capture::WorkRect, (u32, u32))> {
        let source = self.collection.source(id)?;
        let target_id = match source.settings.get(keys::TARGET) {
            Some(SettingValue::Text(text)) => text.as_str(),
            _ => return None,
        };
        let adapter = match source.settings.get(keys::ADAPTER) {
            Some(SettingValue::Text(text)) => Some(text.as_str()),
            _ => None,
        };
        let found = self
            .adapters
            .sources
            .targets(&source.type_id)
            .ok()?
            .into_iter()
            .find(|target| {
                target.target == target_id && adapter.is_none_or(|id| target.adapter.as_str() == id)
            })?;
        let work = found.work_rect?;
        let picture = self.sizes.get(&id).copied().or(found.size)?;
        Some((work, picture))
    }

    /// A new box for a source's picture; `Ok(false)` when it is already there.
    pub(super) fn set_source_box(
        &mut self,
        id: SourceId,
        position: [f32; 2],
        size: [f32; 2],
        gesture: u64,
    ) -> Result<bool, Refusal> {
        let valid = position.iter().chain(size.iter()).all(|v| v.is_finite())
            && size.iter().all(|v| *v != 0.0);
        if !valid {
            return Err(Refusal::Invalid(
                "Position and size must be numbers, and the size cannot be 0.".into(),
            ));
        }
        let (_, item) = self.item_of(id)?;
        // The layout the canvas shows: its own if the item has one, else the default.
        let mut layout = self.shown_layout(item);
        let own = item
            .overrides
            .contains_key(&self.canvas)
            .then_some(self.canvas);
        if layout.position == position && layout.size == size {
            return Ok(false);
        }
        layout.position = position;
        layout.size = size;
        let item = item.id;
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        let name = self.name_of(id);
        self.edits
            .history
            .record(EditKind::Place, &name, gesture, vec![undo]);
        self.crisp_after_box(item, gesture, &name);
        Ok(true)
    }

    /// A new style for an item's layout on the studio's canvas (its own layout there if
    /// it has one, else the default); `Ok(false)` when it already has it.
    pub(super) fn set_item_style(
        &mut self,
        item: ItemId,
        style: ItemStyle,
        gesture: u64,
        on: OutputCanvas,
    ) -> Result<bool, Refusal> {
        if !style.is_valid() {
            return Err(Refusal::Invalid(STYLE_ERROR.into()));
        }
        let Some((_, found)) = self.collection.item(item) else {
            return Err(Refusal::NotFound(
                "This source is not in the scene any more.".to_owned(),
            ));
        };
        let found = found.clone();
        let (mut layout, own) = match on {
            OutputCanvas::Main => (
                self.shown_layout(&found),
                found
                    .overrides
                    .contains_key(&self.canvas)
                    .then_some(self.canvas),
            ),
            OutputCanvas::Vertical => {
                let (_, layout, own) = self.crop_target_on(item, on)?;
                (layout, own)
            }
        };
        if layout.style == style {
            return Ok(false);
        }
        layout.style = style;
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        let name = match found.content {
            ItemContent::Source(id) => self.name_of(id),
            _ => String::new(),
        };
        self.edits
            .history
            .record(EditKind::Style, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// Sets or clears an item's saved scroll on `on` (ADR-0150): one undo step. False
    /// when it was already so.
    pub(super) fn set_item_scroll(
        &mut self,
        item: ItemId,
        scroll: Option<Scroll>,
        gesture: u64,
        on: OutputCanvas,
    ) -> Result<bool, Refusal> {
        let Some((_, found)) = self.collection.item(item) else {
            return Err(Refusal::NotFound(
                "This source is not in the scene any more.".to_owned(),
            ));
        };
        let found = found.clone();
        let (mut layout, own) = match on {
            OutputCanvas::Main => (
                self.shown_layout(&found),
                found
                    .overrides
                    .contains_key(&self.canvas)
                    .then_some(self.canvas),
            ),
            OutputCanvas::Vertical => {
                let (_, layout, own) = self.crop_target_on(item, on)?;
                (layout, own)
            }
        };
        if layout.scroll == scroll {
            return Ok(false);
        }
        layout.scroll = scroll;
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        let name = match found.content {
            ItemContent::Source(id) => self.name_of(id),
            _ => String::new(),
        };
        self.edits
            .history
            .record(EditKind::Scroll, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// A live call on an item's scroll run (ADR-0150 decision 11).
    pub(super) fn api_scroll(
        &mut self,
        item: u32,
        on: OutputCanvas,
        control: ScrollControl,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let canvas = self.edit_canvas(on).map_err(|e| refused(&e))?;
        let id = ItemId::from_raw(item);
        let Some((_, found)) = self.collection.item(id) else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no item {item}"),
            ));
        };
        if matches!(found.content, ItemContent::Scene(_)) {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "scroll is for source items, not nested scenes",
            ));
        }
        match self.studio.scroll_control(id, canvas, control) {
            Ok(_) => {
                // The views, as the window's Properties buttons always had (ADR-0163).
                self.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            Err(ScrollRefused::NoScroll) => Err(ApiError::new(
                ErrorKind::InvalidParams,
                "This source does not scroll on that canvas.",
            )),
            Err(_) => Err(ApiError::new(
                ErrorKind::Conflict,
                "This source is not on a scene a canvas shows, so its scroll is not running.",
            )),
        }
    }

    /// An item placing a source, anywhere in the collection, with its layout on the
    /// studio's canvas and whether that is its own override there.
    pub(super) fn crop_target(
        &self,
        item: ItemId,
    ) -> Result<(SourceId, Layout, Option<CanvasId>), CropRefusal> {
        self.crop_target_on(item, OutputCanvas::Main)
    }

    /// The canvas a window edit of `on` changes: the studio's canvas, or the vertical
    /// canvas while it shows the scene being edited (ADR-0093 decision 4).
    pub(super) fn edit_canvas(&self, on: OutputCanvas) -> Result<CanvasId, Refusal> {
        match on {
            OutputCanvas::Main => Ok(self.canvas),
            OutputCanvas::Vertical => {
                if self.verticals.vertical.is_none() {
                    return Err(Refusal::NotFound("There is no vertical canvas.".into()));
                }
                self.vertical_edits().ok_or_else(|| {
                    Refusal::Conflict(
                        "The vertical canvas shows another scene: its sources are edited while it shows the scene on air outside studio mode."
                            .into(),
                    )
                })
            }
        }
    }

    /// The width and height of canvas `id` in pixels.
    pub(super) fn canvas_px(&self, id: CanvasId) -> Option<(u32, u32)> {
        self.collection
            .canvas(id)
            .map(|c| (c.size.width, c.size.height))
    }

    /// As [`Self::crop_target`], on the canvas `on` names. On the vertical canvas the
    /// layout is always written as that canvas's own (U15); global sources are not on
    /// it yet.
    pub(super) fn crop_target_on(
        &self,
        item: ItemId,
        on: OutputCanvas,
    ) -> Result<(SourceId, Layout, Option<CanvasId>), CropRefusal> {
        if on == OutputCanvas::Vertical {
            let canvas = self.edit_canvas(on).map_err(|e| match e {
                Refusal::NotFound(t) => CropRefusal::NotFound(t),
                e => CropRefusal::Conflict(e.text().into()),
            })?;
            let (scene, found) = self
                .collection
                .item(item)
                .ok_or_else(|| CropRefusal::NotFound(format!("no item {}", item.get())))?;
            if self.globals() == Some(scene) {
                return Err(CropRefusal::Conflict(
                    "Global sources are not on the vertical canvas yet.".into(),
                ));
            }
            let ItemContent::Source(source) = found.content else {
                return Err(CropRefusal::Invalid(
                    "crop is for source items, not nested scenes".into(),
                ));
            };
            let layout = self
                .collection
                .placed_layout(canvas, found.layout_on(canvas));
            return Ok((source, layout, Some(canvas)));
        }
        let found = self
            .collection
            .scenes()
            .find_map(|s| s.items.iter().find(|i| i.id == item))
            .ok_or_else(|| CropRefusal::NotFound(format!("no item {}", item.get())))?;
        let ItemContent::Source(source) = found.content else {
            return Err(CropRefusal::Invalid(
                "crop is for source items, not nested scenes".into(),
            ));
        };
        let own = found
            .overrides
            .contains_key(&self.canvas)
            .then_some(self.canvas);
        Ok((source, self.shown_layout(found), own))
    }

    /// Writes `layout` as one undo step of `kind`; `Ok(false)` when it is unchanged.
    pub(super) fn write_crop(
        &mut self,
        item: ItemId,
        source: SourceId,
        own: Option<CanvasId>,
        old: Layout,
        layout: Layout,
        gesture: u64,
    ) -> Result<bool, CropRefusal> {
        if layout == old {
            return Ok(false);
        }
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| CropRefusal::Failed(edit_error(&e)))?;
        let name = self.name_of(source);
        self.edits
            .history
            .record(EditKind::Crop, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// A new crop for an item's layout on the studio's canvas (ADR-0058); `Ok(false)`
    /// when it already has it.
    pub(super) fn set_item_crop(
        &mut self,
        item: ItemId,
        crop: Crop,
        keep_scale: bool,
        gesture: u64,
        on: OutputCanvas,
    ) -> Result<bool, CropRefusal> {
        let (source, old, own) = self.crop_target_on(item, on)?;
        let size = self.sizes.get(&source).copied();
        let region = self.collection.source(source).and_then(|s| s.region);
        let layout = if keep_scale {
            let size = size.ok_or(CropRefusal::NoPicture)?;
            if crop == old.crop {
                return Ok(false);
            }
            recrop_keeping(old, size, region, crop)
                .ok_or_else(|| CropRefusal::Invalid(NO_AREA.into()))?
        } else {
            if size.is_some_and(|picture| visible_size(picture, region, crop).is_none()) {
                return Err(CropRefusal::Invalid(NO_AREA.into()));
            }
            old.with_crop(crop)
        };
        self.write_crop(item, source, own, old, layout, gesture)
    }

    /// A transform command from the control API: the edit, the rebuild when something
    /// changed and the answer.
    pub(super) fn api_transform(
        &mut self,
        item: u32,
        op: TransformOp,
        gesture: u64,
        canvas: OutputCanvas,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let changed = self
            .transform_item(ItemId::from_raw(item), op, gesture, canvas)
            .map_err(|e| refused(&e))?;
        if changed {
            self.apply(notify);
        }
        Ok(Response::Done(Empty {}))
    }

    /// Flips, turns, fits or centres an item (ADR-0084) on the layout the canvas shows,
    /// as one undo step; `Ok(false)` when nothing changes. A locked item and an
    /// operation that needs a picture size the source has not reported are refused.
    pub(super) fn transform_item(
        &mut self,
        item: ItemId,
        op: TransformOp,
        gesture: u64,
        on: OutputCanvas,
    ) -> Result<bool, Refusal> {
        let locked = self.collection.item(item).is_some_and(|(_, i)| i.locked);
        let target = self.edit_canvas(on)?;
        let (source, old, own) = self.crop_target_on(item, on).map_err(|e| match e {
            CropRefusal::NotFound(t) => Refusal::NotFound(t),
            CropRefusal::Invalid(t) => Refusal::Invalid(t),
            CropRefusal::Conflict(t) => Refusal::Conflict(t),
            CropRefusal::NoPicture | CropRefusal::Failed(_) => Refusal::Failed(e.text()),
        })?;
        if locked {
            return Err(Refusal::Conflict(
                "The item is locked. Unlock it to change its transform.".into(),
            ));
        }
        let canvas = self
            .canvas_px(target)
            .ok_or_else(|| Refusal::Failed("the canvas is gone".into()))?;
        let picture = || {
            let capture = self.sizes.get(&source).copied().ok_or_else(|| {
                Refusal::Invalid("This source has no picture yet, so its size is not known.".into())
            })?;
            let region = self.collection.source(source).and_then(|s| s.region);
            visible_size(capture, region, old.crop).ok_or_else(|| Refusal::Invalid(NO_AREA.into()))
        };
        let layout = match op {
            TransformOp::FlipHorizontal => old.flipped_horizontally(),
            TransformOp::FlipVertical => old.flipped_vertically(),
            TransformOp::Turn(degrees) => old.turned(degrees),
            TransformOp::SetAngle(degrees) => old.angled(degrees),
            TransformOp::Center(axis) => old.centred(canvas, axis),
            TransformOp::Fit(mode) => old
                .fitted(canvas, picture()?, mode)
                .ok_or_else(|| Refusal::Invalid("The picture or the canvas is empty.".into()))?,
            TransformOp::Reset => old
                .transform_reset(match self.stored_box_of(source) {
                    Some(size) => size,
                    None => self.reset_size_of(source, picture()?, old.size),
                })
                .ok_or_else(|| Refusal::Invalid("The picture is empty.".into()))?,
        };
        if layout == old {
            return Ok(false);
        }
        let undo = self
            .save_live_layout(item, own, layout)
            .map_err(|e| Refusal::of(&e))?;
        let name = self.name_of(source);
        self.edits
            .history
            .record(EditKind::Transform, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// The box a Library shape or plain frame was added with (see [`stored_box`]).
    fn stored_box_of(&self, source: SourceId) -> Option<(u32, u32)> {
        self.collection
            .source(source)
            .and_then(|s| stored_box(&s.settings))
    }

    /// The size Reset transform gives `source`'s item (see [`reset_size`]).
    pub(super) fn reset_size_of(
        &self,
        source: SourceId,
        shown: (u32, u32),
        current: [f32; 2],
    ) -> (u32, u32) {
        match self.collection.source(source) {
            Some(s) => reset_size(&s.settings, shown, current),
            None => shown,
        }
    }

    /// Shows `region` of an item's picture on canvas `on` (Properties' Show buttons).
    pub(super) fn set_shown_region(
        &mut self,
        item: ItemId,
        region: CropRegion,
        on: OutputCanvas,
    ) -> Result<bool, CropRefusal> {
        let (source, old, own) = self.crop_target_on(item, on)?;
        let size = self
            .sizes
            .get(&source)
            .copied()
            .ok_or(CropRefusal::NoPicture)?;
        let canvas = self
            .collection
            .canvas(own.unwrap_or(self.canvas))
            .map(|c| (c.size.width, c.size.height))
            .ok_or_else(|| CropRefusal::Failed("the canvas is gone".into()))?;
        let crop = region.crop(size, canvas);
        let layout = {
            if !crop.fits(size.0, size.1) {
                return Err(CropRefusal::Invalid(NO_AREA.into()));
            }
            let w = size.0 - crop.left - crop.right;
            let h = size.1 - crop.top - crop.bottom;
            let fit = Layout::fit(canvas.0, canvas.1, w, h);
            let mut layout = old.with_crop(crop);
            layout.position = fit.position;
            // A flipped picture stays flipped.
            layout.size = [
                fit.size[0].copysign(old.size[0]),
                fit.size[1].copysign(old.size[1]),
            ];
            if layout.size[0] < 0.0 {
                layout.position[0] -= layout.size[0];
            }
            if layout.size[1] < 0.0 {
                layout.position[1] -= layout.size[1];
            }
            layout
        };
        self.write_crop(item, source, own, old, layout, 0)
    }

    /// Remembers the picture size of every running video source (ADR-0058 decision 2)
    /// and forgets sources that are gone.
    pub(super) fn refresh_sizes(&mut self) {
        let collection = &self.collection;
        self.sizes.retain(|id, _| collection.source(*id).is_some());
        for source in self.collection.sources() {
            if let Some(size) = self.studio.picture_size(source.id) {
                self.sizes.insert(source.id, size);
            }
        }
    }
}
