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
use crate::theme_assist::{self, Assist, Colors, Issue, MAIN};

/// How long "Keep this look?" waits before the previous look comes back.
pub const KEEP_LOOK_TIMEOUT: Duration = Duration::from_secs(15);

/// An editor group: the three main colours, or the tokens of one token group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorGroup {
    /// Background, text and accent; every other colour is worked out from them.
    Main,
    /// Every token of a group (Colours: every colour, one by one).
    Tokens(Group),
}

/// The editor groups, in tab order. The editor opens on the main colours.
pub const GROUPS: [EditorGroup; 7] = [
    EditorGroup::Main,
    EditorGroup::Tokens(Group::Colours),
    EditorGroup::Tokens(Group::Text),
    EditorGroup::Tokens(Group::Font),
    EditorGroup::Tokens(Group::SpacingAndSize),
    EditorGroup::Tokens(Group::Shapes),
    EditorGroup::Tokens(Group::Motion),
];

/// The choice with every slot that names a user theme `gone` replaced by the
/// built-in theme `base_of` gives for the slot (0 dark, 1 light, 2 the one theme);
/// `None` when no slot names one.
pub fn forget_themes(
    choice: &Appearance,
    gone: impl Fn(&EntryId) -> bool,
    base_of: impl Fn(usize) -> Base,
) -> Option<Appearance> {
    let mut next = choice.clone();
    for (slot, id) in [&mut next.dark, &mut next.light, &mut next.theme]
        .into_iter()
        .enumerate()
    {
        if let ThemeId::User(e) = id
            && gone(e)
        {
            *id = ThemeId::BuiltIn(base_of(slot));
        }
    }
    (next != *choice).then_some(next)
}

/// At start: slots that name a user theme whose file is gone (deleted, or removed by
/// hand) take the built-in Dark (dark and the one theme) or Light theme, so a
/// deleted theme is never reported as unreadable (themes spec, "Deleting").
pub fn forget_missing(data: &UserData) -> Option<Appearance> {
    let (choice, err) = data.appearance();
    if err.is_some() {
        return None;
    }
    let themes = data.themes();
    forget_themes(
        &choice,
        |id| !themes.path(id).exists(),
        |slot| if slot == 1 { Base::Light } else { Base::Dark },
    )
}

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
    /// What the windows show: the draft without the edits that fail a rule (themes
    /// spec, "a failing edit is shown with its problem and not applied"). A failing
    /// edit holds back only itself, so later passing edits still show at once.
    pub shown: ThemeBody,
}

impl Editor {
    fn new(id: EntryId, name: String, body: ThemeBody, file: &TokenFile) -> Self {
        let mut ed = Editor {
            id,
            name,
            saved: body.clone(),
            draft: body.clone(),
            problems: Vec::new(),
            shown: body,
        };
        ed.check(file);
        ed
    }

    /// Checks the draft and moves every edit that passes into [`Editor::shown`].
    fn check(&mut self, file: &TokenFile) {
        self.problems = problems(file, &self.draft);
        if self.problems.is_empty() {
            self.shown = self.draft.clone();
            return;
        }
        // One differing token at a time, again until nothing more passes: an edit
        // that fails alone (text on a new background) passes once the user fixes
        // the edit it depends on. A few hundred tokens at most, each check well
        // under a millisecond (spec, performance); user-paced only.
        loop {
            let mut moved = false;
            for step in differences(&self.shown, &self.draft) {
                let mut next = self.shown.clone();
                step.apply(&mut next, &self.draft);
                if file.resolve(&next).is_ok() {
                    self.shown = next;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }
}

/// One token whose value differs between two theme bodies.
enum Difference {
    Color(String),
    Style(String),
    Length(String),
    Duration(String),
    Fonts,
}

impl Difference {
    /// Gives `to` the value `from` has for this token (or removes it).
    fn apply(&self, to: &mut ThemeBody, from: &ThemeBody) {
        fn copy<V: Clone>(
            to: &mut std::collections::BTreeMap<String, V>,
            from: &std::collections::BTreeMap<String, V>,
            k: &str,
        ) {
            match from.get(k) {
                Some(v) => {
                    to.insert(k.to_owned(), v.clone());
                }
                None => {
                    to.remove(k);
                }
            }
        }
        match self {
            Difference::Color(k) => copy(&mut to.colors, &from.colors, k),
            Difference::Style(k) => copy(&mut to.type_styles, &from.type_styles, k),
            Difference::Length(k) => copy(&mut to.length, &from.length, k),
            Difference::Duration(k) => copy(&mut to.duration, &from.duration, k),
            Difference::Fonts => to.font_family = from.font_family.clone(),
        }
    }
}

/// The tokens whose values differ between `a` and `b` (overrides only; invalid and
/// unknown entries never resolve and are not moved).
fn differences(a: &ThemeBody, b: &ThemeBody) -> Vec<Difference> {
    fn keys<V: PartialEq>(
        a: &std::collections::BTreeMap<String, V>,
        b: &std::collections::BTreeMap<String, V>,
        out: &mut Vec<Difference>,
        make: fn(String) -> Difference,
    ) {
        let mut names: Vec<&String> = a.keys().chain(b.keys()).collect();
        names.sort();
        names.dedup();
        for k in names {
            if a.get(k) != b.get(k) {
                out.push(make(k.clone()));
            }
        }
    }
    let mut out = Vec::new();
    keys(&a.colors, &b.colors, &mut out, Difference::Color);
    keys(&a.type_styles, &b.type_styles, &mut out, Difference::Style);
    keys(&a.length, &b.length, &mut out, Difference::Length);
    keys(&a.duration, &b.duration, &mut out, Difference::Duration);
    if a.font_family != b.font_family {
        out.push(Difference::Fonts);
    }
    out
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
            ed.check(file);
        }
    }

    /// The list position of `id`.
    pub fn index_of(&self, id: &ThemeId) -> Option<usize> {
        self.entries.iter().position(|e| &e.id == id)
    }

    /// The themes of the two slots (OS dark, OS light), with the editor's passing
    /// edits ([`Editor::shown`]) in place of its saved theme.
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
                && let Ok(r) = file.resolve(&ed.shown)
            {
                return r;
            }
            look::resolve(file, data, id, fallback, notes)
        };
        [one(dark, Base::Dark, notes), one(light, Base::Light, notes)]
    }

    /// Whether a slot of the choice uses `id` (dark and light while following the
    /// system, the one theme otherwise).
    pub fn in_use(&self, id: &ThemeId) -> bool {
        self.choice.slots().contains(&id)
    }

    /// After `id` was deleted: the slots that named it take the built-in theme of
    /// their base. Returns the choice to save, if it changed (no "Keep this look?":
    /// the theme is gone).
    pub fn forget(&mut self, id: &EntryId, base: Base) -> Option<Appearance> {
        let next = forget_themes(&self.choice, |e| e == id, |_| base)?;
        self.choice = next.clone();
        Some(next)
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
                    ed.check(file);
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
        self.editor = Some(Editor::new(id.clone(), e.name.clone(), body.clone(), file));
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
        let in_use = self.in_use(&ThemeId::User(ed.id.clone()));
        let changed = self.change(file, |d| d.font_family = Some(list));
        // Only a theme in use changes the look, so only then is "Keep this look?" asked.
        if changed
            && in_use
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
        ed.check(file);
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
        let colors = colors_of(file, &ed.draft);
        let issues = assist(file, &ed.draft).issues(&colors);
        let shown: Vec<&rbs_ui::tokens::Themeable> = match group {
            EditorGroup::Main => MAIN
                .iter()
                .filter_map(|n| rbs_ui::tokens::themeable(n))
                .collect(),
            EditorGroup::Tokens(g) => THEMEABLE.iter().filter(|t| t.group == g).collect(),
        };
        shown
            .into_iter()
            .map(|t| {
                let (problem, fixable) = row_problem(ed, &issues, t.name);
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
                    label: match group {
                        EditorGroup::Main => theme_assist::main_label(t.name).unwrap_or(t.label),
                        EditorGroup::Tokens(_) => t.label,
                    },
                    value,
                    overridden: overridden || d.invalid.contains_key(t.name),
                    problem,
                    fixable,
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
                    .map(|p| plain(&p.reason))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// True when the editor shows the main colours.
    pub fn on_main(&self) -> bool {
        GROUPS[self.group.min(GROUPS.len() - 1)] == EditorGroup::Main
    }

    /// Sets a main colour and works out every other colour from the three (themes
    /// spec, "Main colours"). True when the draft changed.
    pub fn set_main_color(&mut self, file: &TokenFile, name: &str, hex: &str) -> bool {
        let Some(c) = Color::parse(hex).map(|c| Color { a: 255, ..c }) else {
            return false;
        };
        let Some(i) = MAIN.iter().position(|m| *m == name) else {
            return false;
        };
        let Some(ed) = &self.editor else {
            return false;
        };
        let now = colors_of(file, &ed.draft);
        let mut main = MAIN.map(|m| now.get(m).copied().unwrap_or(c));
        main[i] = c;
        let base = file.built_in_theme(ed.draft.base).colors;
        let derived = assist(file, &ed.draft).derive(&base, main, name);
        self.change(file, |d| set_colors(d, &base, &derived))
    }

    /// "Fix": moves colour `name` just far enough in lightness to pass every rule it
    /// takes part in. True when the draft changed.
    pub fn fix_color(&mut self, file: &TokenFile, name: &str) -> bool {
        let Some(ed) = &self.editor else {
            return false;
        };
        let colors = colors_of(file, &ed.draft);
        let Some(c) = assist(file, &ed.draft).fix(&colors, name) else {
            return false;
        };
        self.change(file, |d| {
            d.colors.insert(name.to_owned(), c);
        })
    }

    /// "Fix colours": fixes every colour problem it can, moving the text, borders and
    /// marks before the surfaces. True when the draft changed.
    pub fn fix_all_colors(&mut self, file: &TokenFile) -> bool {
        let Some(ed) = &self.editor else {
            return false;
        };
        let mut colors = colors_of(file, &ed.draft);
        let changed = assist(file, &ed.draft).fix_all(&mut colors, &[]);
        if changed.is_empty() {
            return false;
        }
        self.change(file, |d| {
            for n in changed {
                if let Some(c) = colors.get(&n) {
                    d.colors.insert(n, *c);
                }
            }
        })
    }

    /// The summary over the editor while the draft has problems: what to change before
    /// Save, by group (empty when there is nothing), and whether "Fix colours" can help.
    pub fn summary(&self, file: &TokenFile) -> (String, bool) {
        let Some(ed) = &self.editor else {
            return (String::new(), false);
        };
        if ed.problems.is_empty() {
            return (String::new(), false);
        }
        let colors = colors_of(file, &ed.draft);
        let issues = assist(file, &ed.draft).issues(&colors);
        let mut names: Vec<&'static str> = Vec::new();
        for p in &ed.problems {
            if let Some(t) = p.token.as_deref().and_then(rbs_ui::tokens::themeable)
                && !names.contains(&t.name)
            {
                names.push(t.name);
            }
        }
        let mut parts: Vec<String> = Vec::new();
        for g in GROUPS {
            let EditorGroup::Tokens(g) = g else { continue };
            let in_group: Vec<&str> = THEMEABLE
                .iter()
                .filter(|t| t.group == g && names.contains(&t.name))
                .map(|t| t.label)
                .collect();
            if !in_group.is_empty() {
                parts.push(format!("{}: {}", group_label(g), in_group.join(", ")));
            }
        }
        let count = names.len() + self.general_problems().len();
        let head = if count == 1 {
            "1 setting needs a change before you can save.".to_owned()
        } else {
            format!("{count} settings need a change before you can save.")
        };
        let text = if parts.is_empty() {
            head
        } else {
            format!("{head} {}.", parts.join(" · "))
        };
        (text, !issues.is_empty())
    }
}

/// The name of a token group in the editor.
pub fn group_label(g: Group) -> &'static str {
    match g {
        Group::Colours => "All colours",
        Group::Text => "Text",
        Group::Font => "Font",
        Group::SpacingAndSize => "Spacing and size",
        Group::Shapes => "Shapes",
        Group::Motion => "Motion",
    }
}

/// The name of an editor group.
pub fn editor_group_label(g: EditorGroup) -> &'static str {
    match g {
        EditorGroup::Main => "Main colours",
        EditorGroup::Tokens(g) => group_label(g),
    }
}

/// Every theme colour of `body`: its base's, with its colour overrides.
fn colors_of(file: &TokenFile, body: &ThemeBody) -> Colors {
    let mut c = file.built_in_theme(body.base).colors;
    for (n, v) in &body.colors {
        if c.contains_key(n) {
            c.insert(n.clone(), *v);
        }
    }
    c
}

fn assist(file: &TokenFile, body: &ThemeBody) -> Assist {
    Assist::new(
        body.base == Base::HighContrast,
        file.fixed
            .get("color.state.live")
            .copied()
            .unwrap_or(Color {
                r: 0xc4,
                g: 0x26,
                b: 0x2e,
                a: 255,
            }),
    )
}

/// Gives `d` the colours `all`, as overrides only where they differ from `base`.
fn set_colors(d: &mut ThemeBody, base: &Colors, all: &Colors) {
    for (n, c) in all {
        if base.get(n) == Some(c) {
            d.colors.remove(n);
        } else {
            d.colors.insert(n.clone(), *c);
        }
    }
}

/// The problem text of the row of `token`, in words, and whether "Fix" can help. A
/// colour's contrast problems are told from its side (what is hard to read on what),
/// at most three, the rest counted.
fn row_problem(ed: &Editor, issues: &[Issue], token: &str) -> (String, bool) {
    const MOST: usize = 3;
    let mut mine: Vec<&Issue> = issues
        .iter()
        .filter(|i| i.tokens().contains(&token))
        .collect();
    // The live red first: it is rule U12, and the reason a colour cannot be used at all.
    mine.sort_by_key(|i| matches!(i, Issue::Contrast { .. }));
    let mut lines: Vec<String> = mine.iter().map(|i| i.describe(token)).collect();
    // Problems the colour helper does not cover (hue, bounds, order): their reasons
    // with the tokens named in words.
    let is_colour = rbs_ui::tokens::themeable(token).is_some_and(|t| t.kind == Kind::Color);
    for p in ed
        .problems
        .iter()
        .filter(|p| p.token.as_deref() == Some(token))
    {
        let covered = is_colour
            && (p.reason.contains("contrast") || p.reason.contains("looks like the live"));
        if !covered {
            lines.push(plain(&p.reason));
        }
    }
    let fixable = !mine.is_empty();
    if lines.len() > MOST {
        let more = lines.len() - MOST;
        lines.truncate(MOST);
        lines.push(format!("and {more} more"));
    }
    (lines.join("\n"), fixable)
}

/// A problem's reason with the token names in words.
fn plain(reason: &str) -> String {
    let mut names: Vec<&rbs_ui::tokens::Themeable> = THEMEABLE.iter().collect();
    names.sort_by_key(|t| std::cmp::Reverse(t.name.len()));
    let mut out = reason.to_owned();
    for t in names {
        out = out.replace(t.name, t.label);
    }
    let mut c = out.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
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
    /// Its problems in words, one per line; empty when none.
    pub problem: String,
    /// "Fix" can move this colour so its contrast problems go away.
    pub fixable: bool,
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
    fn a_deleted_theme_leaves_the_slots_that_named_it() {
        let (_d, data, _file, mut ui, id) = setup("forget");
        ui.choice.dark = ThemeId::User(id.clone());
        ui.choice.theme = ThemeId::User(id.clone());
        let next = ui.forget(&id, Base::HighContrast).unwrap();
        assert_eq!(next.dark, ThemeId::BuiltIn(Base::HighContrast));
        assert_eq!(next.theme, ThemeId::BuiltIn(Base::HighContrast));
        assert_eq!(next.light, ThemeId::BuiltIn(Base::Light));
        assert!(ui.forget(&id, Base::Dark).is_none());
        assert!(ui.keep.is_none());

        // At start: a slot naming a theme whose file is gone takes the slot's default.
        let lock = data.try_lock().unwrap().unwrap();
        let mut choice = data.appearance().0;
        choice.light = ThemeId::User(EntryId::parse("gone").unwrap());
        choice.dark = ThemeId::User(id);
        data.set_appearance(&lock, &choice).unwrap();
        let healed = forget_missing(&data).unwrap();
        assert_eq!(healed.light, ThemeId::BuiltIn(Base::Light));
        assert_eq!(healed.dark, choice.dark);
    }

    #[test]
    fn a_font_change_asks_to_keep_only_for_a_theme_in_use() {
        let (_d, _data, file, mut ui, id) = setup("font-not-in-use");
        ui.selected = Some(3);
        assert!(ui.edit(&file));
        assert!(ui.set_fonts(&file, "Inter", Instant::now()));
        assert!(ui.keep.is_none());
        assert!(!ui.in_use(&ThemeId::User(id)));
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
        ui.group = 1;
        let row = ui
            .rows(&file)
            .into_iter()
            .find(|r| r.name == "color.accent")
            .unwrap();
        assert!(row.overridden && row.problem.contains("live"), "{row:?}");
        let slots = ui.slots(&data, &file, &mut notes);
        assert_ne!(
            slots[0].colors["color.accent"],
            Color::parse("#c4262e").unwrap()
        );
        assert!(ui.to_save().is_none());

        // The failing edit holds back only itself: the earlier and later passing
        // edits still show.
        assert_eq!(
            slots[0].colors["color.surface.base"],
            Color::parse("#0b1020").unwrap()
        );
        assert!(ui.set_number(&file, "radius.md", 6.0));
        let slots = ui.slots(&data, &file, &mut notes);
        assert_eq!(slots[0].length["radius.md"], 6.0);
        assert!(ui.to_save().is_none(), "Save waits for the failing edit");

        // Reset removes the override.
        assert!(ui.reset(&file, "color.accent"));
        assert!(ui.to_save().is_some());
        ui.cancel();
        assert!(ui.editor.is_none());
    }

    #[test]
    fn an_edit_that_fails_because_of_another_shows_once_that_one_is_fixed() {
        let (_d, data, file, mut ui, id) = setup("depends");
        ui.selected = ui.index_of(&ThemeId::User(id.clone()));
        assert!(ui.edit(&file));
        ui.choice = Appearance::new(
            AppearanceMode::Fixed,
            ThemeId::BuiltIn(Base::Dark),
            ThemeId::BuiltIn(Base::Light),
            ThemeId::User(id),
        );
        // A failing edit that stays: the live red as accent.
        assert!(ui.set_color(&file, "color.accent", "#c4262e"));
        // Medium corners above the large ones fail the order rule: held back.
        assert!(ui.set_number(&file, "radius.md", 12.0));
        let mut notes = Vec::new();
        let slots = ui.slots(&data, &file, &mut notes);
        assert_ne!(slots[0].length["radius.md"], 12.0);
        // Larger large corners pass, and then the medium ones pass too, while the
        // accent is still held back.
        assert!(ui.set_number(&file, "radius.lg", 16.0));
        let slots = ui.slots(&data, &file, &mut notes);
        assert_eq!(slots[0].length["radius.lg"], 16.0);
        assert_eq!(slots[0].length["radius.md"], 12.0);
        assert_ne!(
            slots[0].colors["color.accent"],
            Color::parse("#c4262e").unwrap()
        );
        assert!(ui.to_save().is_none());
    }

    #[test]
    fn main_colours_set_a_whole_palette_that_can_be_saved() {
        let (_d, _data, file, mut ui, id) = setup("main");
        ui.selected = ui.index_of(&ThemeId::User(id));
        assert!(ui.edit(&file));
        assert!(ui.on_main(), "the editor opens on the main colours");
        let rows = ui.rows(&file);
        let labels: Vec<&str> = rows.iter().map(|r| r.label).collect();
        assert_eq!(labels, ["Background", "Text", "Accent"]);
        // A light background on a dark theme: the text turns dark, nothing is left
        // to fix, and Save is on.
        assert!(ui.set_main_color(&file, "color.surface.base", "#f6f6f4"));
        let ed = ui.editor.as_ref().unwrap();
        assert!(ed.problems.is_empty(), "{:?}", ed.problems);
        assert_eq!(
            ed.draft.colors["color.surface.base"],
            Color::parse("#f6f6f4").unwrap()
        );
        assert!(ui.to_save().is_some());
        assert_eq!(ui.summary(&file), (String::new(), false));
    }

    #[test]
    fn fix_and_fix_colours_clear_contrast_problems() {
        let (_d, _data, file, mut ui, id) = setup("fix");
        ui.selected = ui.index_of(&ThemeId::User(id));
        assert!(ui.edit(&file));
        ui.group = 1;
        // A mid blue window background, set one colour at a time (All colours).
        assert!(ui.set_color(&file, "color.surface.base", "#4a7bd0"));
        let row = ui
            .rows(&file)
            .into_iter()
            .find(|r| r.name == "color.surface.base")
            .unwrap();
        assert!(row.fixable);
        assert!(
            row.problem.contains("is hard to read on it"),
            "{}",
            row.problem
        );
        let (summary, can_fix) = ui.summary(&file);
        assert!(summary.contains("before you can save"), "{summary}");
        assert!(summary.contains("All colours:"), "{summary}");
        assert!(can_fix);
        // Fix on the row moves only that colour.
        assert!(ui.fix_color(&file, "color.surface.base"));
        let ed = ui.editor.as_ref().unwrap();
        assert!(
            ed.problems
                .iter()
                .all(|p| p.token.as_deref() != Some("color.surface.base")),
            "{:?}",
            ed.problems
        );
        // A second clash, fixed with Fix colours.
        assert!(ui.set_color(&file, "color.text.secondary", "#777777"));
        assert!(!ui.editor.as_ref().unwrap().problems.is_empty());
        assert!(ui.fix_all_colors(&file));
        assert!(ui.editor.as_ref().unwrap().problems.is_empty());
        assert!(ui.to_save().is_some());
    }

    #[test]
    fn problems_name_tokens_in_words() {
        let (_d, _data, file, mut ui, id) = setup("words");
        ui.selected = ui.index_of(&ThemeId::User(id));
        assert!(ui.edit(&file));
        assert!(ui.set_number(&file, "radius.sm", 30.0));
        ui.group = 5;
        let row = ui
            .rows(&file)
            .into_iter()
            .find(|r| r.name == "radius.md")
            .unwrap();
        assert!(!row.fixable);
        assert!(!row.problem.contains("radius."), "{}", row.problem);
        assert_eq!(
            row.problem,
            "Corner radius must not be smaller than Small corner radius"
        );
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
        // The theme is in use (dark slot, following the system).
        ui.choice.dark = ThemeId::User(id.clone());
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
        ui.group = 5;
        let radius = ui
            .rows(&file)
            .into_iter()
            .find(|r| r.name == "radius.md")
            .unwrap();
        assert_eq!(radius.value, RowValue::Number(8.0));
        assert!(radius.overridden);
        // Text styles change one part at a time.
        ui.group = 2;
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
