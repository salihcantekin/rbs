//! Settings › Appearance (ADR-0057, themes spec): the theme choice, the theme list and
//! the theme editor. Toolkit-free state and rules; `main.rs` shows it in the
//! `Appearance` global and applies the look it asks for.

use std::time::{Duration, Instant};

use rbs_ui::tokens::{
    Base, Color, ColorRule, FONT_FAMILY, Group, Kind, ResolvedTheme, THEMEABLE, ThemeBody,
    ThemeProblem, Tokens as TokenFile, TypeStyle,
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
    /// The draft's problems: errors hold it (not applied, not saved), warnings do not
    /// (ADR-0072).
    pub problems: Vec<ThemeProblem>,
}

impl Editor {
    /// True when a problem refuses the draft.
    pub fn has_error(&self) -> bool {
        self.problems.iter().any(ThemeProblem::is_error)
    }
}

/// The contrast under which text is unreadable rather than poor: a colour edit that
/// takes primary text on a surface the editor draws below it asks "Keep this look?"
/// (ADR-0072 decision 5), so the user always has a way back.
pub const READABLE_FLOOR: f64 = 1.5;

/// The surfaces the Settings window draws primary text on.
const EDITOR_SURFACES: [&str; 3] = [
    "color.surface.base",
    "color.surface.raised",
    "color.surface.overlay",
];

/// True when `body` resolves (it is applied) and primary text on a surface the
/// editor draws is under [`READABLE_FLOOR`].
fn unreadable(file: &TokenFile, body: &ThemeBody) -> bool {
    let Ok(r) = file.resolve(body) else {
        return false;
    };
    let Some(text) = r.colors.get("color.text.primary") else {
        return false;
    };
    EDITOR_SURFACES
        .iter()
        .filter_map(|s| r.colors.get(*s))
        .any(|bg| text.contrast(*bg) < READABLE_FLOOR)
}

/// What "Keep this look?" goes back to.
#[derive(Debug, Clone, PartialEq)]
pub enum Previous {
    /// The appearance choice before a change.
    Choice(Appearance),
    /// The edited theme's draft before a change (of the font family, or of a colour
    /// that made text unreadable).
    Draft(ThemeBody),
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

    /// How many colour warnings the theme at `index` has (ADR-0072); 0 for a theme that
    /// cannot be used.
    pub fn warning_count_of(&self, index: usize) -> usize {
        self.entries
            .get(index)
            .and_then(|e| e.resolved.as_ref().ok())
            .map_or(0, |r| r.warnings.len())
    }

    /// What to tell the user after importing the theme `id`: its name and warnings,
    /// the Live ones named apart because they can hide whether the show is live.
    /// Empty when it has none.
    pub fn import_note(&self, id: &ThemeId) -> String {
        let Some(e) = self.index_of(id).and_then(|i| self.entries.get(i)) else {
            return String::new();
        };
        let Ok(r) = &e.resolved else {
            return String::new();
        };
        let total = r.warnings.len();
        if total == 0 {
            return String::new();
        }
        let live = r
            .warnings
            .iter()
            .filter(|w| {
                matches!(
                    &w.rule,
                    Some(ColorRule::LiveHue { .. } | ColorRule::LiveLook { .. })
                ) || matches!(&w.rule, Some(ColorRule::Contrast { fg, .. }) if fg == "color.state.live")
            })
            .count();
        let mut out = format!(
            "Imported \"{}\". It has {} colour warning{}",
            e.name,
            total,
            if total == 1 { "" } else { "s" }
        );
        if live > 0 {
            out.push_str(&format!(
                ", {live} about the Live colour, which can hide whether you are live"
            ));
        }
        out.push_str(". Open it to see them.");
        out
    }

    /// The list position of `id`.
    pub fn index_of(&self, id: &ThemeId) -> Option<usize> {
        self.entries.iter().position(|e| &e.id == id)
    }

    /// The themes of the two slots (OS dark, OS light), with the editor's draft in
    /// place of its saved theme while the draft has no error (warnings apply too,
    /// ADR-0072).
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
                && let Ok(r) = file.resolve(&ed.draft)
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
            Previous::Draft(body) => {
                if let Some(ed) = &mut self.editor {
                    ed.draft = body;
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

    /// Sets a colour override from `#rrggbb` text. True when the draft changed. Asks
    /// "Keep this look?" when it makes text unreadable in a theme in use.
    pub fn set_color(&mut self, file: &TokenFile, name: &str, hex: &str, now: Instant) -> bool {
        let Some(c) = Color::parse(hex).map(|c| Color { a: 255, ..c }) else {
            return false;
        };
        self.change_looked(file, now, |d| {
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
        let before = ed.draft.clone();
        let changed = self.change(file, |d| d.font_family = Some(list));
        if changed {
            self.ask_keep(before, now);
        }
        changed
    }

    /// "Keep this look?" for an edit of the theme in use, going back to `before`.
    /// Only a theme in use changes the look, so only then is it asked; a question
    /// already open keeps the first look of the series.
    fn ask_keep(&mut self, before: ThemeBody, now: Instant) {
        let Some(ed) = &self.editor else { return };
        if !self.in_use(&ThemeId::User(ed.id.clone()))
            || matches!(
                self.keep,
                Some(KeepLook {
                    previous: Previous::Draft(_),
                    ..
                })
            )
        {
            return;
        }
        self.keep = Some(KeepLook {
            previous: Previous::Draft(before),
            since: now,
        });
    }

    /// A change that can affect readability: asks "Keep this look?" when it takes
    /// text from readable to unreadable (ADR-0072 decision 5).
    fn change_looked(
        &mut self,
        file: &TokenFile,
        now: Instant,
        f: impl FnOnce(&mut ThemeBody),
    ) -> bool {
        let Some(ed) = &self.editor else {
            return false;
        };
        let before = ed.draft.clone();
        if !self.change(file, f) {
            return false;
        }
        let after = self.editor.as_ref().map(|e| &e.draft);
        if !unreadable(file, &before) && after.is_some_and(|a| unreadable(file, a)) {
            self.ask_keep(before, now);
        }
        true
    }

    /// The "Fix" of a colour warning: the colour moves to the nearest lightness that
    /// clears the token's warnings (ADR-0072). True when the draft changed.
    pub fn fix_colour(&mut self, file: &TokenFile, name: &str, now: Instant) -> bool {
        let Some(ed) = &self.editor else {
            return false;
        };
        let Some(c) = rbs_ui::tokens::suggest_colour(file, &ed.draft, name) else {
            return false;
        };
        self.change_looked(file, now, |d| {
            d.colors.insert(name.to_owned(), c);
        })
    }

    /// Removes an override (the token takes the base's value again).
    pub fn reset(&mut self, file: &TokenFile, name: &str, now: Instant) -> bool {
        self.change_looked(file, now, |d| {
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
                previous: Previous::Draft(_),
                ..
            })
        ) {
            self.keep = None;
        }
    }

    /// The draft to save, if it has no error (warnings are saved, ADR-0072) and
    /// differs from the saved theme.
    pub fn to_save(&self) -> Option<(EntryId, String, ThemeBody)> {
        let ed = self.editor.as_ref()?;
        (!ed.has_error() && ed.draft != ed.saved)
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
                previous: Previous::Draft(_),
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
                    .filter(|p| p.is_error() && p.token.as_deref() == Some(t.name))
                    .map(|p| p.reason.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                let warning = warnings_for(&ed.problems, t.name).join(" ");
                let fixable = !warning.is_empty()
                    && matches!(t.kind, Kind::Color)
                    && rbs_ui::tokens::suggest_colour(file, &ed.draft, t.name).is_some();
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
                    warning,
                    fixable,
                }
            })
            .collect()
    }

    /// The number of warnings of the draft (ADR-0072).
    pub fn warning_count(&self) -> usize {
        self.editor
            .as_ref()
            .map_or(0, |ed| ed.problems.iter().filter(|p| !p.is_error()).count())
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

/// A token's label in words, for warnings.
fn label_of(token: &str) -> &'static str {
    if token == "color.state.live" {
        return "the Live badge";
    }
    rbs_ui::tokens::themeable(token).map_or("another colour", |t| t.label)
}

/// The warnings of `problems` that concern the row `token`, in words with labels
/// (never token ids): a contrast pair shows on both of its rows, live warnings first.
pub fn warnings_for(problems: &[ThemeProblem], token: &str) -> Vec<String> {
    let mut live = Vec::new();
    let mut other = Vec::new();
    for p in problems.iter().filter(|p| !p.is_error()) {
        let (words, about_live) = match &p.rule {
            Some(ColorRule::Contrast {
                fg,
                bg,
                ratio,
                need,
            }) => {
                let n = format!("contrast {ratio:.1}, needs {need}");
                if fg == "color.state.live" && bg == token {
                    (
                        format!(
                            "The Live badge is hard to see on this colour ({n}). Pick a lighter or darker colour."
                        ),
                        true,
                    )
                } else if fg == token {
                    (
                        format!(
                            "Hard to read on {} ({n}). Pick a lighter or darker colour.",
                            label_of(bg)
                        ),
                        false,
                    )
                } else if bg == token {
                    (
                        format!(
                            "{} is hard to read on this colour ({n}). Pick a lighter or darker colour.",
                            label_of(fg)
                        ),
                        false,
                    )
                } else {
                    continue;
                }
            }
            Some(ColorRule::LiveLook { token: t, .. }) if t == token => (
                "Looks like the Live colour, so it may be taken for live. Pick a different colour."
                    .to_owned(),
                true,
            ),
            Some(ColorRule::LiveHue { token: t }) if t == token => (
                "Its hue is close to the Live colour. Pick a different hue.".to_owned(),
                true,
            ),
            _ => continue,
        };
        if about_live {
            live.push(words);
        } else {
            other.push(words);
        }
    }
    live.dedup();
    other.dedup();
    live.extend(other);
    // Two sentences at most: the rest would bury the row.
    const SHOWN: usize = 2;
    if live.len() > SHOWN {
        let more = live.len() - SHOWN;
        live.truncate(SHOWN);
        live.push(format!("And {more} more."));
    }
    live
}

/// Every problem of `body`: errors and warnings.
fn problems(file: &TokenFile, body: &ThemeBody) -> Vec<ThemeProblem> {
    match file.resolve(body) {
        Ok(r) => r.warnings,
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
    /// Its errors, joined; empty when none.
    pub problem: String,
    /// Its colour warnings in words (ADR-0072); empty when none.
    pub warning: String,
    /// True when "Fix" can clear them by moving the colour's lightness.
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
    fn editing_applies_every_draft_without_an_error() {
        let (_d, data, file, mut ui, id) = setup("edit");
        ui.selected = ui.index_of(&ThemeId::User(id.clone()));
        assert!(ui.edit(&file));
        ui.choice = Appearance::new(
            AppearanceMode::Fixed,
            ThemeId::BuiltIn(Base::Dark),
            ThemeId::BuiltIn(Base::Light),
            ThemeId::User(id),
        );
        let t0 = Instant::now();
        assert!(ui.set_color(&file, "color.surface.base", "#0b1020", t0));
        let mut notes = Vec::new();
        let slots = ui.slots(&data, &file, &mut notes);
        assert_eq!(
            slots[0].colors["color.surface.base"],
            Color::parse("#0b1020").unwrap()
        );
        assert!(ui.to_save().is_some());

        // A colour that breaks a rule is applied and saved, with its warning shown
        // (ADR-0072).
        assert!(ui.set_color(&file, "color.accent", "#c4262e", t0));
        ui.group = 0;
        let row = ui
            .rows(&file)
            .into_iter()
            .find(|r| r.name == "color.accent")
            .unwrap();
        assert!(row.overridden && row.problem.is_empty(), "{row:?}");
        assert!(
            row.warning.starts_with("Looks like the Live colour"),
            "{row:?}"
        );
        assert!(
            !row.warning.contains("color."),
            "labels, not token ids: {row:?}"
        );
        let slots = ui.slots(&data, &file, &mut notes);
        assert_eq!(
            slots[0].colors["color.accent"],
            Color::parse("#c4262e").unwrap()
        );
        assert!(ui.to_save().is_some());

        // An error (a value out of bounds) holds the draft: the saved theme stays.
        assert!(ui.set_number(&file, "radius.md", 500.0));
        let slots = ui.slots(&data, &file, &mut notes);
        assert_eq!(slots[0].length["radius.md"], 8.0, "saved theme still used");
        assert!(ui.to_save().is_none());

        // Reset removes the override.
        assert!(ui.reset(&file, "radius.md", t0));
        assert!(ui.to_save().is_some());
        ui.cancel();
        assert!(ui.editor.is_none());
    }

    #[test]
    fn fix_moves_a_colour_until_its_warnings_are_gone() {
        let (_d, _data, file, mut ui, id) = setup("fix");
        ui.selected = ui.index_of(&ThemeId::User(id));
        ui.edit(&file);
        let t0 = Instant::now();
        assert!(ui.set_color(&file, "color.surface.base", "#5b8de0", t0));
        ui.group = 0;
        let row = |ui: &AppearanceUi| {
            ui.rows(&file)
                .into_iter()
                .find(|r| r.name == "color.surface.base")
                .unwrap()
        };
        assert!(row(&ui).fixable && row(&ui).warning.contains("Live badge"));
        assert!(ui.fix_colour(&file, "color.surface.base", t0));
        let r = row(&ui);
        assert!(r.warning.is_empty() && !r.fixable, "{r:?}");
        assert!(
            !ui.fix_colour(&file, "color.surface.base", t0),
            "nothing left to fix"
        );
    }

    #[test]
    fn a_theme_with_warnings_counts_them_and_an_import_says_so() {
        let (_d, data, file, mut ui, _id) = setup("import-note");
        let mut body = ThemeBody::new(Base::Dark);
        body.colors
            .insert("color.accent".into(), Color::parse("#c4262e").unwrap());
        let id = {
            let lock = data.try_lock().unwrap().unwrap();
            data.themes().create(&lock, "Loud", &body).unwrap()
        };
        ui.refresh(&data, &file);
        let theme = ThemeId::User(id);
        let i = ui.index_of(&theme).unwrap();
        assert!(ui.warning_count_of(i) > 0);
        assert_eq!(ui.warning_count_of(0), 0, "a built-in theme has none");
        let note = ui.import_note(&theme);
        assert!(note.starts_with("Imported \"Loud\". It has "), "{note}");
        assert!(note.contains("about the Live colour"), "{note}");
        // A theme without warnings says nothing.
        assert!(ui.import_note(&ThemeId::BuiltIn(Base::Dark)).is_empty());
    }

    #[test]
    fn unreadable_text_asks_to_keep_and_undo_brings_it_back() {
        let (_d, _data, file, mut ui, id) = setup("unreadable");
        ui.choice.dark = ThemeId::User(id.clone());
        ui.selected = ui.index_of(&ThemeId::User(id));
        ui.edit(&file);
        let t0 = Instant::now();
        // Poor but readable: no question.
        assert!(ui.set_color(&file, "color.surface.base", "#5b8de0", t0));
        assert!(ui.keep.is_none());
        // Text close to the surfaces: "Keep this look?".
        assert!(ui.set_color(&file, "color.text.primary", "#1c1f24", t0));
        assert!(ui.keep.is_some());
        assert!(ui.undo(&file).is_none());
        let ed = ui.editor.as_ref().unwrap();
        assert!(!ed.draft.colors.contains_key("color.text.primary"));
        assert!(ed.draft.colors.contains_key("color.surface.base"));
    }

    #[test]
    fn reloading_lists_new_files_and_checks_the_draft_again() {
        let (_d, data, file, mut ui, id) = setup("reload");
        ui.selected = ui.index_of(&ThemeId::User(id));
        assert!(ui.edit(&file));
        assert!(ui.set_color(&file, "color.accent", "#c4262e", Instant::now()));
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
