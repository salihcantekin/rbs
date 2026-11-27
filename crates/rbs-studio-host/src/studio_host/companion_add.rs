//! Transactional creation of a picture and its companion sound.

use super::*;
use rbs_scene::model::Source;

impl Host {
    pub(super) fn companion_target(
        &self,
        source: &Source,
    ) -> Result<(SourceKind, Target, String, bool), Refusal> {
        match SourceKind::of(&source.type_id) {
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
                Ok((SourceKind::SystemAudio, target, "System sound".into(), true))
            }
            Some(SourceKind::Window) => {
                let app = source.hints.get(HINT_APP).cloned().ok_or_else(|| {
                    Refusal::Failed("This window's application is not known.".into())
                })?;
                let target = self
                    .targets(SourceKind::ApplicationAudio)
                    .into_iter()
                    .find(|t| super::targets::same_app(&t.name, &app))
                    .ok_or_else(|| {
                        Refusal::Failed(format!("{app} is not playing sound that can be captured."))
                    })?;
                Ok((
                    SourceKind::ApplicationAudio,
                    target,
                    format!("{} sound", source.name),
                    false,
                ))
            }
            Some(SourceKind::Camera) => {
                let target = self.camera_microphone(source).ok_or_else(|| {
                    Refusal::Failed("No microphone belongs with this camera.".into())
                })?;
                Ok((
                    SourceKind::AudioInput,
                    target,
                    format!("{} mic", source.name),
                    false,
                ))
            }
            _ => Err(Refusal::Invalid(
                "Only a Display, a Window or a Camera can capture audio too.".into(),
            )),
        }
    }

    /// Stage the entire edit on a collection copy; publish history and device effects
    /// only after both source additions and all validation have succeeded.
    pub(super) fn add_with_audio(
        &mut self,
        kind: SourceKind,
        target: &Target,
        global: bool,
    ) -> Result<SourceId, Refusal> {
        let (settings, layout) = self.new_source(kind, target)?;
        let mut collection = self.collection.clone();
        let mut records = Vec::new();
        let scene = if global {
            match collection.globals_of(self.canvas) {
                Some(scene) => scene,
                None => {
                    let name = unique_scene_name(&collection, "Global sources");
                    let (scene, undo) = collection
                        .add_scene(&name, SceneScope::Canvas(self.canvas))
                        .map_err(|e| Refusal::of(&e))?;
                    records.push(undo);
                    records.push(
                        collection
                            .set_globals(self.canvas, Some(scene))
                            .map_err(|e| Refusal::of(&e))?,
                    );
                    scene
                }
            }
        } else {
            self.edited()
        };
        let name = unique_name(&collection, &target.name);
        let (picture, undo) = collection
            .add_source(&name, kind.type_id(), settings.clone())
            .map_err(|e| Refusal::of(&e))?;
        records.push(undo);
        let (_, undo) = collection
            .add_item(
                scene,
                ItemContent::Source(picture),
                self.stored_layout(&layout),
                None,
            )
            .map_err(|e| Refusal::of(&e))?;
        records.push(undo);
        collection
            .set_source_settings_and_hints(picture, settings, hints_of(target))
            .map_err(|e| Refusal::of(&e))?;
        let source = collection
            .source(picture)
            .ok_or_else(|| Refusal::Failed("The new picture source is missing.".into()))?;
        let (sound_kind, sound_target, sound_name, exclude) = self.companion_target(source)?;
        let (mut sound_settings, sound_layout) = self.new_source(sound_kind, &sound_target)?;
        if exclude {
            sound_settings.insert(keys::EXCLUDE_OWN_PROCESS.into(), SettingValue::Bool(true));
        }
        let sound_name = unique_name(&collection, &sound_name);
        let (sound, _, undo) = collection
            .add_companion(
                picture,
                &sound_name,
                sound_kind.type_id(),
                sound_settings,
                hints_of(&sound_target),
                self.stored_layout(&sound_layout),
            )
            .map_err(|e| Refusal::of(&e))?;
        records.push(undo);
        self.collection = collection;
        self.edits.history.record(EditKind::Add, &name, 0, records);
        if !global {
            self.remember_target(kind, &target.adapter, &target.target, &target.name);
        }
        self.fill_device_offset(picture, None, &name, EditKind::Add);
        self.fill_device_offset(sound, None, &sound_name, EditKind::Add);
        Ok(picture)
    }
}
