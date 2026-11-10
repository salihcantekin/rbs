//! Settings › Appearance in the Settings window (ADR-0057, themes spec): shows the
//! [`AppearanceUi`] state in the `Appearance` global, handles its callbacks and applies
//! the look it asks for to every open window.

use std::rc::Rc;
use std::time::{Duration, Instant};

use rbs_ui::tokens::{Base, Color, Tokens as TokenFile};
use rbs_user_data::{AppearanceMode, ThemeId};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{App, Ctx, Theme, Tokens, entry, look};
use crate::appearance::{AppearanceUi, GROUPS, KEEP_LOOK_TIMEOUT, RowValue};
use crate::ui::{Appearance, Overlays, PickerRow, ThemeOption, ThemeTokenRow};

/// The page's state, kept while the app runs (it is small and idle when unused).
#[derive(Default)]
pub struct Page {
    ui: AppearanceUi,
    fonts: look::SystemFonts,
    /// The families the font picker lists now (a row's index is its place here).
    font_listing: Vec<String>,
    /// The installed families, read the first time the font picker opens (P13).
    families: Option<Vec<String>>,
    /// The lists the page shows, changed row by row: a list set anew rebuilds its rows
    /// and closes a menu open in one (the font menu while "Keep this look?" counts).
    themes: Rc<VecModel<ThemeOption>>,
    names: Rc<VecModel<SharedString>>,
    groups: Rc<VecModel<SharedString>>,
    rows: Rc<VecModel<ThemeTokenRow>>,
    /// List positions of the usable themes (the pickers' options).
    usable: Vec<usize>,
    /// A theme in use could not be used, or an action failed.
    note: String,
    /// Counts down "Keep this look?" once a second; runs only while it is asked.
    timer: Rc<slint::Timer>,
}

fn label(g: rbs_ui::tokens::Group) -> &'static str {
    use rbs_ui::tokens::Group::*;
    match g {
        Colours => "Colours",
        Text => "Text",
        Font => "Font",
        SpacingAndSize => "Spacing and size",
        Shapes => "Shapes",
        Motion => "Motion",
    }
}

fn color(c: Color) -> slint::Color {
    slint::Color::from_argb_u8(c.a, c.r, c.g, c.b)
}

/// Connects the Settings window's `Appearance` global.
pub fn install(g: &Appearance<'_>, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$app:ident, $page:ident, $file:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let ctx = ctx.clone();
            g.$name(move |$($arg: $ty),*| {
                let c = ctx.clone();
                ctx.update(move |$app: &mut App| {
                    let file = c.look.borrow().as_ref().map(|l| l.file.clone());
                    let Some($file) = file else { return };
                    let changed = {
                        let mut guard = c.page.borrow_mut();
                        let $page: &mut Page = &mut guard;
                        $body
                    };
                    after(&c, $app, changed);
                })
            });
        }};
    }
    on!(on_pick, |app, page, _file, slot: i32, index: i32| {
        let Some(&i) = page.usable.get(index.max(0) as usize) else {
            return;
        };
        choose(app, page, |ui, now| ui.pick(slot as usize, i, now))
    });
    on!(on_set_follow_system, |app, page, _file, on: bool| {
        choose(app, page, |ui, now| ui.set_follow_system(on, now))
    });
    on!(on_set_compact, |app, page, _file, on: bool| {
        choose(app, page, |ui, now| ui.set_compact(on, now))
    });
    on!(on_select, |_app, page, _file, index: i32| {
        page.ui.selected = usize::try_from(index).ok();
        Change::Page
    });
    on!(on_use_selected, |app, page, _file| {
        let Some(i) = page.ui.selected else {
            return;
        };
        let slot = match page.ui.choice.mode {
            AppearanceMode::Fixed => 2,
            _ => app.main.global::<Theme>().get_active() as usize,
        };
        choose(app, page, |ui, now| ui.pick(slot, i, now))
    });
    on!(on_edit, |app, page, file| {
        let built_in = page
            .ui
            .selected
            .and_then(|i| page.ui.entries.get(i))
            .is_some_and(|e| matches!(e.id, ThemeId::BuiltIn(_)));
        if built_in && !duplicate(app, page, &file) {
            return;
        }
        page.ui.edit(&file);
        Change::Look
    });
    on!(on_duplicate, |app, page, file| {
        duplicate(app, page, &file);
        Change::Page
    });
    on!(on_rename, |app, page, file, name: SharedString| {
        let Some(ThemeId::User(id)) = selected_id(page) else {
            return;
        };
        if let Err(why) = app.studio.user.rename_theme(&id, name.trim()) {
            page.note = why;
        }
        page.ui.refresh(app.studio.user.data(), &file);
        Change::Page
    });
    on!(on_delete, |app, page, file| {
        let Some(ThemeId::User(id)) = selected_id(page) else {
            return;
        };
        let base = page
            .ui
            .index_of(&ThemeId::User(id.clone()))
            .and_then(|i| page.ui.entries[i].body.as_ref())
            .map_or(Base::Dark, |b| b.base);
        match app.studio.user.delete_theme(&id) {
            // A deleted theme in use: its slots take the built-in theme of its base
            // and the choice is saved (themes spec, "Deleting").
            Ok(()) => {
                if let Some(next) = page.ui.forget(&id, base)
                    && let Err(why) = app.studio.user.set_appearance(&next)
                {
                    page.note = why;
                }
            }
            Err(why) => page.note = why,
        }
        page.ui.selected = None;
        page.ui.refresh(app.studio.user.data(), &file);
        // A deleted theme in use falls back to its base (themes spec).
        Change::Look
    });
    {
        let ctx = ctx.clone();
        g.on_import(move || entry::guard(|| transfer(&ctx, true)));
    }
    {
        let ctx = ctx.clone();
        g.on_export(move || entry::guard(|| transfer(&ctx, false)));
    }
    on!(on_set_group, |_app, page, _file, group: i32| {
        page.ui.group = (group.max(0) as usize).min(GROUPS.len() - 1);
        Change::Page
    });
    on!(on_set_color, |_app,
                       page,
                       file,
                       name: SharedString,
                       hex: SharedString| {
        edited(page.ui.set_color(&file, &name, &hex, Instant::now()))
    });
    on!(on_set_number, |_app,
                        page,
                        file,
                        name: SharedString,
                        value: f32| {
        edited(page.ui.set_number(&file, &name, value))
    });
    on!(on_set_style, |_app,
                       page,
                       file,
                       name: SharedString,
                       part: i32,
                       value: f32| {
        edited(page.ui.set_style(&file, &name, part.max(0) as usize, value))
    });
    on!(on_set_fonts, |_app, page, file, text: SharedString| {
        edited(page.ui.set_fonts(&file, &text, Instant::now()))
    });
    // The font picker: the installed families whose names contain the filter fill the
    // picker's rows (`Overlays.picker-*`); only the picker changes, so the editor's
    // rows stay as they are.
    {
        let ctx = ctx.clone();
        g.on_find_fonts(move |filter: SharedString| fill_font_picker(&ctx, &filter));
    }
    // The font field's suggestions for the family being typed. Like the font menu it
    // reads the installed families the first time it is needed, and only fills the
    // list, so the editor's rows (and the field being typed in) stay as they are.
    {
        let ctx = ctx.clone();
        g.on_suggest_fonts(move |text: SharedString| {
            let c = ctx.clone();
            ctx.update(move |app: &mut App| {
                let Some(w) = &app.settings else { return };
                let mut page = c.page.borrow_mut();
                let page: &mut Page = &mut page;
                let names: Vec<SharedString> = if text.trim().is_empty() {
                    Vec::new()
                } else {
                    let all = page
                        .families
                        .get_or_insert_with(|| look::FontCatalog::families(&mut page.fonts));
                    look::suggest_families(all, &text)
                        .into_iter()
                        .map(SharedString::from)
                        .collect()
                };
                w.global::<Appearance>()
                    .set_font_suggestions(ModelRc::new(VecModel::from(names)));
            });
        });
    }
    g.on_complete_font(|text: SharedString, family: SharedString| {
        entry::guard_or(
            || text.clone(),
            || look::complete_family(&text, &family).into(),
        )
    });
    on!(on_fix, |_app, page, file, name: SharedString| {
        edited(page.ui.fix_colour(&file, &name, Instant::now()))
    });
    on!(on_reset, |_app, page, file, name: SharedString| {
        edited(page.ui.reset(&file, &name, Instant::now()))
    });
    on!(on_save, |app, page, file| {
        let Some((id, name, body)) = page.ui.to_save() else {
            return;
        };
        match app.studio.user.save_theme(&id, &name, body) {
            Ok(()) => {
                page.ui.saved();
                page.ui.refresh(app.studio.user.data(), &file);
                page.note.clear();
            }
            Err(why) => page.note = why,
        }
        Change::Look
    });
    on!(on_cancel, |_app, page, _file| {
        page.ui.cancel();
        Change::Look
    });
    on!(on_keep, |_app, page, _file| {
        page.ui.keep();
        Change::Page
    });
    on!(on_undo, |app, page, file| {
        if let Some(a) = page.ui.undo(&file)
            && let Err(why) = app.studio.user.set_appearance(&a)
        {
            page.note = why;
        }
        Change::Look
    });
}

/// What a callback changed.
#[derive(Clone, Copy, PartialEq)]
enum Change {
    /// Only what the page shows.
    Page,
    /// The look of every window too.
    Look,
}

fn edited(changed: bool) -> Change {
    if changed { Change::Look } else { Change::Page }
}

fn choose(
    app: &mut App,
    page: &mut Page,
    f: impl FnOnce(&mut AppearanceUi, Instant) -> Option<rbs_user_data::Appearance>,
) -> Change {
    let Some(a) = f(&mut page.ui, Instant::now()) else {
        return Change::Page;
    };
    match app.studio.user.set_appearance(&a) {
        Ok(()) => page.note.clear(),
        Err(why) => page.note = why,
    }
    Change::Look
}

fn selected_id(page: &Page) -> Option<ThemeId> {
    page.ui
        .selected
        .and_then(|i| page.ui.entries.get(i))
        .map(|e| e.id.clone())
}

/// Adds a copy of the selected theme and selects it; false when it failed.
fn duplicate(app: &mut App, page: &mut Page, file: &TokenFile) -> bool {
    let Some((name, body)) = page.ui.duplicate_of_selected() else {
        return false;
    };
    match app.studio.user.create_theme(&name, body) {
        Ok(id) => {
            page.ui.refresh(app.studio.user.data(), file);
            page.ui.selected = page.ui.index_of(&ThemeId::User(id));
            true
        }
        Err(why) => {
            page.note = why;
            false
        }
    }
}

/// Import (`import`) a theme file, or export the selected theme, through the system
/// file dialog (asynchronous, as for output profiles).
fn transfer(ctx: &Ctx, import: bool) {
    let name = {
        let page = ctx.page.borrow();
        page.ui
            .selected
            .and_then(|i| page.ui.entries.get(i))
            .map(|e| e.name.clone())
            .unwrap_or_default()
    };
    let c = ctx.clone();
    let task = entry::spawn_local(async move {
        let picked = if import {
            super::file_dialog::open("Import theme").await
        } else {
            super::file_dialog::save(&format!("Export {name}"), &format!("{name}.toml")).await
        };
        let ctx = c.clone();
        c.update(move |app| {
            let file = ctx.look.borrow().as_ref().map(|l| l.file.clone());
            let Some(file) = file else { return };
            {
                let mut page = ctx.page.borrow_mut();
                let mut imported = String::new();
                let result = match picked {
                    Ok(None) => Ok(()),
                    Err(why) => Err(why),
                    Ok(Some(path)) if import => app.studio.user.import_theme(&path).map(|id| {
                        page.ui.refresh(app.studio.user.data(), &file);
                        page.ui.selected = page.ui.index_of(&ThemeId::User(id.clone()));
                        imported = page.ui.import_note(&ThemeId::User(id));
                    }),
                    Ok(Some(path)) => match selected_id(&page) {
                        Some(ThemeId::User(id)) => app.studio.user.export_theme(&id, &path),
                        _ => Ok(()),
                    },
                };
                match result {
                    Ok(()) => page.note = imported,
                    Err(why) => page.note = why,
                }
            }
            after(&ctx, app, Change::Page);
        });
    });
    if let Err(e) = task {
        ctx.page.borrow_mut().note = e.to_string();
    }
}

/// The Appearance page is shown: read the choice and the theme list again.
pub fn opened(ctx: &Ctx, app: &mut App) {
    let file = ctx.look.borrow().as_ref().map(|l| l.file.clone());
    {
        let mut page = ctx.page.borrow_mut();
        match &file {
            Some(file) => {
                page.ui.refresh(app.studio.user.data(), file);
                // The list opens on the theme in use (the slot the windows show now).
                if page.ui.selected.is_none() && page.ui.editor.is_none() {
                    let active = app.main.global::<Theme>().get_active().clamp(0, 1) as usize;
                    let id = page.ui.choice.slots()[active].clone();
                    page.ui.selected = page.ui.index_of(&id);
                }
            }
            None => {
                page.note = "Themes are not available: the token file cannot be read.".into();
            }
        }
    }
    show(ctx, app);
}

/// `ui.theme.reload` (View › Reload themes, themes spec "Reading"): reads the token
/// file, `appearance.toml` and the theme files again and applies the result to every
/// open window. An open editor draft stays applied. What cannot be used is shown as
/// the app's error, as at start.
pub fn reload(ctx: &Ctx, app: &mut App) {
    let mut notes = Vec::new();
    let dev = look::dev_theme().is_some();
    if dev || ctx.look.borrow().is_none() {
        // As at start: `RBS_THEME` fills both slots from the new file.
        let (loaded, note) = look::load(app.studio.user.data(), &mut look::SystemFonts::default());
        notes.extend(note);
        *ctx.look.borrow_mut() = loaded;
    } else if let Some(file) = look::token_file(&mut notes)
        && let Some(look) = ctx.look.borrow_mut().as_mut()
    {
        look.file = file;
    }
    let file = ctx.look.borrow().as_ref().map(|l| l.file.clone());
    let Some(file) = file else {
        notes.push("Themes are not available: the token file cannot be read.".into());
        app.error = Some(super::UiError::Other(notes.join(" ")));
        return;
    };
    {
        let mut page = ctx.page.borrow_mut();
        page.ui.refresh(app.studio.user.data(), &file);
        page.ui.recheck(&file);
        page.note.clear();
    }
    if dev {
        apply_all(ctx, app);
    } else {
        relook(ctx, app);
    }
    let page_note = ctx.page.borrow().note.clone();
    if !page_note.is_empty() {
        notes.push(page_note);
    }
    if !notes.is_empty() {
        app.error = Some(super::UiError::Other(notes.join(" ")));
    }
    show(ctx, app);
}

fn after(ctx: &Ctx, app: &mut App, change: Change) {
    if change == Change::Look {
        relook(ctx, app);
    }
    keep_timer(ctx);
    show(ctx, app);
}

/// Resolves the two slots again and applies them to every open window.
fn relook(ctx: &Ctx, app: &mut App) {
    // `RBS_THEME` fixes the look; the choice is still saved.
    if look::dev_theme().is_some() {
        return;
    }
    {
        let mut guard = ctx.page.borrow_mut();
        let page: &mut Page = &mut guard;
        let mut look = ctx.look.borrow_mut();
        let Some(look) = look.as_mut() else {
            return;
        };
        let mut notes = Vec::new();
        let themes = page
            .ui
            .slots(app.studio.user.data(), &look.file, &mut notes);
        look.fonts = [0, 1]
            .map(|i| look::choose_font(&themes[i].font_family, &mut page.fonts, look::SAMPLE_TEXT));
        look.themes = themes;
        if !notes.is_empty() {
            page.note = notes.join(" ");
        }
    }
    apply_all(ctx, app);
}

/// Applies the current look to every open window.
fn apply_all(ctx: &Ctx, app: &App) {
    let look = ctx.look.borrow();
    let look = look.as_ref();
    look::apply(
        &app.main.global::<Theme>(),
        &app.main.global::<Tokens>(),
        look,
    );
    for f in &app.floats {
        look::apply(
            &f.window.global::<Theme>(),
            &f.window.global::<Tokens>(),
            look,
        );
    }
    if let Some(w) = &app.settings {
        look::apply(&w.global::<Theme>(), &w.global::<Tokens>(), look);
    }
}

/// Runs the "Keep this look?" countdown while it is asked; the timeout is Undo.
fn keep_timer(ctx: &Ctx) {
    let page = ctx.page.borrow();
    if page.ui.keep.is_none() {
        page.timer.stop();
        return;
    }
    if page.timer.running() {
        return;
    }
    let c = ctx.clone();
    entry::start(
        &page.timer,
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            let ctx = c.clone();
            c.update(move |app| {
                let file = ctx.look.borrow().as_ref().map(|l| l.file.clone());
                let Some(file) = file else { return };
                let change = {
                    let mut page = ctx.page.borrow_mut();
                    let expired = page
                        .ui
                        .keep
                        .as_ref()
                        .is_none_or(|k| k.expired(Instant::now()));
                    if expired {
                        if let Some(a) = page.ui.undo(&file)
                            && let Err(why) = app.studio.user.set_appearance(&a)
                        {
                            page.note = why;
                        }
                        Change::Look
                    } else {
                        Change::Page
                    }
                };
                after(&ctx, app, change);
            });
        },
    );
}

/// Pushes the page state into the Settings window.
fn show(ctx: &Ctx, app: &App) {
    let Some(w) = &app.settings else {
        return;
    };
    let mut page = ctx.page.borrow_mut();
    let look = ctx.look.borrow();
    let g = w.global::<Appearance>();
    let ui = &page.ui;
    let usable: Vec<usize> = ui
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.resolved.is_ok())
        .map(|(i, _)| i)
        .collect();
    // A slot whose theme cannot be used shows the built-in theme used instead.
    let pos = |id: &ThemeId, fallback: Base| {
        let at = |id: &ThemeId| {
            ui.index_of(id)
                .and_then(|i| usable.iter().position(|u| *u == i))
        };
        at(id)
            .or_else(|| at(&ThemeId::BuiltIn(fallback)))
            .map_or(-1, |p| p as i32)
    };
    let options: Vec<ThemeOption> = ui
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let swatch = |name: &str| {
                e.resolved
                    .as_ref()
                    .ok()
                    .and_then(|r| r.colors.get(name))
                    .map_or(slint::Color::default(), |c| color(*c))
            };
            ThemeOption {
                name: e.name.as_str().into(),
                user: matches!(e.id, ThemeId::User(_)),
                usable: e.resolved.is_ok(),
                problem: e
                    .resolved
                    .as_ref()
                    .err()
                    .cloned()
                    .unwrap_or_default()
                    .into(),
                surface: swatch("color.surface.base"),
                text: swatch("color.text.primary"),
                accent: swatch("color.accent"),
                warnings: i32::try_from(ui.warning_count_of(i)).unwrap_or(i32::MAX),
            }
        })
        .collect();
    super::set_rows(&page.themes, options);
    super::set_rows(
        &page.names,
        usable
            .iter()
            .map(|i| SharedString::from(ui.entries[*i].name.as_str()))
            .collect(),
    );
    show_model(&g.get_themes(), &page.themes, |m| g.set_themes(m));
    show_model(&g.get_usable_names(), &page.names, |m| {
        g.set_usable_names(m)
    });
    g.set_follow_system(ui.choice.mode == AppearanceMode::System);
    g.set_compact(ui.choice.density == rbs_ui::tokens::Density::Compact);
    g.set_dark_index(pos(&ui.choice.dark, Base::Dark));
    g.set_light_index(pos(&ui.choice.light, Base::Light));
    g.set_theme_index(pos(&ui.choice.theme, Base::Dark));
    g.set_selected(ui.selected.map_or(-1, |i| i as i32));
    g.set_note(page.note.as_str().into());
    g.set_dev_override(
        look::dev_theme().is_some()
            || std::env::var_os("RBS_TOKENS").is_some_and(|v| !v.is_empty()),
    );
    g.set_keep_asking(ui.keep.is_some());
    let left = ui.keep.as_ref().map_or(0, |k| {
        KEEP_LOOK_TIMEOUT
            .saturating_sub(k.since.elapsed())
            .as_secs_f32()
            .ceil() as i32
    });
    g.set_keep_seconds(left);
    super::set_rows(
        &page.groups,
        GROUPS
            .iter()
            .map(|g| SharedString::from(label(*g)))
            .collect(),
    );
    show_model(&g.get_groups(), &page.groups, |m| g.set_groups(m));
    g.set_group(ui.group as i32);
    match (&ui.editor, look.as_ref()) {
        (Some(ed), Some(look)) => {
            g.set_editing(true);
            g.set_editor_name(ed.name.as_str().into());
            g.set_can_save(ui.to_save().is_some());
            g.set_changed(ed.draft != ed.saved);
            g.set_editing_in_use(ui.in_use(&ThemeId::User(ed.id.clone())));
            let rows: Vec<ThemeTokenRow> = ui.rows(&look.file).into_iter().map(row).collect();
            super::set_rows(&page.rows, rows);
            g.set_general_problems(strings(ui.general_problems()));
            g.set_warning_count(i32::try_from(ui.warning_count()).unwrap_or(i32::MAX));
            g.set_unknown_tokens(strings(ed.draft.unknown_names()));
            let active = app.main.global::<Theme>().get_active().clamp(0, 1) as usize;
            g.set_font_in_use(look.fonts[active].as_str().into());
        }
        _ => {
            g.set_editing(false);
            super::set_rows(&page.rows, Vec::new());
        }
    }
    show_model(&g.get_rows(), &page.rows, |m| g.set_rows(m));
    page.usable = usable;
}

/// Gives the page `model` the first time (later changes go through it row by row).
fn show_model<T: Clone + 'static>(
    shown: &ModelRc<T>,
    model: &Rc<VecModel<T>>,
    set: impl FnOnce(ModelRc<T>),
) {
    let ours = ModelRc::from(model.clone());
    if *shown != ours {
        set(ours);
    }
}

fn strings(v: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        v.into_iter().map(SharedString::from).collect::<Vec<_>>(),
    ))
}

fn row(r: crate::appearance::TokenRow) -> ThemeTokenRow {
    let mut out = ThemeTokenRow {
        name: r.name.into(),
        label: r.label.into(),
        overridden: r.overridden,
        problem: r.problem.into(),
        warning: r.warning.into(),
        fixable: r.fixable,
        step: 1.0,
        ..Default::default()
    };
    match r.value {
        RowValue::Color(c) => {
            out.kind = 0;
            out.color = color(c);
        }
        RowValue::Number(n) => {
            out.kind = 1;
            out.number = n;
            out.unit = if r.name.starts_with("motion.") {
                "ms"
            } else {
                "px"
            }
            .into();
        }
        RowValue::Style(s) => {
            out.kind = 2;
            out.size = f32::from(s.size);
            out.line = f32::from(s.line);
            out.weight = f32::from(s.weight);
        }
        RowValue::Fonts(f) => {
            out.kind = 3;
            out.fonts = f.into();
        }
    }
    out
}

/// Fills the font picker's rows from the installed families (read once, P13). A row's
/// index is its place in `font_listing`, which a pick resolves against.
fn fill_font_picker(ctx: &Ctx, filter: &str) {
    let c = ctx.clone();
    let filter = filter.to_owned();
    ctx.update(move |app: &mut App| {
        let Some(w) = &app.settings else { return };
        let mut page = c.page.borrow_mut();
        let page: &mut Page = &mut page;
        let all = page
            .families
            .get_or_insert_with(|| look::FontCatalog::families(&mut page.fonts));
        page.font_listing = look::matching_families(all, &filter)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let rows: Vec<PickerRow> = page
            .font_listing
            .iter()
            .enumerate()
            .map(|(i, name)| PickerRow {
                name: name.as_str().into(),
                note: "".into(),
                index: i as i32,
            })
            .collect();
        let o = w.global::<Overlays>();
        let status = if rows.is_empty() {
            "No font on this computer has that in its name."
        } else {
            ""
        };
        o.set_picker_status(status.into());
        o.set_picker_rows(ModelRc::new(VecModel::from(rows)));
    });
}

/// The Settings window's picker callbacks: a filter refills the list, a pick goes back
/// to the button that opened it.
pub fn install_picker(o: &Overlays<'_>, ctx: &Ctx) {
    {
        let ctx = ctx.clone();
        o.on_picker_filtered(move |text| entry::guard(|| fill_font_picker(&ctx, &text)));
    }
    {
        let ctx = ctx.clone();
        o.on_picker_chosen(move |index| {
            let c = ctx.clone();
            entry::guard(|| {
                ctx.update(move |app: &mut App| {
                    let Some(w) = &app.settings else { return };
                    let name = usize::try_from(index)
                        .ok()
                        .and_then(|i| c.page.borrow().font_listing.get(i).cloned());
                    let Some(name) = name else { return };
                    let a = w.global::<Appearance>();
                    a.set_font_picked(name.into());
                    a.set_font_picked_serial(a.get_font_picked_serial() + 1);
                    w.global::<Overlays>().set_picker_open(false);
                })
            });
        });
    }
    let ctx = ctx.clone();
    o.on_picker_cancelled(move || {
        entry::guard(|| {
            ctx.update(|app: &mut App| {
                if let Some(w) = &app.settings {
                    w.global::<Overlays>().set_picker_open(false);
                }
            });
        });
    });
}
