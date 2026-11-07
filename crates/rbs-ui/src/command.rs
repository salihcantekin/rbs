//! The command registry (ADR-0050): every command the desktop UI offers, once, with
//! the control API methods it runs. The menu bar, context menus, toolbar, command
//! palette and shortcut map are built from it.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Commands, menus and the toolbar".
//!
//! An entry runs control API methods only (U20): engine methods (`stream.start`) or
//! client-scoped `ui.*` methods. Whether an entry is enabled, why not, whether a
//! toggle is on and which choices a picker offers is the shell's state; it answers
//! through [`CommandState`], so this crate stays free of engine and toolkit types.
//!
//! Not yet: titles and tooltips are English text until the UI has Fluent messages
//! (ADR-0050 decision 1). Icons name the design system's icon set; an area toggle's
//! icon depends on its area (see [`icon`]).

use std::fmt;

use crate::panel::{PanelId, built_in};
use crate::workspace::{Toolbar, ToolbarItem, ToolbarStyle};

/// Longest command id in bytes, counting the percent-encoded form.
pub const MAX_COMMAND_ID_LEN: usize = 128;

/// The menus of the menu bar, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Menu {
    /// File.
    File,
    /// Edit.
    Edit,
    /// View.
    View,
    /// Scene.
    Scene,
    /// Source.
    Source,
    /// Output.
    Output,
    /// Tools.
    Tools,
    /// Window.
    Window,
    /// Help.
    Help,
}

impl Menu {
    /// Every menu, in menu bar order.
    pub const ALL: [Menu; 9] = [
        Menu::File,
        Menu::Edit,
        Menu::View,
        Menu::Scene,
        Menu::Source,
        Menu::Output,
        Menu::Tools,
        Menu::Window,
        Menu::Help,
    ];

    /// The menu's title.
    pub fn title(self) -> &'static str {
        match self {
            Menu::File => "File",
            Menu::Edit => "Edit",
            Menu::View => "View",
            Menu::Scene => "Scene",
            Menu::Source => "Source",
            Menu::Output => "Output",
            Menu::Tools => "Tools",
            Menu::Window => "Window",
            Menu::Help => "Help",
        }
    }
}

/// The type of a command parameter, used to check an id's parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    /// Any text.
    Text,
    /// One of these values.
    OneOf(&'static [&'static str]),
}

/// One parameter of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param {
    /// Its key in ids and in the method's parameters.
    pub name: &'static str,
    /// Its type.
    pub ty: ParamType,
    /// An id without it cannot run (it is kept and shown disabled with the reason).
    pub required: bool,
}

/// What an entry runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One control API method, with the id's parameters.
    Action {
        /// The method.
        method: &'static str,
    },
    /// Two methods, for on and off; the shell's state says which applies
    /// ([`CommandState::checked`]). The off half keeps its confirmation in the UI
    /// (End stream asks while live).
    Toggle {
        /// Runs when the toggle is off.
        on: &'static str,
        /// Runs when the toggle is on.
        off: &'static str,
    },
    /// A list of choices ([`CommandState::choices`]); each runs `method` with
    /// `param` set to the choice's value. The picker's id with that parameter
    /// (`ui.workspace.select?name=Live`) runs the choice directly; without it there is
    /// nothing to run (`ui.command.run` answers `invalid_params`).
    Picker {
        /// The method each choice runs.
        method: &'static str,
        /// The parameter the choice fills.
        param: &'static str,
    },
}

impl Kind {
    /// Every control API method the entry can run.
    pub fn methods(self) -> Vec<&'static str> {
        match self {
            Kind::Action { method } | Kind::Picker { method, .. } => vec![method],
            Kind::Toggle { on, off } => vec![on, off],
        }
    }
}

/// One registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The id without parameters (grammar of [`CommandId`]).
    pub id: &'static str,
    /// Title in menus, the palette and toolbar buttons with text.
    pub title: &'static str,
    /// Longer text for tooltips and the palette.
    pub tooltip: &'static str,
    /// Icon name from the icon set, if any.
    pub icon: Option<&'static str>,
    /// Default shortcut, `Ctrl` meaning Cmd on macOS (for example `Ctrl+Shift+Z`).
    pub shortcut: Option<&'static str>,
    /// The menu it is in. Every entry is in exactly one menu.
    pub menu: Menu,
    /// What it runs.
    pub kind: Kind,
    /// Its parameters; an id may set each once.
    pub params: &'static [Param],
    /// Shows a checkmark: toggles, and actions with an on or off state (area toggles).
    pub checkable: bool,
}

const AREA: Param = Param {
    name: "area",
    ty: ParamType::OneOf(&["left", "right", "bottom"]),
    required: true,
};

/// A picker's choice: optional, because the bare picker id opens the list.
const fn choice(name: &'static str) -> Param {
    Param {
        name,
        ty: ParamType::Text,
        required: false,
    }
}

const fn action(method: &'static str) -> Kind {
    Kind::Action { method }
}

/// The built-in entries, by menu. Ids are a public contract: renaming one keeps the
/// old id as an alias for at least one major version (ADR-0050 decision 2).
pub const ENTRIES: &[Entry] = &[
    Entry {
        id: "profile.select",
        title: "Output profile",
        tooltip: "Use another output profile: its recording and stream settings",
        icon: None,
        shortcut: None,
        menu: Menu::File,
        kind: Kind::Picker {
            method: "profile.switch",
            param: "id",
        },
        params: &[choice("id")],
        checkable: false,
    },
    Entry {
        id: "collection.select",
        title: "Scene collection",
        tooltip: "Run another scene collection: its canvases, scenes and sources",
        icon: None,
        shortcut: None,
        menu: Menu::File,
        kind: Kind::Picker {
            method: "collection.switch",
            param: "id",
        },
        params: &[choice("id")],
        checkable: false,
    },
    Entry {
        id: "edit.undo",
        title: "Undo",
        tooltip: "Undo the last change to the scenes",
        icon: None,
        shortcut: Some("Ctrl+Z"),
        menu: Menu::Edit,
        kind: action("edit.undo"),
        params: &[],
        checkable: false,
    },
    Entry {
        id: "edit.redo",
        title: "Redo",
        tooltip: "Redo the last undone change to the scenes",
        icon: None,
        shortcut: Some("Ctrl+Shift+Z"),
        menu: Menu::Edit,
        kind: action("edit.redo"),
        params: &[],
        checkable: false,
    },
    Entry {
        id: "ui.workspace.select",
        title: "Workspace",
        tooltip: "Switch to another workspace",
        icon: None,
        shortcut: None,
        menu: Menu::View,
        kind: Kind::Picker {
            method: "ui.workspace.switch",
            param: "name",
        },
        params: &[choice("name")],
        checkable: false,
    },
    Entry {
        id: "ui.area.toggle",
        title: "Area",
        tooltip: "Show or hide an area of the window",
        icon: None,
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.area.toggle"),
        params: &[AREA],
        checkable: true,
    },
    Entry {
        id: "ui.area.hide_all",
        title: "Hide side panels",
        tooltip: "Hide the left, right and bottom areas",
        icon: None,
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.area.hide_all"),
        params: &[],
        checkable: false,
    },
    Entry {
        id: "ui.area.show_all",
        title: "Show side panels",
        tooltip: "Show the left, right and bottom areas",
        icon: None,
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.area.show_all"),
        params: &[],
        checkable: false,
    },
    Entry {
        id: "ui.layout.reset_sizes",
        title: "Reset panel sizes",
        tooltip: "Give every area and panel its default size",
        icon: None,
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.layout.reset_sizes"),
        params: &[],
        checkable: false,
    },
    Entry {
        id: "ui.toolbar.reset",
        title: "Reset toolbar",
        tooltip: "Put the workspace's default toolbar back",
        icon: None,
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.toolbar.reset"),
        params: &[],
        checkable: false,
    },
    Entry {
        id: "ui.panel.select",
        title: "Panels",
        tooltip: "Open a panel, or show it if it is open",
        icon: None,
        shortcut: None,
        menu: Menu::Window,
        kind: Kind::Picker {
            method: "ui.panel.open",
            param: "id",
        },
        params: &[choice("id")],
        checkable: false,
    },
    Entry {
        id: "ui.panel.maximize",
        title: "Maximize",
        tooltip: "Show a panel over the whole window, or restore the layout",
        icon: Some("expand"),
        shortcut: None,
        menu: Menu::Window,
        kind: Kind::Toggle {
            on: "ui.panel.maximize",
            off: "ui.panel.restore",
        },
        params: &[Param {
            name: "id",
            ty: ParamType::Text,
            required: true,
        }],
        checkable: true,
    },
    Entry {
        id: "source.add",
        title: "Add source",
        tooltip: "Add a screen, window, camera or audio source to the scene",
        icon: Some("plus"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "source.add",
            param: "kind",
        },
        params: &[choice("kind")],
        checkable: false,
    },
    Entry {
        id: "canvas.studio_mode.toggle",
        title: "Studio mode",
        tooltip: "Prepare the next scene in Preview while Program stays on air",
        icon: Some("copy"),
        shortcut: None,
        menu: Menu::Scene,
        kind: Kind::Toggle {
            on: "canvas.set_preview",
            off: "canvas.set_preview",
        },
        params: &[],
        checkable: true,
    },
    Entry {
        id: "canvas.transition",
        title: "Transition",
        tooltip: "Put the preview scene on air (studio mode)",
        icon: None,
        shortcut: Some("Ctrl+Enter"),
        menu: Menu::Scene,
        kind: action("canvas.transition"),
        params: &[],
        checkable: false,
    },
    Entry {
        id: "stream.toggle",
        title: "Go live",
        tooltip: "Start or end the stream",
        icon: Some("bolt"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "stream.start",
            off: "stream.stop",
        },
        params: &[],
        checkable: true,
    },
    Entry {
        id: "record.toggle",
        title: "Record",
        tooltip: "Start or stop recording",
        icon: Some("dot"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "record.start",
            off: "record.stop",
        },
        params: &[],
        checkable: true,
    },
];

/// The entry with id `base` (no parameters).
pub fn entry(base: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|e| e.id == base)
}

/// Why a command id is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandIdError(pub String);

impl fmt::Display for CommandIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CommandIdError {}

/// A command id with its parameters: `ui.area.toggle?area=left`. The panel id grammar
/// (ADR-0017), with parameter values UTF-8, percent-encoded outside the id characters
/// and ASCII capitals (`ui.workspace.switch?name=Live%20show`; `%` escapes are written
/// in capitals). Parameters are stored sorted
/// by key, so one command has one id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommandId {
    base: String,
    params: Vec<(String, String)>,
}

fn id_char(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
}

fn valid_word(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(id_char)
}

/// Bytes a parameter value holds unencoded: the id characters and ASCII capitals
/// (`name=Live%20show`). Everything else is percent-encoded.
fn value_char(b: u8) -> bool {
    id_char(b) || b.is_ascii_uppercase()
}

fn decode(v: &str) -> Option<String> {
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                // Two hex digits; lower case is read too and written in capitals.
                let hex = v.get(i + 1..i + 3)?;
                if !hex.bytes().all(|h| h.is_ascii_hexdigit()) {
                    return None;
                }
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b if value_char(b) => {
                out.push(b);
                i += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

fn encode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        if value_char(b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl CommandId {
    /// Parse an id. Checks the grammar only, not that the command exists.
    pub fn parse(s: &str) -> Result<CommandId, CommandIdError> {
        let err = |m: &str| Err(CommandIdError(format!("command id {s:?}: {m}")));
        if s.len() > MAX_COMMAND_ID_LEN {
            return err("longer than 128 bytes");
        }
        let (base, params) = match s.split_once('?') {
            Some((b, p)) => (b, Some(p)),
            None => (s, None),
        };
        if !crate::panel::valid_base(base) {
            return err(
                "the base must be `namespace.name` in lowercase letters, digits, `.`, `-` and `_`",
            );
        }
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Some(params) = params {
            for pair in params.split('&') {
                let Some((k, v)) = pair.split_once('=') else {
                    return err("parameters must be `key=value` pairs joined by `&`");
                };
                if !valid_word(k) {
                    return err("parameter keys use lowercase letters, digits, `.`, `-` and `_`");
                }
                let Some(v) = decode(v).filter(|v| !v.is_empty()) else {
                    return err("a parameter value is not valid percent-encoded UTF-8 text");
                };
                if pairs.iter().any(|(key, _)| key == k) {
                    return err("a parameter key appears twice");
                }
                pairs.push((k.to_owned(), v));
            }
        }
        pairs.sort();
        Ok(CommandId {
            base: base.to_owned(),
            params: pairs,
        })
    }

    /// A command without parameters.
    pub fn plain(base: &str) -> Result<CommandId, CommandIdError> {
        CommandId::parse(base).and_then(|id| {
            if id.params.is_empty() {
                Ok(id)
            } else {
                Err(CommandIdError(format!("{base:?} has parameters")))
            }
        })
    }

    /// The id without parameters.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The decoded value of parameter `key`.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every parameter, sorted by key, decoded.
    pub fn params(&self) -> &[(String, String)] {
        &self.params
    }

    /// Its registry entry. `Ok(None)` for an id no entry has (a missing plugin);
    /// `Err` with the reason when the entry does not accept its parameters or misses a
    /// required one. Either way a toolbar keeps the id and shows it disabled.
    pub fn resolve(&self) -> Result<Option<&'static Entry>, CommandIdError> {
        let Some(e) = entry(&self.base) else {
            return Ok(None);
        };
        for (k, v) in &self.params {
            match e.params.iter().find(|p| p.name == k) {
                None => {
                    return Err(CommandIdError(format!("{} has no parameter {k:?}", e.id)));
                }
                Some(Param {
                    ty: ParamType::OneOf(values),
                    ..
                }) if !values.contains(&v.as_str()) => {
                    return Err(CommandIdError(format!(
                        "{}: {k} is one of {}",
                        e.id,
                        values.join(", ")
                    )));
                }
                Some(_) => {}
            }
        }
        if let Some(p) = e
            .params
            .iter()
            .find(|p| p.required && self.param(p.name).is_none())
        {
            return Err(CommandIdError(format!("{} needs {}", e.id, p.name)));
        }
        Ok(Some(e))
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base)?;
        for (i, (k, v)) in self.params.iter().enumerate() {
            let sep = if i == 0 { '?' } else { '&' };
            write!(f, "{sep}{k}={}", encode(v))?;
        }
        Ok(())
    }
}

/// The shell's answers about commands, at the moment a menu, toolbar or palette is
/// built. Calls are cheap reads of state the shell already has (no polling, U5).
pub trait CommandState {
    /// `Ok(())` when the command can run now, else the reason shown as its tooltip.
    fn enabled(&self, id: &CommandId) -> Result<(), String>;
    /// For checkable entries: is it on (a toggle running, an area shown)?
    fn checked(&self, id: &CommandId) -> bool;
    /// For pickers: the choices as `(title, value, is the current one)`, in order.
    fn choices(&self, id: &CommandId) -> Vec<(String, String, bool)>;
}

/// One item of a built menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuItem {
    /// A command.
    Command {
        /// What a click runs.
        id: CommandId,
        /// Title.
        title: String,
        /// Default shortcut text.
        shortcut: Option<&'static str>,
        /// Why it is disabled; `None` when enabled.
        disabled: Option<String>,
        /// `Some` for checkable entries.
        checked: Option<bool>,
    },
    /// A submenu of a picker's choices; each is the picker's id with the choice.
    Choices {
        /// The picker's title.
        title: String,
        /// `(title, id to run, checked)`.
        choices: Vec<(String, CommandId, bool)>,
    },
    /// A separator.
    Separator,
}

/// What each menu holds, in order: command ids (with parameters) and `|` separators.
pub const MENU_LAYOUT: &[(Menu, &[&str])] = &[
    (Menu::File, &["profile.select", "collection.select"]),
    (Menu::Edit, &["edit.undo", "edit.redo"]),
    (
        Menu::View,
        &[
            "ui.workspace.select",
            "|",
            "ui.area.toggle?area=left",
            "ui.area.toggle?area=right",
            "ui.area.toggle?area=bottom",
            "ui.area.hide_all",
            "ui.area.show_all",
            "|",
            "ui.layout.reset_sizes",
            "ui.toolbar.reset",
        ],
    ),
    (
        Menu::Scene,
        &["canvas.studio_mode.toggle", "canvas.transition"],
    ),
    (Menu::Source, &["source.add"]),
    (Menu::Output, &["stream.toggle", "record.toggle"]),
    (
        Menu::Window,
        &["ui.panel.select", "|", "ui.panel.maximize?id=rbs.preview"],
    ),
];

/// The icon an id shows, from the design system's icon set: the entry's, or for an
/// area toggle the panel icon of its area.
pub fn icon(id: &CommandId, e: &Entry) -> Option<&'static str> {
    match (e.id, id.param("area")) {
        ("ui.area.toggle", Some("left")) => Some("panel-left"),
        ("ui.area.toggle", Some("right")) => Some("panel-right"),
        ("ui.area.toggle", Some("bottom")) => Some("panel-bottom"),
        _ => e.icon,
    }
}

/// The title an id shows: the entry's, with the area named for area toggles.
pub fn title(id: &CommandId, e: &Entry) -> String {
    match (e.id, id.param("area")) {
        ("ui.area.toggle", Some("left")) => "Left area".to_owned(),
        ("ui.area.toggle", Some("right")) => "Right area".to_owned(),
        ("ui.area.toggle", Some("bottom")) => "Bottom area".to_owned(),
        _ => match (e.id, id.param("id").and_then(|p| PanelId::parse(p).ok())) {
            ("ui.panel.maximize", Some(p)) => {
                let name = built_in(&p).map_or(p.as_str(), |i| i.title);
                format!("Maximize {name}")
            }
            _ => e.title.to_owned(),
        },
    }
}

fn item(id: CommandId, e: &Entry, state: &dyn CommandState) -> MenuItem {
    if let Kind::Picker { param, .. } = e.kind {
        let choices = state
            .choices(&id)
            .into_iter()
            .map(|(t, v, current)| {
                let run = CommandId {
                    base: e.id.to_owned(),
                    params: vec![(param.to_owned(), v)],
                };
                (t, run, current)
            })
            // A choice whose id would pass the length limit cannot be run by id.
            .filter(|(_, run, _)| run.to_string().len() <= MAX_COMMAND_ID_LEN)
            .collect();
        return MenuItem::Choices {
            title: title(&id, e),
            choices,
        };
    }
    MenuItem::Command {
        title: title(&id, e),
        shortcut: e.shortcut,
        disabled: state.enabled(&id).err(),
        checked: e.checkable.then(|| state.checked(&id)),
        id,
    }
}

/// The menu bar's menus with their items, from [`MENU_LAYOUT`] and the shell's state.
/// Menus without items are left out.
pub fn menus(state: &dyn CommandState) -> Vec<(Menu, Vec<MenuItem>)> {
    let mut out = Vec::new();
    for menu in Menu::ALL {
        let Some((_, ids)) = MENU_LAYOUT.iter().find(|(m, _)| *m == menu) else {
            continue;
        };
        let mut items = Vec::new();
        for text in *ids {
            if *text == "|" {
                items.push(MenuItem::Separator);
                continue;
            }
            let Ok(id) = CommandId::parse(text) else {
                continue;
            };
            if let Ok(Some(e)) = id.resolve() {
                items.push(item(id, e, state));
            }
        }
        if !items.is_empty() {
            out.push((menu, items));
        }
    }
    out
}

/// The default toolbar (Edit's, and that of user workspaces without one). Only
/// commands the registry has; items join as their commands do.
pub fn default_toolbar() -> Toolbar {
    let items = [
        "ui.workspace.select",
        "|",
        "source.add",
        "canvas.studio_mode.toggle",
        "~",
        "ui.area.toggle?area=left",
        "ui.area.toggle?area=bottom",
        "ui.area.toggle?area=right",
        "|",
        "stream.toggle",
        "record.toggle",
    ];
    Toolbar {
        style: ToolbarStyle::IconsText,
        visible: true,
        items: items
            .iter()
            .map(|t| match *t {
                "|" => ToolbarItem::Separator,
                "~" => ToolbarItem::Space,
                id => ToolbarItem::Command((*id).to_owned()),
            })
            .collect(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    struct Fake;

    impl CommandState for Fake {
        fn enabled(&self, id: &CommandId) -> Result<(), String> {
            if id.base() == "edit.redo" {
                Err("Nothing to redo".into())
            } else {
                Ok(())
            }
        }
        fn checked(&self, id: &CommandId) -> bool {
            id.param("area") == Some("left")
        }
        fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
            vec![
                ("Edit".into(), "Edit".into(), true),
                ("Live show".into(), "Live show".into(), false),
            ]
        }
    }

    #[test]
    fn ids_are_canonical_and_round_trip() {
        let id = CommandId::parse("ui.workspace.switch?name=Live%20show").unwrap();
        assert_eq!(id.param("name"), Some("Live show"));
        assert_eq!(id.to_string(), "ui.workspace.switch?name=Live%20show");
        let a = CommandId::parse("x.y?b=1&a=2").unwrap();
        let b = CommandId::parse("x.y?a=2&b=1").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), "x.y?a=2&b=1");
        let turkish = CommandId::parse("ui.workspace.switch?name=%C3%87anl%C4%B1").unwrap();
        assert_eq!(turkish.param("name"), Some("Çanlı"));
        assert_eq!(CommandId::parse(&turkish.to_string()).unwrap(), turkish);
        for bad in [
            "",
            "nodot",
            "Up.per",
            "a.b?x",
            "a.b?x=1&x=2",
            "a.b?x=%zz",
            "a.b?x=a b",
            "A.b",
            "a.b?x=%FF",
            "a.b?x=",
        ] {
            assert!(CommandId::parse(bad).is_err(), "{bad}");
        }
        assert!(CommandId::parse(&format!("a.{}", "b".repeat(127))).is_err());
    }

    #[test]
    fn parameters_are_checked_against_the_entry() {
        let ok = CommandId::parse("ui.area.toggle?area=left").unwrap();
        assert_eq!(ok.resolve().unwrap().unwrap().id, "ui.area.toggle");
        assert!(
            CommandId::parse("ui.area.toggle?area=top")
                .unwrap()
                .resolve()
                .is_err()
        );
        assert!(
            CommandId::parse("edit.undo?x=1")
                .unwrap()
                .resolve()
                .is_err()
        );
        assert_eq!(
            CommandId::parse("com.example.thing").unwrap().resolve(),
            Ok(None)
        );
    }

    #[test]
    fn registry_is_consistent() {
        let mut ids: Vec<&str> = ENTRIES.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ENTRIES.len(), "duplicate ids");
        for e in ENTRIES {
            CommandId::plain(e.id).unwrap();
            assert!(!e.title.is_empty() && !e.tooltip.is_empty(), "{}", e.id);
            for m in e.kind.methods() {
                CommandId::plain(m).unwrap();
            }
            if matches!(e.kind, Kind::Toggle { .. }) {
                assert!(e.checkable, "{}", e.id);
            }
            // Every entry is in its own menu exactly once (area toggles once per area).
            let placed: Vec<&str> = MENU_LAYOUT
                .iter()
                .flat_map(|(m, items)| items.iter().map(move |i| (*m, *i)))
                .filter(|(_, i)| CommandId::parse(i).is_ok_and(|id| id.base() == e.id))
                .map(|(m, _)| {
                    assert_eq!(m, e.menu, "{} is in the wrong menu", e.id);
                    m.title()
                })
                .collect();
            assert!(!placed.is_empty(), "{} is in no menu", e.id);
        }
        for (_, items) in MENU_LAYOUT {
            for i in *items {
                if *i != "|" {
                    let id = CommandId::parse(i).unwrap();
                    assert!(id.resolve().unwrap().is_some(), "{i}");
                    assert_eq!(id.to_string(), *i, "menu ids are written canonically");
                }
            }
        }
    }

    #[test]
    fn maximize_names_its_panel_and_add_source_is_on_the_default_toolbar() {
        let id = CommandId::parse("ui.panel.maximize?id=rbs.preview").unwrap();
        let e = id.resolve().unwrap().unwrap();
        assert_eq!(title(&id, e), "Maximize Preview");
        assert_eq!(icon(&id, e), Some("expand"));
        assert!(
            default_toolbar()
                .items
                .contains(&ToolbarItem::Command("source.add".into()))
        );
    }

    #[test]
    fn menus_follow_state() {
        let m = menus(&Fake);
        let titles: Vec<&str> = m.iter().map(|(m, _)| m.title()).collect();
        assert_eq!(
            titles,
            ["File", "Edit", "View", "Source", "Output", "Window"]
        );
        let file = &m[0].1;
        let MenuItem::Choices { title, choices } = &file[1] else {
            panic!("scene collection picker")
        };
        assert_eq!(title, "Scene collection");
        assert_eq!(choices[1].1.to_string(), "collection.select?id=Live%20show");
        let edit = &m[1].1;
        assert!(
            matches!(&edit[1], MenuItem::Command { disabled: Some(r), .. } if r == "Nothing to redo")
        );
        let view = &m[2].1;
        let MenuItem::Choices { choices, .. } = &view[0] else {
            panic!("workspace picker")
        };
        assert_eq!(
            choices[1].1.to_string(),
            "ui.workspace.select?name=Live%20show"
        );
        assert!(choices[1].1.resolve().unwrap().is_some());
        assert!(choices[0].2 && !choices[1].2);
        assert!(matches!(
            &view[2],
            MenuItem::Command { title, checked: Some(true), .. } if title == "Left area"
        ));
        assert!(matches!(
            &view[3],
            MenuItem::Command {
                checked: Some(false),
                ..
            }
        ));
    }

    #[test]
    fn default_toolbar_uses_known_commands() {
        let t = default_toolbar();
        for i in &t.items {
            if let ToolbarItem::Command(text) = i {
                let id = CommandId::parse(text).unwrap();
                assert!(id.resolve().unwrap().is_some(), "{text}");
            }
        }
    }

    #[test]
    fn required_parameters_pickers_and_escapes() {
        // A required parameter must be there; a picker's choice need not.
        assert!(
            CommandId::parse("ui.area.toggle")
                .unwrap()
                .resolve()
                .is_err()
        );
        let bare = CommandId::parse("ui.workspace.select").unwrap();
        assert!(bare.resolve().unwrap().is_some());
        let pick = CommandId::parse("ui.workspace.select?name=Live%20show").unwrap();
        assert_eq!(pick.resolve().unwrap().unwrap().id, "ui.workspace.select");
        for bad in ["a.b?x=%+1", "a.b?x=%2", "a.b?x=%2g"] {
            assert!(CommandId::parse(bad).is_err(), "{bad}");
        }
        // Lower case escapes are read and written in capitals.
        let lower = CommandId::parse("ui.workspace.switch?name=a%2ab").unwrap();
        assert_eq!(lower.to_string(), "ui.workspace.switch?name=a%2Ab");
    }
}
