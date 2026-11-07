//! Shortcuts in the shell (ADR-0060, `docs/specs/shortcuts.md`): the keymap in force,
//! app-scope key presses, global shortcuts through the OS, and Settings › Shortcuts.
//!
//! The state lives in [`Keys`], apart from the app's state, so a key press looks its
//! chord up without borrowing the app. Commands run through `App::run_command` (app
//! scope, with the menu's confirmations) or `App::run_command_as` with the confirmation
//! left out (global scope, a control surface).

use rbs_app::hotkeys::{self, GlobalKeys, Presses, Read};
use rbs_ui::command::{self, CommandId, CommandState, ENTRIES, Kind, Menu, ParamType};
use rbs_ui::shortcut::{Check, Chord, Key, Keymap, Mods, Named, Platform, Scope, ShortcutsDoc};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{App, CTX, Ctx, entry};
use crate::ui::{ShortcutChip, ShortcutRow, Shortcuts};

/// The recorder of Settings › Shortcuts.
#[derive(Debug, Clone)]
struct Recording {
    /// The command it adds to.
    command: String,
    mods: Mods,
    key: Option<Key>,
    global: bool,
    /// A key press that is not a chord: why.
    unusable: Option<&'static str>,
}

/// One row of the page: a command or a picker choice.
#[derive(Debug, Clone, PartialEq)]
struct RowSrc {
    group: &'static str,
    command: String,
    title: String,
    /// A bound choice that no longer exists, or a command this build does not have.
    missing: Option<&'static str>,
}

/// Shortcut state, shared by every window.
pub struct Keys {
    platform: Platform,
    doc: ShortcutsDoc,
    keymap: Keymap,
    /// The OS's global shortcuts; only while there are global bindings (P13).
    global: Option<Box<dyn GlobalKeys>>,
    /// The ids the OS reports, with their commands.
    global_ids: Vec<(u32, String)>,
    /// Global chords the OS refused, with why.
    refused: Vec<(String, Chord, String)>,
    presses: Presses,
    /// The page: search text, the rows shown, the recorder, Reset all asked.
    search: String,
    rows: Vec<RowSrc>,
    recording: Option<Recording>,
    confirm_reset_all: bool,
    /// The file could not be read or written, or global shortcuts could not start.
    note: String,
}

impl Default for Keys {
    fn default() -> Self {
        let platform = Platform::current();
        let doc = ShortcutsDoc::default();
        Keys {
            platform,
            keymap: Keymap::resolve(&doc, platform),
            doc,
            global: None,
            global_ids: Vec::new(),
            refused: Vec::new(),
            presses: Presses::default(),
            search: String::new(),
            rows: Vec::new(),
            recording: None,
            confirm_reset_all: false,
            note: String::new(),
        }
    }
}

/// Every key the key picker offers, in its order.
fn all_keys() -> Vec<Key> {
    let mut keys: Vec<Key> = (b'A'..=b'Z').map(Key::Letter).collect();
    keys.extend((0..=9).map(Key::Digit));
    keys.extend((1..=24).map(Key::F));
    keys.extend(Named::ALL.map(Key::Named));
    keys
}

fn key_label(k: Key, platform: Platform) -> String {
    Chord {
        mods: Mods::NONE,
        key: k,
    }
    .label(platform)
}

/// Modifier names in the order of `Mods` (Ctrl, Alt, Shift, Meta) as this platform
/// calls them.
fn mod_names(platform: Platform) -> [&'static str; 4] {
    match platform {
        Platform::MacOs => ["Command", "Option", "Shift", "Control"],
        Platform::Windows => ["Ctrl", "Alt", "Shift", "Win"],
        Platform::Linux => ["Ctrl", "Alt", "Shift", "Super"],
    }
}

fn hint(platform: Platform) -> String {
    let mut s = String::from(
        "Shortcuts run commands while an RBS window is in front. Global shortcuts also work while another app is in front, or RBS has no window open; they start and stop outputs without asking.",
    );
    match platform {
        Platform::MacOs => s.push_str(
            " System shortcuts win over RBS, and global shortcuts pause while another app asks for a password. Letters of global shortcuts are the keys of a US keyboard.",
        ),
        Platform::Windows => {}
        Platform::Linux => s.push_str(" Global shortcuts are not available on Linux yet."),
    }
    s
}

impl Keys {
    /// Uses `doc`, read at start or after a change.
    fn set_doc(&mut self, doc: ShortcutsDoc) {
        self.keymap = Keymap::resolve(&doc, self.platform);
        self.doc = doc;
        self.sync_global();
    }

    /// Registers the keymap's global chords with the OS, or drops the OS's manager when
    /// there are none. While the recorder runs, none are registered.
    fn sync_global(&mut self) {
        let mut chords = Vec::new();
        let mut commands = Vec::new();
        for b in self.keymap.global() {
            chords.push(b.chord);
            commands.push(b.command.clone());
        }
        self.global_ids.clear();
        self.refused.clear();
        self.presses.clear();
        if chords.is_empty() {
            self.global = None;
            return;
        }
        if self.recording.is_some() {
            if let Some(g) = &mut self.global {
                g.set(&[]);
            }
            return;
        }
        if self.global.is_none() {
            match hotkeys::system(on_global_event) {
                Ok(g) => self.global = Some(g),
                Err(why) => {
                    for (chord, command) in chords.into_iter().zip(commands) {
                        self.refused.push((command, chord, format!("{why}.")));
                    }
                    return;
                }
            }
        }
        let Some(g) = &mut self.global else { return };
        for ((result, chord), command) in g.set(&chords).into_iter().zip(chords).zip(commands) {
            match result {
                Ok(id) => self.global_ids.push((id, command)),
                Err(r) => self.refused.push((command, chord, r.text())),
            }
        }
    }

    /// The command an app-scope key press runs; `None` when the key is not bound.
    fn app_command(&self, text: &str, mods: Mods) -> Option<String> {
        match hotkeys::read_key(text, mods) {
            Read::Chord(c) => self.keymap.app(&c).map(str::to_owned),
            _ => None,
        }
    }

    fn chord_of(&self, r: &Recording) -> Option<Result<Chord, String>> {
        if let Some(why) = r.unusable {
            return Some(Err(format!("{why}. Pick the keys below instead.")));
        }
        let key = r.key?;
        Some(Chord::new(r.mods, key).map_err(|e| format!("{e}.")))
    }

    /// The recorder's state for the page: label, state code, message.
    fn recorder_view(&self, title: &dyn Fn(&str) -> String) -> (String, i32, String) {
        let Some(r) = &self.recording else {
            return (String::new(), 0, String::new());
        };
        let chord = match self.chord_of(r) {
            None => return (String::new(), 0, String::new()),
            Some(Err(why)) => return (String::new(), 3, why),
            Some(Ok(c)) => c,
        };
        let label = chord.label(self.platform);
        let scope = if r.global { Scope::Global } else { Scope::App };
        let (state, message) = match self.keymap.check(&chord, scope) {
            Check::Free => (1, String::new()),
            Check::Warn(w) => (4, w),
            Check::Refused(why) => (3, why),
            Check::Taken { command, .. } if command == r.command => {
                (3, format!("{label} is already a shortcut of this command."))
            }
            Check::Taken { command, .. } => (
                2,
                format!(
                    "{label} is used by {}. Replace moves it here.",
                    title(&command)
                ),
            ),
        };
        (label, state, message)
    }
}

/// A global chord was pressed or released, on whatever thread the OS reports it.
fn on_global_event(id: u32, pressed: bool) {
    let _ = entry::invoke(move || {
        let Some(ctx) = CTX.with(|c| c.borrow().clone()) else {
            return;
        };
        let command = {
            let Ok(mut keys) = ctx.keys.try_borrow_mut() else {
                return;
            };
            if !pressed {
                keys.presses.released(id);
                return;
            }
            if !keys.presses.pressed(id) {
                return;
            }
            keys.global_ids
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, c)| c.clone())
        };
        if let Some(command) = command {
            // A control surface: no local confirmation (ADR-0060 decision 7).
            ctx.update(move |app| app.run_command_as(&command, false));
        }
    });
}

/// Reads `shortcuts.toml` and starts global shortcuts if any are bound. At start.
pub fn load(ctx: &Ctx, app: &App) {
    let (doc, err) = app.studio.user.data().shortcuts();
    let mut keys = ctx.keys.borrow_mut();
    if let Some(e) = err {
        keys.note = format!(
            "The shortcuts file could not be read ({e}); the default shortcuts are used until you change one."
        );
    }
    keys.set_doc(doc);
}

/// Connects a main or floating window's key presses.
pub fn install_press(g: &Shortcuts<'_>, ctx: &Ctx) {
    let ctx = ctx.clone();
    g.on_press(move |text, ctrl, alt, shift, meta, repeat| {
        entry::guard_or(
            || false,
            || {
                let mods = Mods {
                    ctrl,
                    alt,
                    shift,
                    meta,
                };
                let Some(command) = ctx
                    .keys
                    .try_borrow()
                    .ok()
                    .and_then(|k| k.app_command(&text, mods))
                else {
                    return false;
                };
                // A held key runs its command once.
                if !repeat {
                    ctx.update(move |app| app.run_command(&command));
                }
                true
            },
        )
    });
}

/// The rows of the page: every command by menu, picker choices and parameter values
/// as rows of their own, then bound commands that have no row.
fn rows(keys: &Keys, state: &dyn CommandState) -> Vec<RowSrc> {
    let mut out: Vec<RowSrc> = Vec::new();
    for menu in Menu::ALL {
        for e in ENTRIES.iter().filter(|e| e.menu == menu) {
            let group = menu.title();
            if let Kind::Picker { param, .. } = e.kind {
                let Ok(bare) = CommandId::plain(e.id) else {
                    continue;
                };
                for (title, value, _) in state.choices(&bare) {
                    if let Ok(id) = CommandId::with_params(e.id, &[(param, &value)]) {
                        out.push(RowSrc {
                            group,
                            command: id.to_string(),
                            title: format!("{}: {title}", e.title),
                            missing: None,
                        });
                    }
                }
                continue;
            }
            let required = e.params.iter().find(|p| p.required);
            match required.map(|p| (p.name, p.ty)) {
                Some((name, ParamType::OneOf(values))) => {
                    for v in values {
                        if let Ok(id) = CommandId::with_params(e.id, &[(name, v)]) {
                            out.push(RowSrc {
                                group,
                                title: command::title(&id, e),
                                command: id.to_string(),
                                missing: None,
                            });
                        }
                    }
                }
                Some(_) => {}
                None => out.push(RowSrc {
                    group,
                    command: e.id.to_owned(),
                    title: e.title.to_owned(),
                    missing: None,
                }),
            }
        }
    }
    // Bound commands without a row: a choice that is gone, a plugin that is missing.
    let bound = keys
        .keymap
        .bindings()
        .iter()
        .map(|b| b.command.clone())
        .chain(keys.keymap.problems().iter().map(|p| p.command.clone()));
    for command in bound {
        if out.iter().any(|r| r.command == command) {
            continue;
        }
        let resolved = CommandId::parse(&command)
            .ok()
            .and_then(|id| match id.resolve() {
                Ok(Some(e)) => Some((id, e)),
                _ => None,
            });
        let row = match resolved {
            Some((id, e)) => RowSrc {
                group: e.menu.title(),
                title: match e.kind {
                    Kind::Picker { param, .. } => {
                        format!("{}: {}", e.title, id.param(param).unwrap_or(""))
                    }
                    _ => command::title(&id, e),
                },
                command,
                missing: Some("Not found: this choice does not exist now."),
            },
            None => RowSrc {
                group: "Other",
                title: command.clone(),
                command,
                missing: Some("Not available: no command has this id in this version."),
            },
        };
        out.push(row);
    }
    // Keep groups together (missing rows were appended at the end).
    let order = |g: &str| {
        Menu::ALL
            .iter()
            .position(|m| m.title() == g)
            .unwrap_or(Menu::ALL.len())
    };
    out.sort_by_key(|r| order(r.group));
    out
}

fn title_of(rows: &[RowSrc], command: &str) -> String {
    rows.iter()
        .find(|r| r.command == command)
        .map(|r| r.title.clone())
        .unwrap_or_else(|| command.to_owned())
}

/// Builds the rows and pushes the whole page into the Settings window, if it is open.
pub fn refresh(ctx: &Ctx, app: &App) {
    let Some(w) = &app.settings else { return };
    let mut keys = ctx.keys.borrow_mut();
    let all = rows(&keys, &app.shown());
    let platform = keys.platform;
    let needle = keys.search.trim().to_lowercase();
    let mut shown: Vec<RowSrc> = Vec::new();
    let mut view: Vec<ShortcutRow> = Vec::new();
    let mut last_group = "";
    for r in &all {
        let chips: Vec<ShortcutChip> = keys
            .keymap
            .of(&r.command)
            .map(|b| ShortcutChip {
                label: b.chord.label(platform).into(),
                keys: b.chord.to_string().into(),
                global: b.scope == Scope::Global,
            })
            .collect();
        let recording_here = keys
            .recording
            .as_ref()
            .is_some_and(|rec| rec.command == r.command);
        if !needle.is_empty() && !recording_here {
            let hit = r.title.to_lowercase().contains(&needle)
                || r.group.to_lowercase().contains(&needle)
                || chips.iter().any(|c| {
                    c.label.to_lowercase().contains(&needle)
                        || c.keys.to_lowercase().contains(&needle)
                });
            if !hit {
                continue;
            }
        }
        let mut problems: Vec<String> = keys
            .keymap
            .problems()
            .iter()
            .filter(|p| p.command == r.command)
            .map(|p| {
                format!(
                    "{}: {}",
                    p.chord.label(platform),
                    p.kind.text(&|c| title_of(&all, c))
                )
            })
            .collect();
        problems.extend(
            keys.refused
                .iter()
                .filter(|(c, _, _)| *c == r.command)
                .map(|(_, chord, why)| format!("{}: {why}", chord.label(platform))),
        );
        if let Some(m) = r.missing {
            problems.insert(0, m.to_owned());
        }
        let global = keys.keymap.of(&r.command).any(|b| b.scope == Scope::Global);
        let note = match (global, r.command.as_str()) {
            (true, "stream.toggle") => {
                "Its global shortcut starts or ends the stream without asking."
            }
            (true, "record.toggle") => {
                "Its global shortcut starts or stops the recording without asking."
            }
            _ => "",
        };
        view.push(ShortcutRow {
            group: if r.group != last_group {
                r.group.to_uppercase().into()
            } else {
                SharedString::new()
            },
            command: r.command.clone().into(),
            title: r.title.clone().into(),
            chips: ModelRc::new(VecModel::from(chips)),
            changed: keys.doc.changed(&r.command),
            problem: problems.join("\n").into(),
            note: note.into(),
        });
        last_group = r.group;
        shown.push(r.clone());
    }
    let recording_row = keys.recording.as_ref().and_then(|rec| {
        shown
            .iter()
            .position(|r| r.command == rec.command)
            .map(|i| i as i32)
    });
    let (rec_label, rec_state, rec_message) = keys.recorder_view(&|c| title_of(&all, c));
    keys.rows = shown;
    let g = w.global::<Shortcuts>();
    g.set_rows(ModelRc::new(VecModel::from(view)));
    g.set_hint(hint(platform).into());
    g.set_note(keys.note.clone().into());
    g.set_confirm_reset_all(keys.confirm_reset_all);
    g.set_recording(recording_row.unwrap_or(-1));
    g.set_rec_label(rec_label.into());
    g.set_rec_state(rec_state);
    g.set_rec_message(rec_message.into());
    g.set_rec_global_available(platform.has_global());
    let names = mod_names(platform);
    g.set_mod_names(ModelRc::new(VecModel::from(
        names.map(SharedString::from).to_vec(),
    )));
    let all_keys = all_keys();
    g.set_key_names(ModelRc::new(VecModel::from(
        all_keys
            .iter()
            .map(|k| SharedString::from(key_label(*k, platform)))
            .collect::<Vec<_>>(),
    )));
    if let Some(rec) = &keys.recording {
        g.set_rec_global(rec.global);
        g.set_rec_mods(ModelRc::new(VecModel::from(vec![
            rec.mods.ctrl,
            rec.mods.alt,
            rec.mods.shift,
            rec.mods.meta,
        ])));
        let index = rec
            .key
            .and_then(|k| all_keys.iter().position(|x| *x == k))
            .map_or(-1, |i| i as i32);
        g.set_rec_key(index);
    }
}

/// Saves the document after a change and applies it.
fn changed(ctx: &Ctx, app: &mut App, doc: ShortcutsDoc) {
    {
        let mut keys = ctx.keys.borrow_mut();
        keys.note.clear();
        if let Err(why) = app.studio.user.set_shortcuts(&doc) {
            keys.note =
                format!("The shortcuts could not be saved ({why}); they apply until RBS quits.");
        }
        keys.set_doc(doc);
    }
    refresh(ctx, app);
}

/// Ends the recorder and registers global chords again.
fn stop_recording(ctx: &Ctx) {
    let mut keys = ctx.keys.borrow_mut();
    if keys.recording.take().is_some() {
        keys.sync_global();
    }
}

/// Connects the Settings window's `Shortcuts` global.
pub fn install(g: &Shortcuts<'_>, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$app:ident, $c:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let ctx = ctx.clone();
            g.$name(move |$($arg: $ty),*| {
                let $c = ctx.clone();
                ctx.update(move |$app: &mut App| {
                    $body
                    refresh(&$c, $app);
                });
            });
        }};
    }
    on!(on_set_search, |app, c, text: SharedString| {
        let _ = app;
        c.keys.borrow_mut().search = text.to_string();
    });
    on!(on_ask_reset_all, |app, c| {
        let _ = app;
        c.keys.borrow_mut().confirm_reset_all = true;
    });
    on!(on_answer_reset_all, |app, c, yes: bool| {
        c.keys.borrow_mut().confirm_reset_all = false;
        if yes {
            stop_recording(&c);
            let mut doc = c.keys.borrow().doc.clone();
            doc.reset_all();
            changed(&c, app, doc);
        }
    });
    on!(on_remove, |app, c, row: i32, chip: i32| {
        let doc = {
            let keys = c.keys.borrow();
            let Some(r) = usize::try_from(row).ok().and_then(|i| keys.rows.get(i)) else {
                return;
            };
            let Some(b) = usize::try_from(chip)
                .ok()
                .and_then(|i| keys.keymap.of(&r.command).nth(i))
            else {
                return;
            };
            let mut doc = keys.doc.clone();
            doc.remove(&r.command, b.chord);
            doc
        };
        changed(&c, app, doc);
    });
    on!(on_reset, |app, c, row: i32| {
        let doc = {
            let keys = c.keys.borrow();
            let Some(r) = usize::try_from(row).ok().and_then(|i| keys.rows.get(i)) else {
                return;
            };
            let mut doc = keys.doc.clone();
            doc.reset(&r.command);
            doc
        };
        changed(&c, app, doc);
    });
    on!(on_add, |app, c, row: i32| {
        let _ = app;
        let mut keys = c.keys.borrow_mut();
        let Some(r) = usize::try_from(row).ok().and_then(|i| keys.rows.get(i)) else {
            return;
        };
        keys.recording = Some(Recording {
            command: r.command.clone(),
            mods: Mods::NONE,
            key: None,
            global: false,
            unusable: None,
        });
        // A chord bound globally can be recorded again while the recorder is open.
        keys.sync_global();
    });
    on!(on_rec_set_mod, |app, c, i: i32, on: bool| {
        let _ = app;
        if let Some(r) = c.keys.borrow_mut().recording.as_mut() {
            r.unusable = None;
            match i {
                0 => r.mods.ctrl = on,
                1 => r.mods.alt = on,
                2 => r.mods.shift = on,
                3 => r.mods.meta = on,
                _ => {}
            }
        }
    });
    on!(on_rec_set_key, |app, c, i: i32| {
        let _ = app;
        if let Some(r) = c.keys.borrow_mut().recording.as_mut() {
            r.unusable = None;
            r.key = usize::try_from(i)
                .ok()
                .and_then(|i| all_keys().get(i).copied());
        }
    });
    on!(on_rec_set_global, |app, c, on: bool| {
        let _ = app;
        if let Some(r) = c.keys.borrow_mut().recording.as_mut() {
            r.global = on;
        }
    });
    on!(on_rec_cancel, |app, c| {
        let _ = app;
        stop_recording(&c);
    });
    on!(on_rec_confirm, |app, c| {
        let doc = {
            let keys = c.keys.borrow();
            let Some(r) = keys.recording.clone() else {
                return;
            };
            let Some(Ok(chord)) = keys.chord_of(&r) else {
                return;
            };
            let scope = if r.global { Scope::Global } else { Scope::App };
            let mut doc = keys.doc.clone();
            match keys.keymap.check(&chord, scope) {
                Check::Free | Check::Warn(_) => doc.add(&r.command, chord, scope),
                Check::Taken { command, .. } if command != r.command => {
                    doc.replace(&keys.keymap, &r.command, chord, scope);
                }
                _ => return,
            }
            doc
        };
        c.keys.borrow_mut().recording = None;
        changed(&c, app, doc);
    });
    {
        let ctx = ctx.clone();
        g.on_rec_press(move |text, ctrl, alt, shift, meta| {
            let mods = Mods {
                ctrl,
                alt,
                shift,
                meta,
            };
            let c = ctx.clone();
            ctx.update(move |app| {
                if let Some(r) = c.keys.borrow_mut().recording.as_mut() {
                    match hotkeys::read_key(&text, mods) {
                        Read::Modifier => {
                            r.mods = mods;
                            r.key = None;
                            r.unusable = None;
                        }
                        Read::Chord(chord) => {
                            r.mods = chord.mods;
                            r.key = Some(chord.key);
                            r.unusable = None;
                        }
                        Read::Unusable(why) => {
                            r.mods = mods;
                            r.key = None;
                            r.unusable = Some(why);
                        }
                    }
                }
                refresh(&c, app);
            });
            true
        });
    }
}

/// Settings › Shortcuts was shown.
pub fn opened(ctx: &Ctx, app: &App) {
    refresh(ctx, app);
}

/// The Settings window closed: the recorder ends.
pub fn closed(ctx: &Ctx) {
    stop_recording(ctx);
    let mut keys = ctx.keys.borrow_mut();
    keys.confirm_reset_all = false;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use rbs_app::hotkeys::Refusal;

    use super::*;

    struct State;

    impl CommandState for State {
        fn enabled(&self, _: &CommandId) -> Result<(), String> {
            Ok(())
        }
        fn checked(&self, _: &CommandId) -> bool {
            false
        }
        fn choices(&self, id: &CommandId) -> Vec<(String, String, bool)> {
            match id.base() {
                "ui.workspace.select" => vec![
                    ("Edit".into(), "Edit".into(), true),
                    ("Live show".into(), "Live show".into(), false),
                ],
                _ => Vec::new(),
            }
        }
    }

    fn keys_with(doc: ShortcutsDoc) -> Keys {
        Keys {
            keymap: Keymap::resolve(&doc, Platform::Windows),
            platform: Platform::Windows,
            doc,
            ..Keys::default()
        }
    }

    #[test]
    fn rows_cover_commands_choices_and_missing_bindings() {
        let mut doc = ShortcutsDoc::default();
        doc.add(
            "ui.workspace.select?name=Gone",
            Chord::parse("Ctrl+G").unwrap(),
            Scope::App,
        );
        doc.add(
            "com.example.thing",
            Chord::parse("Ctrl+J").unwrap(),
            Scope::App,
        );
        let k = keys_with(doc);
        let r = rows(&k, &State);
        let find = |c: &str| r.iter().find(|x| x.command == c).unwrap();
        assert_eq!(find("edit.undo").group, "Edit");
        assert_eq!(
            find("ui.workspace.select?name=Live%20show").title,
            "Workspace: Live show"
        );
        assert_eq!(find("ui.area.toggle?area=left").title, "Left area");
        assert!(find("ui.workspace.select?name=Gone").missing.is_some());
        assert_eq!(find("com.example.thing").group, "Other");
        // Groups stay together, in menu order.
        let groups: Vec<&str> = r.iter().map(|x| x.group).collect();
        let mut dedup = groups.clone();
        dedup.dedup();
        let mut sorted = dedup.clone();
        sorted.sort_by_key(|g| Menu::ALL.iter().position(|m| m.title() == *g).unwrap_or(99));
        assert_eq!(dedup, sorted);
        assert_eq!(dedup.len(), {
            let mut u = dedup.clone();
            u.sort();
            u.dedup();
            u.len()
        });
    }

    #[test]
    fn app_presses_find_their_command() {
        let k = keys_with(ShortcutsDoc::default());
        assert_eq!(k.app_command("z", Mods::CTRL).as_deref(), Some("edit.undo"));
        assert_eq!(k.app_command("y", Mods::CTRL).as_deref(), Some("edit.redo"));
        assert_eq!(k.app_command("k", Mods::CTRL), None);
        let shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        assert_eq!(k.app_command("Z", shift).as_deref(), Some("edit.redo"));
        // A global binding is not run in app scope.
        let mut doc = ShortcutsDoc::default();
        doc.add(
            "record.toggle",
            Chord::parse("Ctrl+R").unwrap(),
            Scope::Global,
        );
        let k = keys_with(doc);
        assert_eq!(k.app_command("r", Mods::CTRL), None);
    }

    #[test]
    fn the_recorder_checks_as_it_goes() {
        let mut k = keys_with(ShortcutsDoc::default());
        let title = |c: &str| c.to_owned();
        k.recording = Some(Recording {
            command: "stream.toggle".into(),
            mods: Mods::CTRL,
            key: Some(Key::Letter(b'Z')),
            global: false,
            unusable: None,
        });
        let (label, state, msg) = k.recorder_view(&title);
        assert_eq!((label.as_str(), state), ("Ctrl+Z", 2));
        assert!(msg.contains("edit.undo"), "{msg}");
        if let Some(r) = k.recording.as_mut() {
            r.key = Some(Key::Letter(b'S'));
            r.global = true;
        }
        assert_eq!(k.recorder_view(&title).1, 4);
        if let Some(r) = k.recording.as_mut() {
            r.mods = Mods::NONE;
        }
        assert_eq!(k.recorder_view(&title).1, 3);
        if let Some(r) = k.recording.as_mut() {
            r.unusable = Some("nope");
        }
        assert_eq!(k.recorder_view(&title).1, 3);
    }

    /// With a fake OS: nothing is created without global bindings, refused chords are
    /// listed, and the recorder unregisters every chord until it closes.
    #[test]
    fn global_chords_follow_the_keymap() {
        let mut k = keys_with(ShortcutsDoc::default());
        k.sync_global();
        assert!(k.global.is_none());

        struct Fake(Rc<RefCell<Vec<Vec<Chord>>>>);
        impl GlobalKeys for Fake {
            fn set(&mut self, chords: &[Chord]) -> Vec<Result<u32, Refusal>> {
                self.0.borrow_mut().push(chords.to_vec());
                chords
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        if c.key == Key::F(14) {
                            Err(Refusal::TakenByOtherApp)
                        } else {
                            Ok(i as u32 + 100)
                        }
                    })
                    .collect()
            }
        }
        let calls = Rc::new(RefCell::new(Vec::new()));
        k.global = Some(Box::new(Fake(calls.clone())));
        let mut doc = ShortcutsDoc::default();
        doc.add("record.toggle", Chord::parse("F13").unwrap(), Scope::Global);
        doc.add("stream.toggle", Chord::parse("F14").unwrap(), Scope::Global);
        k.set_doc(doc.clone());
        assert_eq!(k.global_ids, vec![(100, "record.toggle".to_owned())]);
        assert_eq!(k.refused.len(), 1);
        assert_eq!(k.refused[0].0, "stream.toggle");
        k.recording = Some(Recording {
            command: "stream.toggle".into(),
            mods: Mods::NONE,
            key: None,
            global: true,
            unusable: None,
        });
        k.sync_global();
        assert_eq!(calls.borrow().last().unwrap().len(), 0);
        assert!(k.global_ids.is_empty());
        k.recording = None;
        k.sync_global();
        assert_eq!(calls.borrow().last().unwrap().len(), 2);
        doc.reset_all();
        k.set_doc(doc);
        assert!(k.global.is_none(), "dropped with the last global binding");
    }
}
