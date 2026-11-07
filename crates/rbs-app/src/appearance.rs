//! Settings › Appearance (ADR-0057, themes spec): the theme choice, the theme list and
//! the theme editor. Toolkit-free state and rules; `main.rs` shows it in the
//! `Appearance` global and applies the look it asks for.

use std::time::{Duration, Instant};

use rbs_ui::tokens::{
    Base, Color, FONT_FAMILY, Group, Kind, ResolvedTheme, THEMEABLE, ThemeBody, ThemeProblem,
    Tokens as TokenFile, TypeStyle,
};
use rbs_user_data::{Appearance, AppearanceMode, EntryId, ThemeId, UserData};

use crate::look;

/// How long "Keep this look?" waits before the previous look comes back.
pub const KEEP_LOOK_TIMEOUT: Duration = Duration::from_secs(15);

/// The editor groups, in tab order.
pub const GROUPS: [Group; 6] = [
    Group::Colours,
    Group::Text,
    Group::Font,
    Group::SpacingAndSize,
    Group::Shapes,
    Group::Motion,
];

/// A theme in the list: a built-in one or a user theme.
#[derive(Debug, Clone)]
pub struct ThemeEntry {
    /// Its id (`builtin:<base>` or a library id).
    pub id: ThemeId,
    /// Its name.
    pub name: String,
    /// The user theme's body, when it loads.
    pub body: Option<ThemeBody>,
    /// The resolved theme, or why it cannot be used.
    pub resolved: Result<ResolvedTheme, String>,
}

/// A user theme being edited.
#[derive(Debug, Clone)]
pub struct Editor {
    /// Its id.
    pub id: EntryId,
    /// Its name.
    pub name: String,
    /// As saved.
    pub saved: ThemeBody,
    /// With the edits.
    pub draft: ThemeBody,
    /// The draft's problems (empty: it can be saved and is applied).
    pub problems: Vec<ThemeProblem>,
}

/// What "Keep this look?" goes back to.
#[derive(Debug, Clone, PartialEq)]
pub enum Previous {
    /// The appearance choice before a change.
    Choice(Appearance),
    /// The edited theme's font families before a change.
    Fonts(Option<Vec<String>>),
}

/// An open "Keep this look?" question.
#[derive(Debug, Clone)]
pub struct KeepLook {
    /// What comes back on Undo or after the timeout.
    pub previous: Previous,
    /// When it was asked.
    pub since: Instant,
}

impl KeepLook {
    /// True once [`KEEP_LOOK_TIMEOUT`] has passed at `now`.
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) >= KEEP_LOOK_TIMEOUT
    }
}

/// The Appearance page's state.
#[derive(Debug, Default)]
pub struct AppearanceUi {
    /// The appearance choice as saved.
    pub choice: Appearance,
    /// Built-in themes first, then user themes by name.
    pub entries: Vec<ThemeEntry>,
    /// The theme selected in the list.
    pub selected: Option<usize>,
    /// The theme being edited.
    pub editor: Option<Editor>,
    /// The editor tab, an index into [`GROUPS`].
    pub group: usize,
    /// An open "Keep this look?" question.
    pub keep: Option<KeepLook>,
}

fn base_label(base: Base) -> &'static str {
    match base {
        Base::Dark => "Dark",
        Base::Light => "Light",
        Base::HighContrast => "High contrast",
    }
}

impl AppearanceUi {
    /// Reads the choice and the theme list again (when the page opens, and after a
    /// change to the library).
    pub fn refresh(&mut self, data: &UserData, file: &TokenFile) {
        self.choice = data.appearance().0;
        let mut entries: Vec<ThemeEntry> = Base::ALL
            .into_iter()
            .map(|b| ThemeEntry {
                id: ThemeId::BuiltIn(b),
                name: base_label(b).to_owned(),
                body: None,
                resolved: Ok(file.built_in_theme(b)),
            })
            .collect();
        let themes = data.themes();
        if let Ok(listing) = themes.list() {
            for e in listing.entries {
                let (body, resolved) = match themes.load(&e.id) {
                    Ok(entry) => {
                        let r = file.resolve(&entry.document).map_err(|p| p.to_string());
                        (Some(entry.document), r)
                    }
                    Err(err) => (None, Err(err.to_string())),
                };
                entries.push(ThemeEntry {
                    id: ThemeId::User(e.id),
                    name: e.name,
                    body,
                    resolved,
                });
            }
        }
        let keep_selected = self
            .selected
            .and_then(|i| self.entries.get(i))
            .map(|e| e.id.clone());
        self.entries = entries;
        self.selected = keep_selected.and_then(|id| self.index_of(&id));
        if let Some(ed) = &self.editor
            && self.index_of(&ThemeId::User(ed.id.clone())).is_none()
        {
            self.editor = None;
        }
    }

    /// Checks the editor's draft again, against `file` (after the token file was
    /// read again).
    pub fn recheck(&mut self, file: &TokenFile) {
        if let Some(ed) = &mut self.editor {
            ed.problems = problems(file, &ed.draft);
        }
    }

    /// The list position of `id`.
    pub fn index_of(&self, id: &ThemeId) -> Option<usize> {
        self.entries.iter().position(|e| &e.id == id)
    }

    /// The themes of the two slots (OS dark, OS light), with the editor's draft in
    /// place of its saved theme while the draft passes every rule.
    pub fn slots(
        &self,
        data: &UserData,
        file: &TokenFile,
        notes: &mut Vec<String>,
    ) -> [ResolvedTheme; 2] {
        let [dark, light] = self.choice.slots();
        let one = |id: &ThemeId, fallback: Base, notes: &mut Vec<String>| {
            if let (Some(ed), ThemeId::User(uid)) = (&self.editor, id)
                && *uid == ed.id
                && ed.problems.is_empty()
                && let Ok(r) = file.resolve(&ed.draft)
            {
                return r;
            }
            look::resolve(file, data, id, fallback, notes)
        };
        [one(dark, Base::Dark, notes), one(light, Base::Light, notes)]
    }

    /// Picks the theme at `index` for a slot: 0 dark, 1 light, 2 the fixed theme.
    /// Returns the new choice to save; asks "Keep this look?".
    pub fn pick(&mut self, slot: usize, index: usize, now: Instant) -> Option<Appearance> {
        let id = self.entries.get(index)?.id.clone();
        if self.entries[index].resolved.is_err() {
            return None;
        }
        let mut next = self.choice.clone();
        match slot {
            0 => next.dark = id,
            1 => next.light = id,
            _ => next.theme = id,
        }
        self.change_choice(next, now)
    }

    /// Follow the system or not.
    pub fn set_follow_system(&mut self, follow: bool, now: Instant) -> Option<Appearance> {
        let mut next = self.choice.clone();
        next.mode = if follow {
            AppearanceMode::System
        } else {
            AppearanceMode::Fixed
        };
        self.change_choice(next, now)
    }

    fn change_choice(&mut self, next: Appearance, now: Instant) -> Option<Appearance> {
        if next == self.choice {
            return None;
        }
        // Keep the first look of a series of changes as the one to go back to.
        let previous = match self.keep.take() {
            Some(
                k @ KeepLook {
                    previous: Previous::Choice(_),
                    ..
                },
            ) => k.previous,
            _ => Previous::Choice(self.choice.clone()),
        };
        self.keep = Some(KeepLook {
            previous,
            since: now,
        });
        self.choice = next.clone();
        Some(next)
    }

    /// Keep: the question closes.
    pub fn keep(&mut self) {
        self.keep = None;
    }

    /// Undo, or the timeout: the previous look comes back. Returns the choice to save
    /// when the choice changed back.
    pub fn undo(&mut self, file: &TokenFile) -> Option<Appearance> {
        match self.keep.take()?.previous {
            Previous::Choice(a) => {
                self.choice = a.clone();
                Some(a)
            }
            Previous::Fonts(f) => {
                if let Some(ed) = &mut self.editor {
                    ed.draft.font_family = f;
                    ed.problems = problems(file, &ed.draft);
                }
                None
            }
        }
    }

    /// Opens the editor on the selected user theme.
    pub fn edit(&mut self, file: &TokenFile) -> bool {
        let Some(e) = self.selected.and_then(|i| self.entries.get(i)) else {
            return false;
        };
        let (ThemeId::User(id), Some(body)) = (&e.id, &e.body) else {
            return false;
        };
        self.editor = Some(Editor {
            id: id.clone(),
            name: e.name.clone(),
            saved: body.clone(),
            draft: body.clone(),
            problems: problems(file, body),
        });
        true
    }

    /// The body a duplicate of the selected theme gets, and its name.
    pub fn duplicate_of_selected(&self) -> Option<(String, ThemeBody)> {
        let e = self.entries.get(self.selected?)?;
        let body = match (&e.id, &e.body) {
            (ThemeId::BuiltIn(b), _) => ThemeBody::new(*b),
            (_, Some(body)) => body.clone(),
            _ => return None,
        };
        let taken = |n: &str| self.entries.iter().any(|e| e.name.eq_ignore_ascii_case(n));
        let mut name = format!("{} copy", e.name);
        let mut i = 2;
        while taken(&name) {
            name = format!("{} copy {i}", e.name);
            i += 1;
        }
        Some((name, body))
    }

    /// Sets a colour override from `#rrggbb` text. True when the draft changed.
    pub fn set_color(&mut self, file: &TokenFile, name: &str, hex: &str) -> bool {
        let Some(c) = Color::parse(hex).map(|c| Color { a: 255, ..c }) else {
            return false;
        };
        self.change(file, |d| {
            d.colors.insert(name.to_owned(), c);
        })
    }

    /// Sets a length or duration override.
    pub fn set_number(&mut self, file: &TokenFile, name: &str, value: f32) -> bool {
        let Some(t) = rbs_ui::tokens::themeable(name) else {
            return false;
        };
        match t.kind {
            Kind::Length { .. } => self.change(file, |d| {
                d.length.insert(name.to_owned(), value);
            }),
            Kind::Duration { .. } => self.change(file, |d| {
                d.duration
                    .insert(name.to_owned(), value.max(0.0).round() as u32);
            }),
            _ => false,
        }
    }

    /// Sets one part (0 size, 1 line height, 2 weight) of a text style override.
    pub fn set_style(&mut self, file: &TokenFile, name: &str, part: usize, value: f32) -> bool {
        let Some(current) = self.value_style(file, name) else {
            return false;
        };
        let v = value.clamp(0.0, f32::from(u16::MAX)).round() as u16;
        let next = match part {
            0 => TypeStyle { size: v, ..current },
            1 => TypeStyle { line: v, ..current },
            _ => TypeStyle {
                weight: v,
                ..current
            },
        };
        self.change(file, |d| {
            d.type_styles.insert(name.to_owned(), next);
        })
    }

    /// Sets the font families from comma-separated text; asks "Keep this look?".
    pub fn set_fonts(&mut self, file: &TokenFile, text: &str, now: Instant) -> bool {
        let list: Vec<String> = text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let Some(ed) = &self.editor else {
            return false;
        };
        let before = ed.draft.font_family.clone();
        let changed = self.change(file, |d| d.font_family = Some(list));
        if changed
            && !matches!(
                self.keep,
                Some(KeepLook {
                    previous: Previous::Fonts(_),
                    ..
                })
            )
        {
            self.keep = Some(KeepLook {
                previous: Previous::Fonts(before),
                since: now,
            });
        }
        changed
    }

    /// Removes an override (the token takes the base's value again).
    pub fn reset(&mut self, file: &TokenFile, name: &str) -> bool {
        self.change(file, |d| {
            d.colors.remove(name);
            d.length.remove(name);
            d.duration.remove(name);
            d.type_styles.remove(name);
            d.invalid.remove(name);
            if name == FONT_FAMILY {
                d.font_family = None;
            }
        })
    }

    fn change(&mut self, file: &TokenFile, f: impl FnOnce(&mut ThemeBody)) -> bool {
        let Some(ed) = &mut self.editor else {
            return false;
        };
        let before = ed.draft.clone();
        f(&mut ed.draft);
        if ed.draft == before {
            return false;
        }
        ed.problems = problems(file, &ed.draft);
        true
    }

    /// Cancel: the editor closes and the saved theme is used again.
    pub fn cancel(&mut self) {
        self.editor = None;
        if matches!(
            self.keep,
            Some(KeepLook {
                previous: Previous::Fonts(_),
                ..
            })
        ) {
            self.keep = None;
        }
    }

    /// The draft to save, if it passes every rule and differs from the saved theme.
    pub fn to_save(&self) -> Option<(EntryId, String, ThemeBody)> {
        let ed = self.editor.as_ref()?;
        (ed.problems.is_empty() && ed.draft != ed.saved)
            .then(|| (ed.id.clone(), ed.name.clone(), ed.draft.clone()))
    }

    /// After a save: the draft is the saved theme.
    pub fn saved(&mut self) {
        if let Some(ed) = &mut self.editor {
            ed.saved = ed.draft.clone();
        }
        if matches!(
            self.keep,
            Some(KeepLook {
                previous: Previous::Fonts(_),
                ..
            })
        ) {
            self.keep = None;
        }
    }

    fn value_style(&self, file: &TokenFile, name: &str) -> Option<TypeStyle> {
        let ed = self.editor.as_ref()?;
        ed.draft
            .type_styles
            .get(name)
            .copied()
            .or_else(|| file.type_styles.get(name).copied())
    }

    /// The editor's rows for the current group.
    pub fn rows(&self, file: &TokenFile) -> Vec<TokenRow> {
        let Some(ed) = &self.editor else {
            return Vec::new();
        };
        let base = file.built_in_theme(ed.draft.base);
        let group = GROUPS[self.group.min(GROUPS.len() - 1)];
        THEMEABLE
            .iter()
            .filter(|t| t.group == group)
            .map(|t| {
                let problem = ed
                    .problems
                    .iter()
                    .filter(|p| p.token.as_deref() == Some(t.name))
                    .map(|p| p.reason.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                let d = &ed.draft;
                let (value, overridden) = match t.kind {
                    Kind::Color => {
                        let c = d.colors.get(t.name).or(base.colors.get(t.name)).copied();
                        (
                            RowValue::Color(c.unwrap_or(Color {
                                r: 0,
                                g: 0,
                                b: 0,
                                a: 255,
                            })),
                            d.colors.contains_key(t.name),
                        )
                    }
                    Kind::Length { .. } => (
                        RowValue::Number(
                            d.length
                                .get(t.name)
                                .or(base.length.get(t.name))
                                .copied()
                                .unwrap_or(0.0),
                        ),
                        d.length.contains_key(t.name),
                    ),
                    Kind::Duration { .. } => (
                        RowValue::Number(
                            d.duration
                                .get(t.name)
                                .or(base.duration.get(t.name))
                                .map_or(0.0, |v| *v as f32),
                        ),
                        d.duration.contains_key(t.name),
                    ),
                    Kind::TypeStyle => (
                        RowValue::Style(
                            d.type_styles
                                .get(t.name)
                                .or(base.type_styles.get(t.name))
                                .copied()
                                .unwrap_or(TypeStyle {
                                    size: 13,
                                    line: 20,
                                    weight: 400,
                                }),
                        ),
                        d.type_styles.contains_key(t.name),
                    ),
                    Kind::FontFamily => (
                        RowValue::Fonts(
                            d.font_family
                                .clone()
                                .unwrap_or_else(|| base.font_family.clone())
                                .join(", "),
                        ),
                        d.font_family.is_some(),
                    ),
                };
                TokenRow {
                    name: t.name,
                    label: t.label,
                    value,
                    overridden: overridden || d.invalid.contains_key(t.name),
                    problem,
                }
            })
            .collect()
    }

    /// Problems of the draft that belong to no row of any group.
    pub fn general_problems(&self) -> Vec<String> {
        self.editor
            .as_ref()
            .map(|ed| {
                ed.problems
                    .iter()
                    .filter(|p| {
                        p.token
                            .as_deref()
                            .is_none_or(|t| rbs_ui::tokens::themeable(t).is_none())
                    })
                    .map(|p| p.reason.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn problems(file: &TokenFile, body: &ThemeBody) -> Vec<ThemeProblem> {
    match file.resolve(body) {
        Ok(_) => Vec::new(),
        Err(p) => p.0,
    }
}

/// A token's value in the editor.
#[derive(Debug, Clone, PartialEq)]
pub enum RowValue {
    /// A colour.
    Color(Color),
    /// Logical pixels or milliseconds.
    Number(f32),
    /// A text style.
    Style(TypeStyle),
    /// Font families, comma-separated.
    Fonts(String),
}

/// One editor row.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenRow {
    /// Token name.
    pub name: &'static str,
    /// Its label.
    pub label: &'static str,
    /// The value in use (the override, else the base's).
    pub value: RowValue,
    /// True when the theme sets it.
    pub overridden: bool,
    /// Its problems, joined; empty when none.
    pub problem: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use std::path::PathBuf;

    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> TempDir {
            let d = std::env::temp_dir()
                .join(format!("rbs-app-appearance-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup(name: &str) -> (TempDir, UserData, TokenFile, AppearanceUi, EntryId) {
        let d = TempDir::new(name);
        let data = UserData::new(&d.0);
        let file = TokenFile::built_in().unwrap();
        let id = {
            let lock = data.try_lock().unwrap().unwrap();
            let mut body = ThemeBody::new(Base::Dark);
            body.length.insert("radius.md".into(), 8.0);
            data.themes().create(&lock, "Midnight", &body).unwrap()
        };
        let mut ui = AppearanceUi::default();
        ui.refresh(&data, &file);
        (d, data, file, ui, id)
    }

    #[test]
    fn the_list_has_built_in_themes_first_then_user_themes() {
        let (_d, _data, _file, ui, id) = setup("list");
        let names: Vec<_> = ui.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Dark", "Light", "High contrast", "Midnight"]);
        assert_eq!(ui.entries[3].id, ThemeId::User(id));
        assert!(ui.entries.iter().all(|e| e.resolved.is_ok()));
    }

    #[test]
    fn picking_asks_to_keep_and_undo_or_timeout_goes_back() {
        let (_d, _data, file, mut ui, id) = setup("keep");
        let t0 = Instant::now();
        let before = ui.choice.clone();
        let next = ui.pick(0, 3, t0).unwrap();
        assert_eq!(next.dark, ThemeId::User(id));
        let k = ui.keep.as_ref().unwrap();
        assert!(!k.expired(t0 + Duration::from_secs(14)));
        assert!(k.expired(t0 + KEEP_LOOK_TIMEOUT));
        // A second change keeps the first look as the one to go back to.
        ui.pick(1, 2, t0).unwrap();
        assert_eq!(ui.undo(&file), Some(before.clone()));
        assert_eq!(ui.choice, before);
        assert!(ui.keep.is_none());
        // Keep closes the question.
        ui.set_follow_system(false, t0).unwrap();
        ui.keep();
        assert!(ui.keep.is_none());
        assert_eq!(ui.choice.mode, AppearanceMode::Fixed);
        // Picking the same theme again changes nothing.
        assert!(ui.pick(2, 0, t0).is_none());
    }

    #[test]
    fn editing_applies_passing_drafts_and_holds_failing_ones() {
        let (_d, data, file, mut ui, id) = setup("edit");
        ui.selected = ui.index_of(&ThemeId::User(id.clone()));
        assert!(ui.edit(&file));
        ui.choice = Appearance::new(
            AppearanceMode::Fixed,
            ThemeId::BuiltIn(Base::Dark),
            ThemeId::BuiltIn(Base::Light),
            ThemeId::User(id),
        );
        assert!(ui.set_color(&file, "color.surface.base", "#0b1020"));
        let mut notes = Vec::new();
        let slots = ui.slots(&data, &file, &mut notes);
        assert_eq!(
            slots[0].colors["color.surface.base"],
            Color::parse("#0b1020").unwrap()
        );
        assert!(ui.to_save().is_some());

        // A failing edit stays in the editor with its problem and is not applied.
        assert!(ui.set_color(&file, "color.accent", "#c4262e"));
        ui.group = 0;
        let row = ui
            .rows(&file)
            .into_iter()
            .find(|r| r.name == "color.accent")
            .unwrap();
        assert!(row.overridden && row.problem.contains("live"), "{row:?}");
        let slots = ui.slots(&data, &file, &mut notes);
        assert_eq!(slots[0].length["radius.md"], 8.0, "saved theme still used");
        assert_ne!(
            slots[0].colors["color.accent"],
            Color::parse("#c4262e").unwrap()
        );
        assert!(ui.to_save().is_none());

        // Reset removes the override.
        assert!(ui.reset(&file, "color.accent"));
        assert!(ui.to_save().is_some());
        ui.cancel();
        assert!(ui.editor.is_none());
    }

    #[test]
    fn reloading_lists_new_files_and_checks_the_draft_again() {
        let (_d, data, file, mut ui, id) = setup("reload");
        ui.selected = ui.index_of(&ThemeId::User(id));
        assert!(ui.edit(&file));
        assert!(ui.set_color(&file, "color.accent", "#c4262e"));
        // A theme file added on disk shows after `ui.theme.reload` reads the files again.
        {
            let lock = data.try_lock().unwrap().unwrap();
            data.themes()
                .create(&lock, "Added", &ThemeBody::new(Base::Light))
                .unwrap();
        }
        ui.editor.as_mut().unwrap().problems.clear();
        ui.refresh(&data, &file);
        ui.recheck(&file);
        assert!(ui.entries.iter().any(|e| e.name == "Added"));
        let ed = ui.editor.as_ref().expect("the draft stays open");
        assert!(
            !ed.problems.is_empty(),
            "the draft is checked against the file again"
        );
    }

    #[test]
    fn a_font_change_asks_to_keep_and_undo_restores_it() {
        let (_d, _data, file, mut ui, id) = setup("fonts");
        ui.selected = ui.index_of(&ThemeId::User(id));
        ui.edit(&file);
        let t0 = Instant::now();
        assert!(ui.set_fonts(&file, "Wingdings, ", t0));
        assert_eq!(
            ui.editor.as_ref().unwrap().draft.font_family,
            Some(vec!["Wingdings".to_owned()])
        );
        assert!(ui.keep.as_ref().unwrap().expired(t0 + KEEP_LOOK_TIMEOUT));
        assert!(ui.undo(&file).is_none());
        assert_eq!(ui.editor.as_ref().unwrap().draft.font_family, None);
    }

    #[test]
    fn duplicates_get_a_free_name_and_the_right_body() {
        let (_d, _data, _file, mut ui, _id) = setup("dup");
        ui.selected = Some(2);
        let (name, body) = ui.duplicate_of_selected().unwrap();
        assert_eq!(name, "High contrast copy");
        assert_eq!(body, ThemeBody::new(Base::HighContrast));
        ui.selected = Some(3);
        let (name, body) = ui.duplicate_of_selected().unwrap();
        assert_eq!(name, "Midnight copy");
        assert_eq!(body.length["radius.md"], 8.0);
    }

    #[test]
    fn rows_show_each_group() {
        let (_d, _data, file, mut ui, id) = setup("rows");
        ui.selected = ui.index_of(&ThemeId::User(id));
        ui.edit(&file);
        for (g, kind) in GROUPS.iter().enumerate() {
            ui.group = g;
            let rows = ui.rows(&file);
            assert!(!rows.is_empty(), "{kind:?}");
        }
        ui.group = 4;
        let radius = ui
            .rows(&file)
            .into_iter()
            .find(|r| r.name == "radius.md")
            .unwrap();
        assert_eq!(radius.value, RowValue::Number(8.0));
        assert!(radius.overridden);
        // Text styles change one part at a time.
        ui.group = 1;
        assert!(ui.set_style(&file, "type.body", 2, 500.0));
        assert_eq!(
            ui.editor.as_ref().unwrap().draft.type_styles["type.body"],
            TypeStyle {
                size: 13,
                line: 20,
                weight: 500
            }
        );
    }
}
