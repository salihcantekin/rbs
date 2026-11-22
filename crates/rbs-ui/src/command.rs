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

mod errors;
pub use errors::{CommandIdError, Disabled};

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

/// What a command is about, for filtering long command lists (Settings › Shortcuts).
/// Unlike [`Menu`], it follows the thing a command acts on: output profiles and scene
/// collections are under File in the menu bar, but have a category of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    /// Scenes, studio mode and transitions.
    Scene,
    /// Sources, their items on the canvas and their filters.
    Source,
    /// Sound: mute, monitoring, the mixer and loudness.
    Audio,
    /// Stream, recording, replay buffer, virtual camera, markers and canvases.
    Output,
    /// Output profiles and scene collections.
    Profiles,
    /// Workspaces, areas, panels, projectors, the toolbar and the look.
    Layout,
    /// The rest: edit history, Settings, the RBS menu, and commands of plugins.
    App,
}

/// Command id prefixes (ending in `.`) or whole base ids, first match wins, so the
/// sound commands of sources and scenes come before `source.` and `scene.`.
const CATEGORY_RULES: &[(&str, Category)] = &[
    ("source.muted.toggle", Category::Audio),
    ("source.mixer_hidden.toggle", Category::Audio),
    ("source.monitor", Category::Audio),
    ("source.monitor_solo.toggle", Category::Audio),
    ("scene.audio.", Category::Audio),
    ("mixer.", Category::Audio),
    ("ui.mixer.", Category::Audio),
    ("loudness.", Category::Audio),
    ("profile.", Category::Profiles),
    ("collection.direct_transition", Category::Scene),
    ("collection.", Category::Profiles),
    ("ui.profile.", Category::Profiles),
    ("ui.collection.", Category::Profiles),
    ("scene.", Category::Scene),
    ("ui.scene.", Category::Scene),
    ("canvas.studio_mode.toggle", Category::Scene),
    ("canvas.transition", Category::Scene),
    ("ui.transition.", Category::Scene),
    ("source.", Category::Source),
    ("item.", Category::Source),
    ("filter.", Category::Source),
    ("ui.source.", Category::Source),
    ("ui.properties.", Category::Source),
    ("stream.", Category::Output),
    ("record.", Category::Output),
    ("output.", Category::Output),
    ("replay.", Category::Output),
    ("vcam.", Category::Output),
    ("marker.", Category::Output),
    ("canvas.", Category::Output),
    ("ui.workspace.", Category::Layout),
    ("ui.area.", Category::Layout),
    ("ui.layout.", Category::Layout),
    ("ui.panel.", Category::Layout),
    ("ui.projector.", Category::Layout),
    ("ui.toolbar.", Category::Layout),
    ("ui.theme.", Category::Layout),
    ("ui.experience.", Category::Layout),
    ("ui.density.", Category::Layout),
    ("edit.", Category::App),
    ("ui.settings.", Category::App),
    ("ui.app_menu.", Category::App),
];

impl Category {
    /// Every category, in the order a filter row shows them.
    pub const ALL: [Category; 7] = [
        Category::Scene,
        Category::Source,
        Category::Audio,
        Category::Output,
        Category::Profiles,
        Category::Layout,
        Category::App,
    ];

    /// The category's title.
    pub fn title(self) -> &'static str {
        match self {
            Category::Scene => "Scene",
            Category::Source => "Source",
            Category::Audio => "Audio",
            Category::Output => "Output",
            Category::Profiles => "Profiles",
            Category::Layout => "Layout",
            Category::App => "App",
        }
    }

    /// The category of a command id, with or without parameters. Ids of plugins and
    /// ids this version does not know are [`Category::App`].
    pub fn of(id: &str) -> Category {
        Self::rule(id).unwrap_or(Category::App)
    }

    /// The rule that matches `id`'s base, if any.
    fn rule(id: &str) -> Option<Category> {
        let base = id.split('?').next().unwrap_or(id);
        CATEGORY_RULES
            .iter()
            .find(|(p, _)| {
                if p.ends_with('.') {
                    base.starts_with(p)
                } else {
                    base == *p
                }
            })
            .map(|(_, c)| *c)
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
    /// A toggle whose parameters depend on a profile setting (ADR-0111 decision 5):
    /// the shell resolves them (`rbs-app` `vertical_outputs::toggle_params`), so this
    /// crate needs no knowledge of profiles. On and off as for [`Kind::Toggle`]; the
    /// off half runs with the same resolved parameters.
    ResolvedToggle {
        /// Runs when the toggle is off.
        on: &'static str,
        /// Runs when the toggle is on.
        off: &'static str,
    },
}

impl Kind {
    /// Every control API method the entry can run.
    pub fn methods(self) -> Vec<&'static str> {
        match self {
            Kind::Action { method } | Kind::Picker { method, .. } => vec![method],
            Kind::Toggle { on, off } | Kind::ResolvedToggle { on, off } => vec![on, off],
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
    /// String parameters merged into every method call the entry runs (ADR-0111
    /// decision 5): `record.vertical.toggle` runs `record.start {canvas: "vertical"}`.
    /// The id's own parameters keep mapping one to one.
    pub fixed: &'static [(&'static str, &'static str)],
}

const PANEL_ID: Param = Param {
    name: "id",
    ty: ParamType::Text,
    required: true,
};

/// The canvas whose program a projector shows (ADR-0125 decision 1).
const PROJECTOR_CANVAS: Param = Param {
    name: "canvas",
    ty: ParamType::OneOf(&["main", "vertical"]),
    required: true,
};

const AREA: Param = Param {
    name: "area",
    ty: ParamType::OneOf(&["left", "right", "bottom"]),
    required: true,
};

/// The Settings section to show; without it the window opens where it was, or General.
const SECTION: Param = Param {
    name: "section",
    ty: ParamType::OneOf(&SETTINGS_SECTIONS),
    required: false,
};

/// The place to show in the Settings window (spec `settings-search.md`): a key of the
/// settings index (`crate::settings_index`), shown in `section` when given, else in
/// its first section.
const SETTING: Param = Param {
    name: "setting",
    ty: ParamType::Text,
    required: false,
};

/// The command whose row the Shortcuts section shows (spec `settings-search.md`): a
/// registry entry id without parameters.
const COMMAND: Param = Param {
    name: "command",
    ty: ParamType::Text,
    required: false,
};

/// The sections of the Settings window, as `ui.settings.open {section}` names them,
/// in the order of the window's section list.
pub const SETTINGS_SECTIONS: [&str; 14] = [
    "general",
    "audio",
    "appearance",
    "shortcuts",
    "permissions",
    "add_ons",
    "advanced",
    "video",
    "global_sources",
    "recording",
    "stream",
    "virtual_camera",
    "remote_control",
    "ai_tools",
];

/// A picker's choice: optional, because the bare picker id opens the list.
const fn choice(name: &'static str) -> Param {
    Param {
        name,
        ty: ParamType::Text,
        required: false,
    }
}

/// A parameter that names what the command acts on (or the text it needs); without
/// it the command acts on the selection, or the UI asks.
const fn target(name: &'static str) -> Param {
    Param {
        name,
        ty: ParamType::Text,
        required: false,
    }
}

/// The scene collection a targeted id was made in: scene, source and item ids are
/// unique only within one, so an id that names one runs nothing while another
/// collection is in use (as `scene.select`'s, ADR-0060).
const COLLECTION: Param = target("collection");

/// The canvas an item edit changes (protocol 1.44): `vertical` edits the item's own
/// layout on the vertical canvas. Without it the menu bar's entries edit the canvas
/// Properties shows; the vertical picture's right-click menu sets it.
const CANVAS: Param = Param {
    name: "canvas",
    ty: ParamType::OneOf(&["main", "vertical"]),
    required: false,
};

/// A vertical canvas side in pixels (`canvas.vertical_size`).
const fn side(name: &'static str) -> Param {
    Param {
        name,
        ty: ParamType::Text,
        required: true,
    }
}

/// The mirror axis of `item.flip`.
const FLIP_AXIS: Param = Param {
    name: "axis",
    ty: ParamType::OneOf(&["horizontal", "vertical"]),
    required: true,
};

/// Whether `scene.duplicate` shares the sources or copies them (ADR-0133 decision 4).
const DUPLICATE_SOURCES: Param = Param {
    name: "sources",
    ty: ParamType::OneOf(&["shared", "copied"]),
    required: false,
};

/// Which way `item.move` / `scene.move` steps in order (front/back: protocol 1.36).
const MOVE_DIRECTION: Param = Param {
    name: "direction",
    ty: ParamType::OneOf(&["up", "down", "front", "back"]),
    required: true,
};

/// Which Properties tab `ui.properties.show` opens (ADR-0084 Source category).
const PROPERTIES_TAB: Param = Param {
    name: "tab",
    ty: ParamType::OneOf(&["transform", "filters", "audio", "source"]),
    required: true,
};

/// The output kind of `output.with_vertical` (ADR-0111).
const OUTPUT_KIND: Param = Param {
    name: "kind",
    ty: ParamType::OneOf(&["record", "stream"]),
    required: true,
};

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
        icon: Some("user"),
        shortcut: None,
        menu: Menu::File,
        kind: Kind::Picker {
            method: "profile.switch",
            param: "id",
        },
        params: &[choice("id")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "collection.select",
        title: "Scene collection",
        tooltip: "Run another scene collection: its canvases, scenes and sources",
        icon: Some("folder"),
        shortcut: None,
        menu: Menu::File,
        kind: Kind::Picker {
            method: "collection.switch",
            param: "id",
        },
        params: &[choice("id")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.profile.new",
        title: "New output profile...",
        tooltip: "Make a output profile with default settings and use it",
        icon: Some("plus"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.profile.new"),
        params: &[Param {
            name: "name",
            ty: ParamType::Text,
            required: false,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.profile.duplicate",
        title: "Duplicate output profile...",
        tooltip: "Make a copy of the output profile in use and use the copy",
        icon: Some("copy"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.profile.duplicate"),
        params: &[Param {
            name: "name",
            ty: ParamType::Text,
            required: false,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.profile.rename",
        title: "Rename output profile...",
        tooltip: "Rename the output profile in use",
        icon: Some("pencil"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.profile.rename"),
        params: &[Param {
            name: "name",
            ty: ParamType::Text,
            required: false,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.profile.import",
        title: "Import output profile...",
        tooltip: "Read a output profile from a file",
        icon: Some("import"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.profile.import"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.profile.export",
        title: "Export output profile...",
        tooltip: "Write the output profile in use to a file",
        icon: Some("export"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.profile.export"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.profile.delete",
        title: "Delete output profile...",
        tooltip: "Delete the output profile in use; it cannot be undone",
        icon: Some("trash"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.profile.delete"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.collection.new",
        title: "New scene collection...",
        tooltip: "Make a scene collection with default settings and use it",
        icon: Some("plus"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.collection.new"),
        params: &[Param {
            name: "name",
            ty: ParamType::Text,
            required: false,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.collection.duplicate",
        title: "Duplicate scene collection...",
        tooltip: "Make a copy of the scene collection in use and use the copy",
        icon: Some("copy"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.collection.duplicate"),
        params: &[Param {
            name: "name",
            ty: ParamType::Text,
            required: false,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.collection.rename",
        title: "Rename scene collection...",
        tooltip: "Rename the scene collection in use",
        icon: Some("pencil"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.collection.rename"),
        params: &[Param {
            name: "name",
            ty: ParamType::Text,
            required: false,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.collection.import",
        title: "Import scene collection...",
        tooltip: "Read a scene collection from a file",
        icon: Some("import"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.collection.import"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.collection.import_obs",
        title: "Import from OBS...",
        tooltip: "Bring an OBS scene collection and profile in as new ones (ADR-0131)",
        icon: Some("import"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.collection.import_obs"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.collection.export",
        title: "Export scene collection...",
        tooltip: "Write the scene collection in use to a file",
        icon: Some("export"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.collection.export"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.collection.delete",
        title: "Delete scene collection...",
        tooltip: "Delete the scene collection in use; it cannot be undone",
        icon: Some("trash"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.collection.delete"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "filter.add",
        title: "Add filter",
        tooltip: "Add a filter to a source: a picture filter, or for a source with only sound an audio filter",
        icon: Some("plus"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "filter.add",
            param: "type",
        },
        params: &[
            target("source"),
            target("audio"),
            COLLECTION,
            choice("type"),
        ],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.settings.open",
        title: "Settings",
        tooltip: "Open the Settings window",
        icon: Some("gear"),
        shortcut: None,
        menu: Menu::File,
        kind: action("ui.settings.open"),
        params: &[SECTION, SETTING, COMMAND],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "edit.undo",
        title: "Undo",
        tooltip: "Undo the last change to the scenes",
        icon: Some("undo"),
        shortcut: Some("Ctrl+Z"),
        menu: Menu::Edit,
        kind: action("edit.undo"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "edit.redo",
        title: "Redo",
        tooltip: "Redo the last undone change to the scenes",
        icon: Some("redo"),
        shortcut: Some("Ctrl+Shift+Z"),
        menu: Menu::Edit,
        kind: action("edit.redo"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.workspace.select",
        title: "Workspace",
        tooltip: "Switch to another workspace",
        icon: Some("layout"),
        shortcut: None,
        menu: Menu::View,
        kind: Kind::Picker {
            method: "ui.workspace.switch",
            param: "name",
        },
        params: &[choice("name")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.area.toggle",
        title: "Area",
        tooltip: "Show or hide an area of the window",
        icon: Some("panel-left"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.area.toggle"),
        params: &[AREA],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.area.hide_all",
        title: "Hide side panels",
        tooltip: "Hide the left, right and bottom areas",
        icon: Some("panel-none"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.area.hide_all"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.area.show_all",
        title: "Show side panels",
        tooltip: "Show the left, right and bottom areas",
        icon: Some("panel-all"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.area.show_all"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.layout.reset_sizes",
        title: "Reset panel sizes",
        tooltip: "Give every area and panel its default size",
        icon: Some("reset"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.layout.reset_sizes"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.workspace.save",
        title: "Save changes",
        tooltip: "Write this workspace's layout and toolbar to its file now",
        icon: Some("save"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.workspace.save"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.workspace.save_as",
        title: "Save as new...",
        tooltip: "Save this layout and toolbar as a new workspace and switch to it",
        icon: Some("save"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.workspace.save_as"),
        // Without a name the window asks for one.
        params: &[Param {
            name: "name",
            ty: ParamType::Text,
            required: false,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.layout.lock",
        title: "Lock layout",
        tooltip: "Refuse layout changes made with the pointer until it is unlocked",
        icon: Some("lock"),
        shortcut: None,
        menu: Menu::View,
        kind: Kind::Toggle {
            on: "ui.layout.set_locked",
            off: "ui.layout.set_locked",
        },
        params: &[],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.layout.undo",
        title: "Undo layout change",
        tooltip: "Undo the last change to the layout",
        icon: Some("undo"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.layout.undo"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.layout.redo",
        title: "Redo layout change",
        tooltip: "Redo the layout change that was undone",
        icon: Some("redo"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.layout.redo"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.workspace.reset",
        title: "Reset workspace",
        tooltip: "Put this workspace's default layout and toolbar back",
        icon: Some("reset"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.workspace.reset"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.layout.rail_size",
        title: "Transition rail width",
        tooltip: "Set the width in pixels of the Transition rail between Preview and Program",
        icon: Some("rail-between"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.layout.rail_size"),
        params: &[Param {
            name: "px",
            ty: ParamType::Text,
            required: true,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.layout.studio_rail",
        title: "Transition rail",
        tooltip: "Put the Transition rail between or below Preview and Program",
        icon: Some("rail-between"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.layout.studio_rail"),
        params: &[Param {
            name: "at",
            ty: ParamType::OneOf(&["between", "below"]),
            required: true,
        }],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.projector.open",
        title: "Open projector",
        tooltip: "Show the program in a window of its own, to share in a video call or put on another display",
        icon: Some("monitor"),
        shortcut: None,
        menu: Menu::View,
        // Checked while open; an open projector comes to the front (ADR-0125
        // decision 9), Close projector closes it.
        kind: action("ui.projector.open"),
        params: &[PROJECTOR_CANVAS],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.projector.size",
        title: "Projector size",
        tooltip: "Size the projector freely, to the canvas's exact pixels, or full screen on a display",
        icon: Some("fit"),
        shortcut: None,
        menu: Menu::View,
        kind: Kind::Picker {
            method: "ui.projector.set_size",
            param: "size",
        },
        params: &[PROJECTOR_CANVAS, choice("size")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.projector.shared",
        title: "Shared size",
        tooltip: "The size a video call or screen recorder receives from the projector",
        icon: Some("info"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.projector.list"),
        params: &[PROJECTOR_CANVAS],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.projector.close",
        title: "Close projector",
        tooltip: "Close the projector window",
        icon: Some("close"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.projector.close"),
        params: &[PROJECTOR_CANVAS],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.toolbar.reset",
        title: "Reset toolbar",
        tooltip: "Put the workspace's default toolbar back",
        icon: Some("reset"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.toolbar.reset"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.theme.select",
        title: "Theme",
        tooltip: "Switch to another theme",
        icon: Some("palette"),
        shortcut: None,
        menu: Menu::View,
        kind: Kind::Picker {
            method: "ui.theme.switch",
            param: "id",
        },
        params: &[choice("id")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.experience.select",
        title: "Settings shown",
        tooltip: "Show the settings of Simple, Standard or Pro",
        icon: Some("sliders"),
        shortcut: None,
        menu: Menu::View,
        kind: Kind::Picker {
            method: "ui.experience.switch",
            param: "level",
        },
        params: &[choice("level")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.density.select",
        title: "Spacing",
        tooltip: "Use regular, compact or roomy spacing",
        icon: Some("layout"),
        shortcut: None,
        menu: Menu::View,
        kind: Kind::Picker {
            method: "ui.density.switch",
            param: "density",
        },
        params: &[choice("density")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.theme.reload",
        title: "Reload themes",
        tooltip: "Read the theme files again and apply them",
        icon: Some("refresh"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.theme.reload"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    // The vertical canvas's picture (ADR-0093 decision 7): one click shows or hides its
    // panel, beside the pictures; it applies while a vertical canvas exists.
    Entry {
        id: "canvas.vertical_preview.toggle",
        title: "Vertical preview",
        tooltip: "Show or hide the vertical canvas's picture",
        icon: Some("canvas-vertical"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "ui.panel.open",
            off: "ui.panel.close",
        },
        params: &[],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.panel.select",
        title: "Panels",
        tooltip: "Open a panel, or close it if it is open",
        icon: Some("window"),
        shortcut: None,
        menu: Menu::Window,
        kind: Kind::Picker {
            method: "ui.panel.open",
            param: "id",
        },
        params: &[choice("id")],
        checkable: false,
        fixed: &[],
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
        fixed: &[],
    },
    Entry {
        id: "ui.panel.collapse",
        title: "Collapse",
        tooltip: "Fold the tab stack that holds a panel to its tab row, or unfold it",
        icon: Some("collapse"),
        shortcut: None,
        menu: Menu::Window,
        kind: Kind::Toggle {
            on: "ui.panel.collapse",
            off: "ui.panel.expand",
        },
        params: &[PANEL_ID],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.panel.auto_hide",
        title: "Auto-hide to the edge",
        tooltip: "Hide the tab stack that holds a panel at the edge until it is needed",
        icon: Some("auto-hide"),
        shortcut: None,
        menu: Menu::Window,
        kind: action("ui.panel.auto_hide"),
        params: &[PANEL_ID],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.panel.move",
        title: "Move to",
        tooltip: "Move a panel to an edge of its window or to the next tab stack",
        icon: Some("move"),
        shortcut: None,
        menu: Menu::Window,
        kind: Kind::Picker {
            method: "ui.panel.move",
            param: "to",
        },
        params: &[PANEL_ID, choice("to")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.panel.float",
        title: "Float in its own window",
        tooltip: "Show a panel in its own window, or dock its window back",
        icon: Some("float"),
        shortcut: None,
        menu: Menu::Window,
        kind: Kind::Toggle {
            on: "ui.panel.float",
            off: "ui.panel.dock",
        },
        params: &[PANEL_ID],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.panel.close",
        title: "Close",
        tooltip: "Close a panel",
        icon: Some("close"),
        shortcut: None,
        menu: Menu::Window,
        kind: action("ui.panel.close"),
        params: &[PANEL_ID],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.select",
        title: "Scene",
        tooltip: "Put a scene on air",
        icon: Some("image"),
        shortcut: None,
        menu: Menu::Scene,
        kind: Kind::Picker {
            method: "scene.switch",
            param: "scene",
        },
        params: &[
            choice("scene"),
            // Scene ids are unique only within a collection: a shortcut names its
            // collection and runs nothing while another one is in use (ADR-0060).
            Param {
                name: "collection",
                ty: ParamType::Text,
                required: false,
            },
        ],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.position",
        title: "Scene at position",
        tooltip: "Put the scene at this position in the Scenes panel on air",
        icon: Some("image"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.switch"),
        params: &[Param {
            name: "n",
            ty: ParamType::OneOf(&["1", "2", "3", "4", "5", "6", "7", "8", "9"]),
            required: true,
        }],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.next",
        title: "Next scene",
        tooltip: "Put the scene after the one on air on air",
        icon: Some("arrow-right"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.switch"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.previous",
        title: "Previous scene",
        tooltip: "Put the scene before the one on air on air",
        icon: Some("arrow-left"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.switch"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.add",
        title: "Add scene",
        tooltip: "Add an empty scene after the others",
        icon: Some("plus"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.add"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.duplicate",
        title: "Duplicate scene",
        tooltip: "Copy the scene with the same sources and layouts",
        icon: Some("copy"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.duplicate"),
        params: &[target("scene"), COLLECTION, DUPLICATE_SOURCES],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.rename",
        title: "Rename scene",
        tooltip: "Give the scene another name",
        icon: Some("pencil"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.rename"),
        // Without `scene`: the scene on air (in studio mode, the preview scene).
        // Without `name`: the UI asks for it (`ui.command.run` cannot run it).
        params: &[target("scene"), target("name"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.scene.transition",
        title: "Transition into this scene…",
        tooltip: "Choose how the program changes to this scene; Default uses the scene collection's transition",
        icon: Some("transition"),
        shortcut: None,
        menu: Menu::Scene,
        // ADR-0135 decision 14: opens the scene's card; the card sets it with
        // `scene.set_transition`. Without `scene`: the scene on air.
        kind: action("ui.scene.transition"),
        params: &[target("scene"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.move",
        title: "Move",
        tooltip: "Move the scene up or down in the scene list",
        icon: Some("chevron-up"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.move"),
        params: &[target("scene"), COLLECTION, MOVE_DIRECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.remove",
        title: "Remove scene",
        tooltip: "Remove the scene; its sources stay in the scene collection",
        icon: Some("trash"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.remove"),
        params: &[target("scene"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "scene.audio.own_levels.toggle",
        title: "Own levels",
        tooltip: "Let the scene keep its own volumes and mutes; off goes back to the sources' own",
        icon: Some("sliders"),
        shortcut: None,
        menu: Menu::Scene,
        // ADR-0091: on copies what is heard now; off discards the scene's levels (the
        // window asks first; a control surface does not, as for scene.remove).
        kind: Kind::Toggle {
            on: "scene.audio.own",
            off: "scene.audio.shared",
        },
        // Without `scene`: the scene on air, whose levels the Mixer edits.
        params: &[target("scene"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "scene.audio.reset_source",
        title: "Use the source's level",
        tooltip: "Remove the scene's own level for the source; the source's own level is heard",
        icon: Some("reset"),
        shortcut: None,
        menu: Menu::Scene,
        kind: action("scene.audio.reset_source"),
        params: &[target("scene"), target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
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
        fixed: &[],
    },
    Entry {
        id: "source.active.toggle",
        title: "Active",
        tooltip: "Deactivate releases the device. Hide only takes the picture off the scene, and showing it again is immediate",
        icon: Some("bolt"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "source.set_active",
            off: "source.set_active",
        },
        params: &[target("source"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "item.visible.toggle",
        title: "Visible",
        tooltip: "Show or hide the source in the scene",
        icon: Some("eye"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "item.set_visible",
            off: "item.set_visible",
        },
        // Without `item`: the selected source's item in the scene.
        params: &[target("item"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "item.show",
        title: "Show now",
        tooltip: "Draw a hidden source for its show duration, without changing the eye or undo",
        icon: Some("bolt"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("item.show"),
        params: &[target("item"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.set_show_for",
        title: "Show duration",
        tooltip: "How long Show draws this source while it is hidden, from 1 ms to 10 minutes",
        icon: Some("hourglass"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("item.set_show_for"),
        params: &[target("item"), COLLECTION, target("show_for_ms")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.locked.toggle",
        title: "Locked",
        tooltip: "Lock the source's picture so the preview does not select, move or resize it",
        icon: Some("lock"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "item.set_locked",
            off: "item.set_locked",
        },
        params: &[target("item"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "item.flip",
        title: "Flip",
        tooltip: "Mirror the source's picture on the canvas, as you see it",
        icon: Some("flip-h"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("item.flip"),
        params: &[target("item"), COLLECTION, FLIP_AXIS, CANVAS],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "item.rotate",
        title: "Rotate",
        tooltip: "Turn the source's picture by a quarter or half circle",
        icon: Some("rotate-cw"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "item.rotate",
            param: "degrees",
        },
        params: &[target("item"), COLLECTION, choice("degrees"), CANVAS],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.fit",
        title: "Fit to canvas",
        tooltip: "Fit, fill or stretch the source's picture to the canvas",
        icon: Some("fit"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "item.fit",
            param: "mode",
        },
        params: &[target("item"), COLLECTION, choice("mode"), CANVAS],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.align",
        title: "Center",
        tooltip: "Center the source's picture on the canvas",
        icon: Some("center"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "item.align",
            param: "axis",
        },
        params: &[target("item"), COLLECTION, choice("axis"), CANVAS],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.reset_transform",
        title: "Reset transform",
        tooltip: "Undo flips and rotation and show the picture at its own size",
        icon: Some("reset"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("item.reset_transform"),
        params: &[target("item"), COLLECTION, CANVAS],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.muted.toggle",
        title: "Muted",
        tooltip: "Mute or unmute the source's sound",
        icon: Some("speaker-off"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "source.set_muted",
            off: "source.set_muted",
        },
        params: &[target("source"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "source.mixer_hidden.toggle",
        title: "Hidden from Mixer",
        tooltip: "Keep the source out of the Mixer; it is still heard",
        icon: Some("eye-off"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "source.set_mixer_hidden",
            off: "source.set_mixer_hidden",
        },
        params: &[target("source"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "source.monitor",
        title: "Monitor",
        tooltip: "Hear the source on the monitor: off, with the outputs, or on the monitor only",
        icon: Some("headphones"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "source.set_monitor",
            param: "monitor",
        },
        params: &[target("source"), choice("monitor"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.monitor_solo.toggle",
        title: "Solo in monitor",
        tooltip: "While any source is soloed, the monitor plays only soloed sources; the outputs are unchanged",
        icon: Some("headphones"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "monitor.set_solo",
            off: "monitor.set_solo",
        },
        params: &[target("source"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.mixer.orientation",
        title: "Mixer layout",
        tooltip: "Arrange the Mixer in rows, in columns, or by the panel's shape",
        icon: Some("layout"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "ui.mixer.set_orientation",
            param: "orientation",
        },
        params: &[choice("orientation")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "mixer.hide_all",
        title: "Hide all in Mixer",
        tooltip: "Keep every source out of the Mixer; they are still heard",
        icon: Some("eye-off"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("source.set_mixer_hidden"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "mixer.hide_muted",
        title: "Hide muted in Mixer",
        tooltip: "Keep the muted sources out of the Mixer",
        icon: Some("speaker-off"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("source.set_mixer_hidden"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "mixer.unhide_all",
        title: "Show all in Mixer",
        tooltip: "Bring every hidden source back into the Mixer",
        icon: Some("eye"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("source.set_mixer_hidden"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "loudness.shown.toggle",
        title: "Show loudness",
        tooltip: "Measure the master mix in LUFS and show it on the Master strip",
        icon: Some("sliders"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "loudness.set_enabled",
            off: "loudness.set_enabled",
        },
        params: &[],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "loudness.reset",
        title: "Reset loudness",
        tooltip: "Start the integrated loudness, range and true peak again from now",
        icon: Some("reset"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("loudness.reset"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.duplicate",
        title: "Duplicate source",
        tooltip: "Make an independent copy of the source above it, with its own settings",
        icon: Some("copy"),
        shortcut: Some("Ctrl+D"),
        menu: Menu::Source,
        kind: action("source.duplicate"),
        params: &[target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.copy_attributes",
        title: "Copy attributes",
        tooltip: "Remember the source's settings to paste chosen parts onto other sources",
        icon: Some("copy"),
        shortcut: Some("Ctrl+Shift+C"),
        menu: Menu::Source,
        kind: action("source.copy_attributes"),
        params: &[target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.source.paste_attributes",
        title: "Paste attributes…",
        tooltip: "Choose which copied settings to paste onto which sources",
        icon: Some("import"),
        shortcut: Some("Ctrl+Shift+V"),
        menu: Menu::Source,
        kind: action("ui.source.paste_attributes"),
        params: &[target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.source.reset",
        title: "Reset source…",
        tooltip: "Choose which of the source's settings go back to their defaults",
        icon: Some("reset"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("ui.source.reset"),
        params: &[target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.rename",
        title: "Rename source",
        tooltip: "Give the source another name",
        icon: Some("pencil"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("source.rename"),
        // Without `source`: the selected source. Without `name`: the UI asks for it.
        params: &[target("source"), target("name"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.reload",
        title: "Reload source",
        tooltip: "Open the source again, for a window that was closed and opened again or a device that came back",
        icon: Some("refresh"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("source.reload"),
        params: &[target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.move",
        title: "Move",
        tooltip: "Move the source among the layers: forward, back, to the front or to the back",
        icon: Some("chevron-up"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("item.move"),
        params: &[target("item"), COLLECTION, MOVE_DIRECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.settings",
        title: "Source settings…",
        tooltip: "Open the Source settings window for this source (mode, frame rate, cursor)",
        icon: Some("gear"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("ui.source.settings"),
        params: &[target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.properties.show",
        title: "Properties",
        tooltip: "Open the Properties panel on a tab of the selected source",
        icon: Some("sliders"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("ui.properties.show"),
        params: &[target("source"), COLLECTION, PROPERTIES_TAB],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.remove",
        title: "Remove from scene",
        tooltip: "Remove the source from this scene; it is deleted only when no other scene has it",
        icon: Some("close"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("item.remove"),
        // Without `item`: the selected source's item in the scene (ADR-0065).
        params: &[target("item"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.delete",
        title: "Delete source",
        tooltip: "Delete the source from the scene collection",
        icon: Some("trash"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("source.delete"),
        params: &[target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
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
        fixed: &[],
    },
    Entry {
        id: "collection.direct_transition",
        title: "Transition outside studio mode",
        tooltip: "How the picture changes when you switch scenes while studio mode is off",
        icon: Some("transition"),
        shortcut: None,
        menu: Menu::Scene,
        kind: Kind::Picker {
            method: "collection.set_transition",
            param: "direct_kind",
        },
        params: &[choice("direct_kind")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "canvas.transition",
        title: "Transition",
        tooltip: "Put the preview scene on air (studio mode)",
        icon: Some("transition"),
        shortcut: Some("Ctrl+Enter"),
        menu: Menu::Scene,
        kind: action("canvas.transition"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.transition.rehearse",
        title: "Rehearse on Preview",
        tooltip: "Play the transition on the Preview picture only; nothing goes on air",
        icon: Some("play"),
        shortcut: None,
        menu: Menu::Scene,
        // ADR-0135 decisions 11 and 12: a UI action, no control API method.
        kind: action("ui.transition.rehearse"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "stream.toggle",
        title: "Go live",
        tooltip: "Start or end the stream; with \"Main + vertical\" also the vertical stream",
        icon: Some("bolt"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::ResolvedToggle {
            on: "stream.start",
            off: "stream.stop",
        },
        params: &[],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "record.toggle",
        title: "Record",
        tooltip: "Start or stop recording; with \"Main + vertical\" also the vertical recording",
        icon: Some("dot"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::ResolvedToggle {
            on: "record.start",
            off: "record.stop",
        },
        params: &[],
        checkable: true,
        fixed: &[],
    },
    // Pause and resume the running recordings (ADR-0124): the second half of the
    // frame's Stop button while a recording runs. The shell titles it "Resume
    // recording" while paused.
    Entry {
        id: "record.pause_toggle",
        title: "Pause recording",
        tooltip: "Pause the running recordings, or resume them; the files go on without a gap",
        icon: Some("pause"),
        shortcut: Some("Ctrl+Shift+P"),
        menu: Menu::Output,
        kind: action("record.pause_toggle"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    // Hold the live streams, or resume them (ADR-0124 decisions 12 and 13): the second
    // half of the frame's End stream button while a stream runs. The shell titles it
    // "Resume stream" while held.
    Entry {
        id: "stream.hold_toggle",
        title: "Hold stream",
        tooltip: "Keep the stream connected while viewers see the last frame with no sound, or resume it",
        icon: Some("hold"),
        shortcut: Some("Ctrl+Shift+H"),
        menu: Menu::Output,
        kind: action("stream.hold_toggle"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    // Break (ADR-0124 decision 15): every live stream holds and every recording
    // pauses; again, they all resume.
    Entry {
        id: "output.break_toggle",
        title: "Break",
        tooltip: "Hold every live stream and pause every recording, or resume them all",
        icon: Some("coffee"),
        shortcut: Some("Ctrl+Shift+B"),
        menu: Menu::Output,
        kind: action("output.break_toggle"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    // The vertical canvas's outputs (ADR-0093 decision 6): the ▾ menus beside the
    // frame's Go live and Record, shown while a vertical canvas exists.
    Entry {
        id: "stream.vertical.toggle",
        title: "Go live vertical",
        tooltip: "Start or end the vertical canvas's stream",
        icon: Some("bolt"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "stream.start",
            off: "stream.stop",
        },
        params: &[],
        checkable: true,
        fixed: &[("canvas", "vertical")],
    },
    Entry {
        id: "record.vertical.toggle",
        title: "Record vertical",
        tooltip: "Start or stop recording the vertical canvas",
        icon: Some("dot"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "record.start",
            off: "record.stop",
        },
        params: &[],
        checkable: true,
        fixed: &[("canvas", "vertical")],
    },
    // Main only, whatever "with main" says (ADR-0111 decision 5).
    Entry {
        id: "stream.main.toggle",
        title: "Go live main only",
        tooltip: "Start or end the main canvas's stream alone",
        icon: Some("bolt"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "stream.start",
            off: "stream.stop",
        },
        params: &[],
        checkable: true,
        fixed: &[("canvas", "main")],
    },
    Entry {
        id: "record.main.toggle",
        title: "Record main only",
        tooltip: "Start or stop recording the main canvas alone",
        icon: Some("dot"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "record.start",
            off: "record.stop",
        },
        params: &[],
        checkable: true,
        fixed: &[("canvas", "main")],
    },
    // Whether Record and Go live also act on the vertical canvas (ADR-0111): a radio
    // pair per kind, "on" ("Main + vertical") or "off" ("Main only").
    Entry {
        id: "output.with_vertical",
        title: "With the vertical output",
        tooltip: "Whether Record or Go live also starts and stops the vertical output",
        icon: Some("canvas-vertical"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Picker {
            method: "output.set_with_vertical",
            param: "on",
        },
        params: &[OUTPUT_KIND, choice("on")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "vcam.toggle",
        title: "Start virtual camera",
        tooltip: "Start or stop sending the program to the virtual camera",
        icon: Some("camera"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "vcam.start",
            off: "vcam.stop",
        },
        params: &[],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "replay.toggle",
        title: "Arm replay buffer",
        tooltip: "Start or stop keeping the last minutes of the program, ready to save as a clip",
        icon: Some("refresh"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Toggle {
            on: "replay.arm",
            off: "replay.disarm",
        },
        params: &[],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "canvas.add_vertical",
        title: "Add vertical output...",
        tooltip: "Add a vertical canvas for short-form video, from the same sources",
        icon: Some("plus"),
        shortcut: None,
        menu: Menu::Output,
        kind: action("canvas.add_vertical"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "canvas.remove_vertical",
        title: "Remove vertical output",
        tooltip: "Remove the vertical canvas; its outputs must be stopped first",
        icon: Some("trash"),
        shortcut: None,
        menu: Menu::Output,
        kind: action("canvas.remove_vertical"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    // The vertical canvas's layouts and size (protocol 1.44, ADR-0093 decisions 4 and
    // 8): in the vertical picture's panel menu and its right-click menu.
    Entry {
        id: "canvas.auto_layout",
        title: "Auto layout",
        tooltip: "Arrange the vertical canvas's sources again: fit, fill or stack them",
        icon: Some("layout"),
        shortcut: None,
        menu: Menu::Output,
        kind: Kind::Picker {
            method: "canvas.auto_layout",
            param: "mode",
        },
        params: &[choice("mode")],
        checkable: false,
        fixed: &[("canvas", "vertical")],
    },
    Entry {
        id: "canvas.vertical_size",
        title: "Vertical size",
        tooltip: "Change the vertical canvas's size; its outputs must be stopped first",
        icon: Some("canvas-vertical"),
        shortcut: None,
        menu: Menu::Output,
        kind: action("canvas.set_vertical_size"),
        params: &[side("width"), side("height")],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "replay.save",
        title: "Save replay clip",
        tooltip: "Save the last minutes the replay buffer holds as a clip file",
        icon: Some("export"),
        shortcut: None,
        menu: Menu::Output,
        kind: action("replay.save"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "marker.add",
        title: "Add marker",
        tooltip: "Mark this moment in every running recording, for the editor project",
        icon: Some("flag"),
        shortcut: Some("Ctrl+Shift+M"),
        menu: Menu::Output,
        kind: action("marker.add"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.app_menu.open",
        title: "Search commands",
        tooltip: "Open the RBS menu with its search field: every command, setting and panel",
        icon: Some("search"),
        shortcut: Some("Ctrl+K"),
        menu: Menu::Help,
        kind: action("ui.app_menu.open"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
];

/// The entry with id `base` (no parameters).
pub fn entry(base: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|e| e.id == base)
}

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

    /// The id of `base` with `params` (decoded values), checked like a parsed id.
    pub fn with_params(base: &str, params: &[(&str, &str)]) -> Result<CommandId, CommandIdError> {
        let mut text = base.to_owned();
        for (i, (k, v)) in params.iter().enumerate() {
            text.push(if i == 0 { '?' } else { '&' });
            text.push_str(k);
            text.push('=');
            text.push_str(&encode(v));
        }
        CommandId::parse(&text)
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
    fn enabled(&self, id: &CommandId) -> Result<(), Disabled>;
    /// For checkable entries: is it on (a toggle running, an area shown)?
    fn checked(&self, id: &CommandId) -> bool;
    /// For pickers: the choices as `(title, value, is the current one)`, in order.
    fn choices(&self, id: &CommandId) -> Vec<(String, String, bool)>;
    /// For pickers: parameters that pin a choice to where it was made, added to the
    /// choice's id when it is bound to a shortcut (`scene.select`: the scene
    /// collection in use). A menu runs choices at once and leaves them out.
    fn context(&self, _id: &CommandId) -> Vec<(&'static str, String)> {
        Vec::new()
    }
    /// A title that depends on state, used instead of the entry's (`scene.select` of
    /// one scene reads "Put on air" or, in studio mode, "Show in preview").
    fn title(&self, _id: &CommandId) -> Option<String> {
        None
    }
    /// Whether the command makes sense for what its id names at all (Muted for a
    /// source without sound does not): a context menu leaves it out, where the menu
    /// bar shows it disabled.
    fn applies(&self, _id: &CommandId) -> bool {
        true
    }
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
        disabled: Option<Disabled>,
        /// `Some` for checkable entries.
        checked: Option<bool>,
        /// Icon name from the icon set ([`icon`]).
        icon: Option<&'static str>,
    },
    /// A submenu of a picker's choices or a `{Group}` of commands.
    Choices {
        /// The picker's or group's title.
        title: String,
        /// Icon name from the icon set ([`icon`]).
        icon: Option<&'static str>,
        /// Why the submenu cannot run; `None` when it can (entries inside may still
        /// be filtered out). Locked transforms set this so the menu greys out.
        disabled: Option<Disabled>,
        /// `(title, id to run, checked)`.
        choices: Vec<(String, CommandId, bool)>,
    },
    /// A separator.
    Separator,
}

/// What each menu holds, in order: command ids (with parameters), `|` separators,
/// `#Heading` section headings (menu bar: separator; RBS menu: heading text), and
/// `{Title:icon` … `}` groups (one submenu of the commands inside).
pub const MENU_LAYOUT: &[(Menu, &[&str])] = &[
    (
        Menu::File,
        &[
            "#Library",
            "profile.select",
            "collection.select",
            "#App",
            "ui.settings.open",
        ],
    ),
    (Menu::Edit, &["edit.undo", "edit.redo"]),
    (
        Menu::View,
        &[
            "ui.workspace.select",
            "|",
            "{Areas:panel-all",
            "ui.area.toggle?area=left",
            "ui.area.toggle?area=right",
            "ui.area.toggle?area=bottom",
            "ui.area.hide_all",
            "ui.area.show_all",
            "}",
            "{Layout:layout",
            "ui.layout.reset_sizes",
            "ui.workspace.reset",
            "ui.workspace.save",
            "ui.workspace.save_as",
            "ui.layout.lock",
            "ui.layout.undo",
            "ui.layout.redo",
            "ui.toolbar.reset",
            "}",
            "{Transition:transition",
            "ui.layout.studio_rail?at=between",
            "ui.layout.studio_rail?at=below",
            "}",
            "{Projectors:monitor",
            "ui.projector.open?canvas=main",
            "ui.projector.open?canvas=vertical",
            "|",
            "ui.projector.size?canvas=main",
            "ui.projector.size?canvas=vertical",
            "|",
            "ui.projector.close?canvas=main",
            "ui.projector.close?canvas=vertical",
            "}",
            "{Display:palette",
            "ui.theme.select",
            "ui.density.select",
            "ui.experience.select",
            "ui.theme.reload",
            "}",
        ],
    ),
    (
        Menu::Scene,
        &[
            "scene.select",
            "scene.previous",
            "scene.next",
            "|",
            "scene.add",
            "{Edit scene:pencil",
            "scene.duplicate",
            "scene.duplicate?sources=copied",
            "scene.rename",
            "scene.move?direction=up",
            "scene.move?direction=down",
            "scene.remove",
            "}",
            "scene.audio.own_levels.toggle",
            "|",
            "canvas.studio_mode.toggle",
            "canvas.transition",
            "ui.transition.rehearse",
            "collection.direct_transition",
        ],
    ),
    (
        Menu::Source,
        &[
            "source.add",
            "|",
            "{Transform:fit",
            "item.flip?axis=horizontal",
            "item.flip?axis=vertical",
            "item.rotate",
            "item.fit",
            "item.align",
            "item.reset_transform",
            "}",
            "{Arrange:chevron-up",
            "item.move?direction=front",
            "item.move?direction=up",
            "item.move?direction=down",
            "item.move?direction=back",
            "}",
            "{Visibility:eye",
            "source.active.toggle",
            "item.visible.toggle",
            "item.locked.toggle",
            "source.muted.toggle",
            "source.mixer_hidden.toggle",
            "source.monitor",
            "source.monitor_solo.toggle",
            "}",
            "{Source:gear",
            "source.rename",
            "source.duplicate",
            "source.settings",
            "ui.properties.show?tab=filters",
            "source.reload",
            "ui.properties.show?tab=source",
            "}",
            "{Attributes:copy",
            "source.copy_attributes",
            "ui.source.paste_attributes",
            "ui.source.reset",
            "}",
            "|",
            "item.remove",
            "source.delete",
            "|",
            "{Mixer:layout",
            "ui.mixer.orientation",
            "mixer.hide_all",
            "mixer.hide_muted",
            "mixer.unhide_all",
            "loudness.shown.toggle",
            "loudness.reset",
            "}",
        ],
    ),
    (
        Menu::Output,
        &[
            "stream.toggle",
            "record.toggle",
            "record.pause_toggle",
            "stream.hold_toggle",
            "output.break_toggle",
            "{Vertical:canvas-vertical",
            "output.with_vertical?kind=record",
            "output.with_vertical?kind=stream",
            "|",
            "stream.main.toggle",
            "record.main.toggle",
            "stream.vertical.toggle",
            "record.vertical.toggle",
            "|",
            "canvas.vertical_preview.toggle",
            "canvas.add_vertical",
            "canvas.remove_vertical",
            "}",
            "|",
            "vcam.toggle",
            "|",
            "replay.toggle",
            "replay.save",
            "|",
            "marker.add",
        ],
    ),
    (
        Menu::Window,
        &["ui.panel.select", "|", "ui.panel.maximize?id=rbs.preview"],
    ),
    (Menu::Help, &["ui.app_menu.open"]),
];

/// Entries no menu lists: `scene.position` is for shortcuts and the palette (the
/// Scene menu lists the scenes through `scene.select`).
pub const NOT_IN_MENU: &[&str] = &[
    "scene.position",
    // A projector's shared size is the first line of its own menu (ADR-0125 decision 6).
    "ui.projector.shared",
    "ui.layout.rail_size",
    // The panel commands name a panel (`id`): they run from the panel menu and the
    // control API, not from a menu bar menu.
    "ui.panel.collapse",
    "ui.panel.auto_hide",
    "ui.panel.move",
    "ui.panel.float",
    "ui.panel.close",
    // Add filter acts on a source (`source`): it runs from the Add filter button of
    // Properties and the control API's `filter.add`, not from a menu bar menu.
    "filter.add",
    // A scene's own level for a source the Mixer hears (ADR-0091): it runs from the
    // Mixer strip's menu and the control API, not from a menu bar menu.
    "scene.audio.reset_source",
    // A scene's transition into it (ADR-0135): a card under the scene's row, so it
    // runs from the Scene row's menu, not from a menu bar menu.
    "ui.scene.transition",
    // The profile and collection commands run from the status bar's pickers and the
    // control API; the File menu keeps the two pickers.
    "ui.profile.new",
    "ui.profile.duplicate",
    "ui.profile.rename",
    "ui.profile.import",
    "ui.profile.export",
    "ui.profile.delete",
    "ui.collection.new",
    "ui.collection.duplicate",
    "ui.collection.rename",
    "ui.collection.import",
    "ui.collection.import_obs",
    "ui.collection.export",
    "ui.collection.delete",
    // Show and its duration run from the Sources row (not built yet) and from a
    // shortcut or the control API. They are not on the menu bar (ADR-0105).
    "item.show",
    "item.set_show_for",
    // The vertical canvas's Auto layout and Size run from the vertical picture's panel
    // menu and right-click menu (they act on that picture), the palette and the
    // control API.
    "canvas.auto_layout",
    "canvas.vertical_size",
];

/// The icon an id shows, from the design system's icon set: the entry's, or for an
/// area toggle the panel icon of its area.
pub fn icon(id: &CommandId, e: &Entry) -> Option<&'static str> {
    if let ("ui.layout.studio_rail", Some("below")) = (e.id, id.param("at")) {
        return Some("rail-below");
    }
    if let ("item.flip", Some("vertical")) = (e.id, id.param("axis")) {
        return Some("flip-v");
    }
    if let ("item.move" | "scene.move", Some(direction)) = (e.id, id.param("direction")) {
        return Some(match direction {
            "down" | "back" => "chevron-down",
            "front" => "export",
            _ => "chevron-up",
        });
    }
    if let ("ui.properties.show", Some(tab)) = (e.id, id.param("tab")) {
        return Some(match tab {
            "filters" => "sliders",
            "source" => "search",
            "audio" => "speaker",
            _ => "move",
        });
    }
    match (e.id, id.param("area")) {
        ("ui.area.toggle", Some("left")) => Some("panel-left"),
        ("ui.area.toggle", Some("right")) => Some("panel-right"),
        ("ui.area.toggle", Some("bottom")) => Some("panel-bottom"),
        _ => e.icon,
    }
}

/// The icon a picker's choice shows. Empty while the choice is the current one, so
/// the checkmark takes the icon's place (spec, "Icons").
pub fn choice_icon(id: &CommandId, checked: bool) -> Option<&'static str> {
    if checked {
        return None;
    }
    let name = match id.base() {
        "source.add" => match id.param("kind") {
            Some("screen") => "monitor",
            Some("window") => "window",
            Some("camera") => "camera",
            Some("audio_input") => "mic",
            Some("system_audio") | Some("application_audio") => "speaker",
            Some("media") | Some("image") => "image",
            Some("browser") => "window",
            Some("text") => "text",
            Some("existing") => "copy",
            _ => "plus",
        },
        "source.monitor" => match id.param("monitor") {
            Some("off") => "headphones-off",
            _ => "headphones",
        },
        "ui.mixer.orientation" => "layout",
        "ui.theme.select" => "palette",
        "ui.experience.select" => "sliders",
        "ui.density.select" => "layout",
        "ui.workspace.select" => "layout",
        "ui.panel.select" => "window",
        "ui.panel.move" => "move",
        "scene.select" => "image",
        "profile.select" => "sliders",
        "collection.select" => "folder",
        "item.rotate" => match id.param("degrees") {
            Some("-90") => "rotate-ccw",
            _ => "rotate-cw",
        },
        "item.fit" => "fit",
        "item.align" => "center",
        "item.flip" => match id.param("axis") {
            Some("vertical") => "flip-v",
            _ => "flip-h",
        },
        "item.reset_transform" => "reset",
        "filter.add" => "plus",
        _ => {
            return entry(id.base()).and_then(|e| icon(id, e));
        }
    };
    Some(name)
}

/// Whether a toolbar shows the id as its icon alone in every style: the area toggles
/// and Settings, whose icons say what they are (the title stays the accessible name
/// and tooltip).
pub fn icon_only(id: &CommandId) -> bool {
    matches!(
        id.base(),
        "ui.area.toggle" | "ui.settings.open" | "canvas.vertical_preview.toggle"
    )
}

/// The title an id shows: the entry's, with the area named for area toggles, the
/// panel for maximize and the position for `scene.position`.
pub fn title(id: &CommandId, e: &Entry) -> String {
    if let ("item.move", Some(direction)) = (e.id, id.param("direction")) {
        return match direction {
            "front" => "Bring to front".to_owned(),
            "up" => "Bring forward".to_owned(),
            "down" => "Send backward".to_owned(),
            "back" => "Send to back".to_owned(),
            _ => e.title.to_owned(),
        };
    }
    if let ("scene.move", Some(direction)) = (e.id, id.param("direction")) {
        return match direction {
            "front" => "Move to top".to_owned(),
            "up" => "Move up".to_owned(),
            "down" => "Move down".to_owned(),
            "back" => "Move to bottom".to_owned(),
            _ => e.title.to_owned(),
        };
    }
    if let ("scene.duplicate", Some("copied")) = (e.id, id.param("sources")) {
        return "Duplicate scene with new sources".to_owned();
    }
    if let ("ui.properties.show", Some(tab)) = (e.id, id.param("tab")) {
        return match tab {
            "filters" => "Filters…".to_owned(),
            "source" => "Choose another target…".to_owned(),
            "audio" => "Audio…".to_owned(),
            "transform" => "Transform…".to_owned(),
            _ => e.title.to_owned(),
        };
    }
    if let ("ui.settings.open", Some("settings.audio.loudness")) = (e.id, id.param("setting")) {
        return "Loudness settings…".to_owned();
    }
    // The vertical picture's panel: closing it only hides the picture, the vertical
    // output stays (its own item, Remove vertical output, follows in the panel menu).
    if let ("ui.panel.close", Some(VERTICAL_PANEL)) = (e.id, id.param("id")) {
        return "Hide vertical preview".to_owned();
    }
    if let ("scene.position", Some(n)) = (e.id, id.param("n")) {
        return format!("Scene at position {n}");
    }
    if let ("output.with_vertical", Some(kind)) = (e.id, id.param("kind")) {
        return match kind {
            "record" => "When recording".to_owned(),
            "stream" => "When going live".to_owned(),
            _ => e.title.to_owned(),
        };
    }
    match (e.id, id.param("at")) {
        ("ui.layout.studio_rail", Some("between")) => {
            return "Transition between the pictures".to_owned();
        }
        ("ui.layout.studio_rail", Some("below")) => {
            return "Transition below the pictures".to_owned();
        }
        _ => {}
    }
    match (e.id, id.param("area")) {
        ("ui.area.toggle", Some("left")) => "Left area".to_owned(),
        ("ui.area.toggle", Some("right")) => "Right area".to_owned(),
        ("ui.area.toggle", Some("bottom")) => "Bottom area".to_owned(),
        _ => match (e.id, id.param("id").and_then(|p| PanelId::parse(p).ok())) {
            ("ui.panel.maximize", Some(p)) => {
                let name = built_in(&p).map_or(p.as_str(), |i| i.title);
                format!("Maximize {name}")
            }
            // One panel of the Panels picker, as a context menu offers it.
            ("ui.panel.select", Some(p)) => built_in(&p).map_or(p.as_str(), |i| i.title).to_owned(),
            _ => e.title.to_owned(),
        },
    }
}

fn item(id: CommandId, e: &Entry, state: &dyn CommandState) -> MenuItem {
    if let Kind::Picker { param, .. } = e.kind
        && id.param(param).is_none()
    {
        let choices = state
            .choices(&id)
            .into_iter()
            .map(|(t, v, current)| {
                // The choice keeps the menu's target (`id=` of a panel), so it runs on it.
                let mut params = id.params().to_vec();
                params.push((param.to_owned(), v));
                params.sort();
                let run = CommandId {
                    base: e.id.to_owned(),
                    params,
                };
                (t, run, current)
            })
            // A choice whose id would pass the length limit cannot be run by id.
            .filter(|(_, run, _)| run.to_string().len() <= MAX_COMMAND_ID_LEN)
            .collect();
        return MenuItem::Choices {
            title: title(&id, e),
            icon: icon(&id, e),
            disabled: state.enabled(&id).err(),
            choices,
        };
    }
    MenuItem::Command {
        title: state.title(&id).unwrap_or_else(|| title(&id, e)),
        shortcut: e.shortcut,
        disabled: state.enabled(&id).err(),
        checked: e.checkable.then(|| state.checked(&id)),
        icon: icon(&id, e),
        id,
    }
}

/// One row of a menu in the RBS menu: a section heading, a separator or an item.
#[derive(Clone, Debug, PartialEq)]
pub enum AppRow {
    /// A section heading (`#Layout` in [`MENU_LAYOUT`]).
    Heading(&'static str),
    /// A separator.
    Separator,
    /// A command or a picker's submenu.
    Item(MenuItem),
}

/// The menus of [`MENU_LAYOUT`] with their rows, for the shell's state, in menu bar
/// order. Menus without items are left out, and so are headings and separators that
/// would end a menu, lead it (a separator) or come twice (a separator after a
/// heading, a heading or separator after a separator, the later one wins). A
/// `{Title:icon` … `}` group becomes one [`MenuItem::Choices`] submenu (as in
/// [`context_menu`]); commands that do not [`CommandState::applies`] are left out.
pub fn app_menu(state: &dyn CommandState) -> Vec<(Menu, Vec<AppRow>)> {
    let mut out = Vec::new();
    for menu in Menu::ALL {
        let Some((_, ids)) = MENU_LAYOUT.iter().find(|(m, _)| *m == menu) else {
            continue;
        };
        let mut rows: Vec<AppRow> = Vec::new();
        // The picker ids that a group lists more than once with different parameters
        // (`output.with_vertical` for recording and for going live): flattened into
        // one submenu their choices would read the same, so each takes its picker's
        // title first ("When recording: Main only").
        let mut twice: Vec<&str> = Vec::new();
        #[allow(clippy::type_complexity)] // Local scratch, same shape as context_menu.
        let mut group: Option<(
            String,
            Option<&'static str>,
            Vec<(String, CommandId, bool)>,
            Option<Disabled>,
        )> = None;
        for (at, text) in ids.iter().enumerate() {
            if let Some(head) = text.strip_prefix('{') {
                let (title, icon) = match head.split_once(':') {
                    Some((t, i)) => (t, Some(i)),
                    None => (head, None),
                };
                group = Some((title.to_owned(), icon, Vec::new(), None));
                twice = repeated_bases(&ids[at + 1..]);
                continue;
            }
            if *text == "}" {
                if let Some((title, icon, choices, disabled)) = group.take() {
                    // Always keep the group row: the menu bar's Slint shape is fixed
                    // from Shape, so dropping a type-empty group would mis-index choices.
                    rows.push(AppRow::Item(MenuItem::Choices {
                        title,
                        icon,
                        // Runnable entries keep the submenu open; an empty group
                        // (locked, wrong type, nothing selected) stays greyed out.
                        disabled: if choices.is_empty() {
                            disabled.or_else(|| Some("Nothing to do.".into()))
                        } else {
                            None
                        },
                        choices,
                    }));
                }
                continue;
            }
            if *text == "|" {
                if group.is_some() {
                    continue;
                }
                if matches!(rows.last(), Some(AppRow::Item(_))) {
                    rows.push(AppRow::Separator);
                }
                continue;
            }
            if let Some(h) = text.strip_prefix('#') {
                if group.is_some() {
                    continue;
                }
                if matches!(rows.last(), Some(AppRow::Heading(_) | AppRow::Separator)) {
                    rows.pop();
                }
                rows.push(AppRow::Heading(h));
                continue;
            }
            let Ok(id) = CommandId::parse(text) else {
                continue;
            };
            let Ok(Some(e)) = id.resolve() else {
                continue;
            };
            // Non-applying commands stay out of the submenu (mic: no Transform
            // rows); the group row itself is still emitted above so the menu
            // bar's choice indices stay aligned with the generated Slint shape.
            if !state.applies(&id) {
                continue;
            }
            let prefix = twice.contains(&id.base()).then(|| flat_title(&id, e));
            let made = item(id, e, state);
            if let Some((_, _, choices, group_disabled)) = group.as_mut() {
                match made {
                    MenuItem::Command {
                        title,
                        disabled: None,
                        checked,
                        id,
                        ..
                    } => choices.push((title, id, checked.unwrap_or(false))),
                    MenuItem::Command {
                        disabled: Some(reason),
                        ..
                    } => {
                        group_disabled.get_or_insert(reason);
                    }
                    MenuItem::Choices {
                        disabled: None,
                        choices: c,
                        ..
                    } => match &prefix {
                        Some(p) => choices.extend(
                            c.into_iter()
                                .map(|(t, run, on)| (format!("{p}: {t}"), run, on)),
                        ),
                        None => choices.extend(c),
                    },
                    MenuItem::Choices {
                        disabled: Some(reason),
                        ..
                    } => {
                        group_disabled.get_or_insert(reason);
                    }
                    _ => {}
                }
                continue;
            }
            rows.push(AppRow::Item(made));
        }
        while matches!(rows.last(), Some(AppRow::Heading(_) | AppRow::Separator)) {
            rows.pop();
        }
        if !rows.is_empty() {
            out.push((menu, rows));
        }
    }
    out
}

/// What a picker's choices start with when a group lists the picker twice and its
/// choices are flattened into the group's submenu: shorter than its submenu title, so
/// the row with its choice still fits the menu.
fn flat_title(id: &CommandId, e: &Entry) -> String {
    match (e.id, id.param("kind")) {
        ("output.with_vertical", Some("record")) => "Recording".to_owned(),
        ("output.with_vertical", Some("stream")) => "Going live".to_owned(),
        _ => title(id, e),
    }
}

/// The base ids listed more than once in a `{Group` whose entries start `inside` (up
/// to its `}`).
fn repeated_bases<'a>(inside: &[&'a str]) -> Vec<&'a str> {
    let bases: Vec<&str> = inside
        .iter()
        .take_while(|t| **t != "}")
        .map(|t| t.split_once('?').map_or(*t, |(base, _)| base))
        .collect();
    let mut out: Vec<&str> = Vec::new();
    for (i, base) in bases.iter().enumerate() {
        if bases[i + 1..].contains(base) && !out.contains(base) {
            out.push(base);
        }
    }
    out
}

/// The menu bar's menus with their items, from [`MENU_LAYOUT`] and the shell's state:
/// the RBS menu's rows with each heading drawn as a separator (none at a menu's
/// start). Menus without items are left out.
pub fn menus(state: &dyn CommandState) -> Vec<(Menu, Vec<MenuItem>)> {
    app_menu(state)
        .into_iter()
        .map(|(menu, rows)| {
            let mut items = Vec::new();
            for row in rows {
                match row {
                    AppRow::Item(i) => items.push(i),
                    AppRow::Heading(_) | AppRow::Separator => {
                        if !items.is_empty() {
                            items.push(MenuItem::Separator);
                        }
                    }
                }
            }
            (menu, items)
        })
        .collect()
}

/// Most results [`search`] gives.
pub const MAX_SEARCH_RESULTS: usize = 40;

/// The title of a Settings section (`remote_control` reads "Remote control").
pub fn settings_section_title(section: &str) -> String {
    if section == "ai_tools" {
        return "AI tools".to_owned();
    }
    let mut out = section.replace('_', " ");
    if let Some(first) = out.get(0..1) {
        let upper = first.to_ascii_uppercase();
        out.replace_range(0..1, &upper);
    }
    out
}

/// A settings search hit as the RBS menu titles it: "Settings: Recording › Encoding ›
/// Video encoder", or "Settings: Recording › Replay buffer" for a card.
pub fn settings_hit_title(settings: &str, hit: &crate::settings_index::Hit) -> String {
    let section = settings_section_title(hit.place.section);
    match hit.card_title() {
        Some(card) => format!("{settings}: {section} › {card} › {}", hit.setting.title),
        None => format!("{settings}: {section} › {}", hit.setting.title),
    }
}

/// The commands whose words hold every word of `query` (case-insensitive), for the
/// RBS menu's search field: commands as the menus show them, each choice of a picker
/// as its own command ("Workspace: Live"), each Settings section ("Settings:
/// Appearance"), and each card and setting of the settings index that matches
/// (`settings_index::search`, "Settings: Recording › Encoding › Video encoder"), after
/// every command, with room for at least [`SETTINGS_SEARCH_ROOM`] of them. A command's menu title counts as one of its words, so "view" finds
/// the View menu's commands. In menu order; at most [`MAX_SEARCH_RESULTS`]; none for
/// a blank query.
pub fn search(state: &dyn CommandState, query: &str) -> Vec<MenuItem> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let hit = |menu: Menu, title: &str| {
        let text = format!("{} {}", menu.title(), title).to_lowercase();
        words.iter().all(|w| text.contains(w.as_str()))
    };
    let mut out = Vec::new();
    // Cards and settings follow every command, so a common word ("video") does not
    // push commands past the result limit.
    let mut settings = Vec::new();
    for (menu, rows) in app_menu(state) {
        for row in rows {
            let AppRow::Item(item) = row else { continue };
            match item {
                MenuItem::Command {
                    ref id,
                    ref title,
                    ref disabled,
                    icon,
                    ..
                } => {
                    let plain = hit(menu, title);
                    let mut sections = Vec::new();
                    if id.base() == "ui.settings.open" && id.param("section").is_none() {
                        for s in SETTINGS_SECTIONS {
                            let title = format!("{title}: {}", settings_section_title(s));
                            if hit(menu, &title) {
                                let mut run = id.clone();
                                run.params.push(("section".to_owned(), s.to_owned()));
                                sections.push(MenuItem::Command {
                                    id: run,
                                    title,
                                    shortcut: None,
                                    disabled: disabled.clone(),
                                    checked: None,
                                    icon,
                                });
                            }
                        }
                    }
                    if id.base() == "ui.settings.open" && id.param("section").is_none() {
                        for hit in crate::settings_index::search(
                            query,
                            &format!("{} {title}", menu.title()),
                            &settings_section_title,
                        ) {
                            let mut run = id.clone();
                            run.params
                                .push(("section".to_owned(), hit.place.section.to_owned()));
                            run.params
                                .push(("setting".to_owned(), hit.setting.key.to_owned()));
                            settings.push(MenuItem::Command {
                                id: run,
                                title: settings_hit_title(title, &hit),
                                shortcut: None,
                                disabled: disabled.clone(),
                                checked: None,
                                icon,
                            });
                        }
                    }
                    if plain {
                        out.push(item);
                    }
                    out.extend(sections);
                }
                MenuItem::Choices {
                    title,
                    icon,
                    disabled,
                    choices,
                } => {
                    for (choice, run, current) in choices {
                        let title = format!("{title}: {choice}");
                        if hit(menu, &title) {
                            out.push(MenuItem::Command {
                                id: run,
                                title,
                                shortcut: None,
                                disabled: disabled.clone(),
                                checked: Some(current),
                                icon,
                            });
                        }
                    }
                }
                MenuItem::Separator => {}
            }
        }
    }
    // Keep room for a few settings when the commands alone fill the list.
    let keep = settings.len().min(SETTINGS_SEARCH_ROOM);
    out.truncate(MAX_SEARCH_RESULTS - keep);
    out.extend(settings);
    out.truncate(MAX_SEARCH_RESULTS);
    out
}

/// Results [`search`] keeps for cards and settings when commands alone would fill it.
pub const SETTINGS_SEARCH_ROOM: usize = 5;

/// Where a context menu opens (ADR-0050 decision 4; spec "Context menus"). Each has
/// its layout in [`CONTEXT_LAYOUT`]; what was right-clicked comes as a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Context {
    /// A row of the Scenes panel (target `scene`).
    Scene,
    /// The Scenes panel outside its rows.
    Scenes,
    /// A row of the Sources panel or a source's picture in Preview (target `source`
    /// and `item`, the source's item in the scene).
    Source,
    /// The Sources panel outside its rows.
    Sources,
    /// The editable picture panel (Program, or in studio mode Preview) outside any
    /// source's picture (target `id`, the panel).
    Preview,
    /// A source's strip or row in the Mixer, or its "more" button (target `source`).
    MixerSource,
    /// The headphones button's menu on a Mixer strip (target `source`): monitor
    /// modes and solo.
    MixerMonitor,
    /// The Mixer outside its strips and rows.
    Mixer,
    /// The Master strip or row in the Mixer, or its "more" button (no target;
    /// ADR-0092 decision 3).
    MixerMaster,
    /// The "This scene" group's header in the Mixer, or its "more" button (target
    /// `scene`, the program scene; ADR-0091 decision 5).
    MixerScene,
    /// A panel's tab or header menu (target `id`, the panel).
    Panel,
    /// The toolbar's Workspace item (no target).
    Workspace,
    /// The status bar's Output profile picker (no target).
    Profiles,
    /// The status bar's Scene collection picker (no target).
    Collections,
    /// The Add filter button of a source's Properties (target `source`).
    AddFilter,
    /// The Add source button's menu: the source types (no target).
    AddSource,
    /// The ▾ beside the frame's Go live and Record while a vertical canvas exists:
    /// its outputs (no target, ADR-0093 decision 6).
    VerticalOutputs,
    /// A projector window (target `canvas`, ADR-0125 decision 6).
    Projector,
}

impl Context {
    /// Every context.
    pub const ALL: [Context; 18] = [
        Context::Scene,
        Context::Scenes,
        Context::Source,
        Context::Sources,
        Context::Preview,
        Context::MixerSource,
        Context::MixerMonitor,
        Context::Mixer,
        Context::MixerMaster,
        Context::MixerScene,
        Context::Panel,
        Context::Workspace,
        Context::Profiles,
        Context::Collections,
        Context::AddFilter,
        Context::AddSource,
        Context::VerticalOutputs,
        Context::Projector,
    ];

    /// Its name, as the shell's menus ask for it.
    pub fn name(self) -> &'static str {
        match self {
            Context::Scene => "scene",
            Context::Scenes => "scenes",
            Context::Source => "source",
            Context::Sources => "sources",
            Context::Preview => "preview",
            Context::MixerSource => "mixer-source",
            Context::MixerMonitor => "mixer-monitor",
            Context::Mixer => "mixer",
            Context::MixerMaster => "mixer-master",
            Context::MixerScene => "mixer-scene",
            Context::Panel => "panel",
            Context::Workspace => "workspace",
            Context::Profiles => "profiles",
            Context::Collections => "collections",
            Context::AddFilter => "add-filter",
            Context::AddSource => "add-source",
            Context::VerticalOutputs => "vertical-outputs",
            Context::Projector => "projector",
        }
    }

    /// The context named `name`.
    pub fn parse(name: &str) -> Option<Context> {
        Context::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// The vertical canvas's picture panel (ADR-0093 decision 7).
pub const VERTICAL_PANEL: &str = "rbs.program?canvas=vertical";

/// What the vertical picture's panel menu and a right-click on it outside any source
/// add first (protocol 1.44): Auto layout and the size presets.
const VERTICAL_EXTRAS: &[&str] = &[
    "canvas.auto_layout",
    "{Size:canvas-vertical",
    "canvas.vertical_size?height=1920&width=1080",
    "canvas.vertical_size?height=1280&width=720",
    "canvas.vertical_size?height=2560&width=1440",
    "}",
    "|",
];

/// Most items a context menu holds, separators and submenus included (the shell
/// declares this many slots).
pub const MAX_CONTEXT_ITEMS: usize = 16;

/// What each context menu holds, in order: command ids and `|` separators. Every
/// parameter an entry declares, the id leaves unset and the target names is filled
/// from the target (`scene.remove` in a Scene row's menu gets the row's `scene`); an
/// item whose required parameters stay unset is left out.
pub const CONTEXT_LAYOUT: &[(Context, &[&str])] = &[
    (
        Context::Scene,
        &[
            "scene.select",
            "scene.rename",
            "scene.duplicate",
            "scene.duplicate?sources=copied",
            "scene.audio.own_levels.toggle",
            "ui.scene.transition",
            "|",
            "scene.move?direction=up",
            "scene.move?direction=down",
            "|",
            "scene.add",
            "|",
            "scene.remove",
        ],
    ),
    (Context::Scenes, &["scene.add"]),
    // The filter types that fit the source, as items of the menu itself.
    (Context::AddFilter, &["*filter.add"]),
    // The source types (and "Existing source" when other scenes have sources), as
    // items of the menu; a type opens the device picker.
    (Context::AddSource, &["*source.add"]),
    (
        Context::Source,
        &[
            "{Transform:fit",
            "item.flip?axis=horizontal",
            "item.flip?axis=vertical",
            "item.rotate",
            "item.fit",
            "item.align",
            "item.reset_transform",
            "}",
            "|",
            "{Arrange:chevron-up",
            "item.move?direction=front",
            "item.move?direction=up",
            "item.move?direction=down",
            "item.move?direction=back",
            "}",
            "|",
            "source.active.toggle",
            "item.visible.toggle",
            "item.locked.toggle",
            "source.muted.toggle",
            "|",
            "{Source:gear",
            "source.rename",
            "source.duplicate",
            "source.settings",
            "ui.properties.show?tab=filters",
            "source.reload",
            "ui.properties.show?tab=source",
            "}",
            "{Attributes:copy",
            "source.copy_attributes",
            "ui.source.paste_attributes",
            "ui.source.reset",
            "}",
            "|",
            "item.remove",
            "source.delete",
        ],
    ),
    (Context::Sources, &["source.add"]),
    // The shared size first (never enabled: it informs), then the sizes as items of
    // the menu itself (ADR-0125 decision 6).
    (
        Context::Projector,
        &[
            "ui.projector.shared",
            "|",
            "*ui.projector.size",
            "|",
            "ui.projector.close",
        ],
    ),
    (
        Context::VerticalOutputs,
        &[
            "output.with_vertical?kind=record",
            "output.with_vertical?kind=stream",
            "|",
            "record.main.toggle",
            "record.vertical.toggle",
            "stream.main.toggle",
            "stream.vertical.toggle",
            "|",
            "ui.settings.open?section=stream",
        ],
    ),
    (
        Context::Preview,
        &[
            "source.add",
            "|",
            "canvas.studio_mode.toggle",
            "ui.panel.maximize",
        ],
    ),
    (
        Context::MixerSource,
        &[
            "ui.mixer.orientation",
            "|",
            "source.muted.toggle",
            "scene.audio.reset_source",
            "source.mixer_hidden.toggle",
            "source.monitor",
            "source.monitor_solo.toggle",
            "|",
            "source.rename",
            "ui.panel.select?id=rbs.properties",
        ],
    ),
    (
        Context::MixerScene,
        &["scene.audio.own_levels.toggle", "|", "ui.mixer.orientation"],
    ),
    (
        Context::MixerMonitor,
        &["*source.monitor", "|", "source.monitor_solo.toggle"],
    ),
    (
        Context::Mixer,
        &[
            "ui.mixer.orientation",
            "|",
            "mixer.hide_all",
            "mixer.hide_muted",
            "mixer.unhide_all",
            "|",
            "loudness.shown.toggle",
        ],
    ),
    (
        Context::MixerMaster,
        &[
            "loudness.shown.toggle",
            "loudness.reset",
            "ui.settings.open?setting=settings.audio.loudness",
            "|",
            "ui.mixer.orientation",
        ],
    ),
    (
        Context::Panel,
        &[
            "ui.panel.collapse",
            "ui.panel.maximize",
            "ui.panel.move",
            "ui.panel.float",
            "ui.panel.auto_hide",
            "|",
            "ui.panel.close",
        ],
    ),
    (
        Context::Workspace,
        &[
            "ui.workspace.select",
            "|",
            "ui.workspace.save",
            "ui.workspace.save_as",
            "ui.workspace.reset",
            "|",
            "ui.layout.lock",
            "ui.layout.undo",
            "ui.layout.redo",
        ],
    ),
    (
        Context::Profiles,
        &[
            "profile.select",
            "|",
            "ui.profile.new",
            "ui.profile.duplicate",
            "ui.profile.rename",
            "|",
            "ui.profile.import",
            "ui.profile.export",
            "|",
            "ui.profile.delete",
        ],
    ),
    (
        Context::Collections,
        &[
            "collection.select",
            "|",
            "ui.collection.new",
            "ui.collection.duplicate",
            "ui.collection.rename",
            "|",
            "ui.collection.import",
            "ui.collection.import_obs",
            "ui.collection.export",
            "|",
            "ui.collection.delete",
        ],
    ),
];

/// The items of `context`'s menu for `target` (`(parameter, value)` pairs naming what
/// was right-clicked), from [`CONTEXT_LAYOUT`] and the shell's state. Commands that do
/// not apply ([`CommandState::applies`]) are left out, and so are separators that
/// would lead, end or follow another separator. A picker whose choice the target
/// fills is one item that runs that choice.
pub fn context_menu(
    context: Context,
    target: &[(String, String)],
    state: &dyn CommandState,
) -> Vec<MenuItem> {
    let Some((_, layout)) = CONTEXT_LAYOUT.iter().find(|(c, _)| *c == context) else {
        return Vec::new();
    };
    // The Mixer's "..." is the panel menu (context `panel`, target `id=rbs.mixer`).
    // The layout picker is a submenu there, as it is on a right-click and on a
    // strip's "..." (ADR-0083). The same command, not a second one.
    let mut ids: Vec<&str> = Vec::with_capacity(layout.len() + VERTICAL_EXTRAS.len());
    if context == Context::Panel && target.iter().any(|(k, v)| k == "id" && v == "rbs.mixer") {
        ids.push("ui.mixer.orientation");
        ids.push("|");
    }
    if matches!(context, Context::Panel | Context::Preview)
        && target.iter().any(|(k, v)| k == "id" && v == VERTICAL_PANEL)
    {
        ids.extend_from_slice(VERTICAL_EXTRAS);
    }
    ids.extend_from_slice(layout);
    // A canvas's picture opens that canvas's projector (ADR-0125 decision 9).
    if context == Context::Preview {
        match target
            .iter()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.as_str())
        {
            // Outside studio mode the Preview panel shows the program.
            Some("rbs.program" | "rbs.preview") => {
                ids.extend_from_slice(&["|", "ui.projector.open?canvas=main"]);
            }
            Some(VERTICAL_PANEL) => {
                ids.extend_from_slice(&["|", "ui.projector.open?canvas=vertical"]);
            }
            _ => {}
        }
    }
    // A source right-clicked on a picture (`picture=main|vertical`) offers that
    // canvas's projector too: sources often cover the whole picture.
    if context == Context::Source {
        match target
            .iter()
            .find(|(k, _)| k == "picture")
            .map(|(_, v)| v.as_str())
        {
            Some("main") => ids.extend_from_slice(&["|", "ui.projector.open?canvas=main"]),
            Some("vertical") => {
                ids.extend_from_slice(&["|", "ui.projector.open?canvas=vertical"]);
            }
            _ => {}
        }
    }
    // Its Close reads "Hide vertical preview"; removing the output is said apart.
    if context == Context::Panel && target.iter().any(|(k, v)| k == "id" && v == VERTICAL_PANEL) {
        ids.push("canvas.remove_vertical");
    }
    let mut items: Vec<MenuItem> = Vec::new();
    // A group (`{Title:icon` ... `}`) is one submenu of every command and choice
    // inside it, so a family of commands takes one row of the menu.
    #[allow(clippy::type_complexity)] // A local scratch value, named once.
    let mut group: Option<(
        String,
        Option<&'static str>,
        Vec<(String, CommandId, bool)>,
        Option<Disabled>,
    )> = None;
    for text in &ids {
        if let Some(head) = text.strip_prefix('{') {
            let (title, icon) = match head.split_once(':') {
                Some((t, i)) => (t, Some(i)),
                None => (head, None),
            };
            group = Some((title.to_owned(), icon, Vec::new(), None));
            continue;
        }
        if *text == "}" {
            if let Some((title, icon, choices, disabled)) = group.take() {
                if choices.is_empty() && disabled.is_none() {
                    continue;
                }
                items.push(MenuItem::Choices {
                    title,
                    icon,
                    // Runnable entries keep the submenu open; only an empty group
                    // stays greyed out (locked transform, no picture, …).
                    disabled: if choices.is_empty() {
                        disabled.or_else(|| Some("Nothing to do.".into()))
                    } else {
                        None
                    },
                    choices,
                });
            }
            continue;
        }
        if *text == "|" {
            if items.last().is_some_and(|i| *i != MenuItem::Separator) {
                items.push(MenuItem::Separator);
            }
            continue;
        }
        // `*id` lists a picker's choices as items of the menu itself, not a submenu.
        let (inline, text) = match text.strip_prefix('*') {
            Some(rest) => (true, rest),
            None => (false, *text),
        };
        let Ok(id) = CommandId::parse(text) else {
            continue;
        };
        let Some(e) = entry(id.base()) else {
            continue;
        };
        let mut params: Vec<(&str, &str)> = id
            .params()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        for (k, v) in target {
            if id.param(k).is_none() && e.params.iter().any(|p| p.name == k) {
                params.push((k.as_str(), v.as_str()));
            }
        }
        // A required parameter the target does not fill leaves the item out.
        let Ok(id) = CommandId::with_params(e.id, &params) else {
            continue;
        };
        if !matches!(id.resolve(), Ok(Some(_))) {
            continue;
        }
        if !state.applies(&id) {
            continue;
        }
        if inline {
            // A picker that cannot run shows as one disabled item with its reason.
            if let Err(reason) = state.enabled(&id) {
                items.push(MenuItem::Command {
                    id,
                    title: e.title.to_owned(),
                    shortcut: e.shortcut,
                    disabled: Some(reason),
                    checked: None,
                    icon: e.icon,
                });
                continue;
            }
            if let MenuItem::Choices { choices, .. } = item(id.clone(), e, state) {
                for (title, run, current) in choices {
                    if items.len() >= MAX_CONTEXT_ITEMS {
                        break;
                    }
                    items.push(MenuItem::Command {
                        icon: choice_icon(&run, current),
                        id: run,
                        title,
                        shortcut: None,
                        disabled: None,
                        checked: current.then_some(true),
                    });
                }
            }
            continue;
        }
        let made = item(id, e, state);
        if let Some((_, _, choices, group_disabled)) = group.as_mut() {
            match made {
                MenuItem::Command {
                    title,
                    disabled: None,
                    checked,
                    id,
                    ..
                } => choices.push((title, id, checked.unwrap_or(false))),
                MenuItem::Command {
                    disabled: Some(reason),
                    ..
                } => {
                    group_disabled.get_or_insert(reason);
                }
                MenuItem::Choices {
                    disabled: None,
                    choices: c,
                    ..
                } => choices.extend(c),
                MenuItem::Choices {
                    disabled: Some(reason),
                    ..
                } => {
                    group_disabled.get_or_insert(reason);
                }
                _ => {}
            }
            continue;
        }
        items.push(made);
        if items.len() >= MAX_CONTEXT_ITEMS {
            break;
        }
    }
    while items.last() == Some(&MenuItem::Separator) {
        items.pop();
    }
    items
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
        "canvas.vertical_preview.toggle",
        "|",
        "ui.settings.open",
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

    #[test]
    fn every_entry_has_a_category_rule() {
        for e in ENTRIES {
            assert!(
                Category::rule(e.id).is_some(),
                "{} has no category rule",
                e.id
            );
        }
    }

    #[test]
    fn categories_follow_what_a_command_acts_on() {
        assert_eq!(Category::of("scene.next"), Category::Scene);
        assert_eq!(
            Category::of("scene.select?scene=a&collection=b"),
            Category::Scene
        );
        assert_eq!(
            Category::of("scene.audio.own_levels.toggle"),
            Category::Audio
        );
        assert_eq!(Category::of("source.muted.toggle"), Category::Audio);
        assert_eq!(Category::of("source.monitor?monitor=off"), Category::Audio);
        assert_eq!(Category::of("source.rename"), Category::Source);
        assert_eq!(Category::of("profile.select?profile=x"), Category::Profiles);
        assert_eq!(Category::of("ui.collection.new"), Category::Profiles);
        assert_eq!(Category::of("stream.toggle"), Category::Output);
        assert_eq!(Category::of("canvas.transition"), Category::Scene);
        assert_eq!(Category::of("canvas.add_vertical"), Category::Output);
        assert_eq!(Category::of("ui.panel.close?id=x"), Category::Layout);
        assert_eq!(Category::of("edit.undo"), Category::App);
        assert_eq!(Category::of("com.example.thing"), Category::App);
        // Every category has a command, so no filter shows an empty list by design.
        for c in Category::ALL {
            assert!(
                ENTRIES.iter().any(|e| Category::of(e.id) == c),
                "{c:?} is empty"
            );
        }
    }

    struct Fake;

    impl CommandState for Fake {
        fn enabled(&self, id: &CommandId) -> Result<(), Disabled> {
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
        assert_eq!(
            CommandId::with_params("ui.workspace.switch", &[("name", "Live show")]).unwrap(),
            id
        );
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
            if matches!(e.kind, Kind::Toggle { .. } | Kind::ResolvedToggle { .. }) {
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
            assert_eq!(
                placed.is_empty(),
                NOT_IN_MENU.contains(&e.id),
                "{} is in no menu, or in one and in NOT_IN_MENU",
                e.id
            );
        }
        for (_, items) in MENU_LAYOUT {
            for i in *items {
                if *i != "|" && *i != "}" && !i.starts_with(['#', '{']) {
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
    fn settings_is_last_on_the_default_toolbar_as_an_icon() {
        let t = default_toolbar();
        assert_eq!(
            t.items.last(),
            Some(&ToolbarItem::Command("ui.settings.open".into()))
        );
        let id = CommandId::parse("ui.settings.open").unwrap();
        let e = id.resolve().unwrap().unwrap();
        assert_eq!(icon(&id, e), Some("gear"));
        assert!(icon_only(&id));
        assert_eq!(e.menu, Menu::File);
        for s in SETTINGS_SECTIONS {
            let id = CommandId::parse(&format!("ui.settings.open?section={s}")).unwrap();
            assert!(id.resolve().unwrap().is_some(), "{s}");
        }
        let bad = CommandId::parse("ui.settings.open?section=mixer").unwrap();
        assert!(bad.resolve().is_err());
    }

    #[test]
    fn menus_follow_state() {
        let m = menus(&Fake);
        let titles: Vec<&str> = m.iter().map(|(m, _)| m.title()).collect();
        assert_eq!(
            titles,
            [
                "File", "Edit", "View", "Scene", "Source", "Output", "Window", "Help"
            ]
        );
        let file = &m[0].1;
        let MenuItem::Choices { title, choices, .. } = &file[1] else {
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
        let areas = view
            .iter()
            .find_map(|i| match i {
                MenuItem::Choices { title, choices, .. } if title == "Areas" => Some(choices),
                _ => None,
            })
            .expect("Areas submenu");
        assert!(areas.iter().any(|(t, _, on)| t == "Left area" && *on));
        assert!(areas.iter().any(|(t, _, on)| t == "Right area" && !*on));
    }

    #[test]
    fn context_layouts_use_known_commands_and_fit() {
        for c in Context::ALL {
            assert_eq!(Context::parse(c.name()), Some(c));
            let (_, ids) = CONTEXT_LAYOUT
                .iter()
                .find(|(l, _)| *l == c)
                .expect("every context has a layout");
            let mut shown = 0;
            let mut grouped = false;
            for i in *ids {
                match *i {
                    "}" => grouped = false,
                    _ if i.starts_with('{') => {
                        grouped = true;
                        shown += 1;
                    }
                    _ if !grouped => shown += 1,
                    _ => {}
                }
            }
            assert!(shown <= MAX_CONTEXT_ITEMS, "{c:?}");
            for i in *ids {
                if *i != "|" && *i != "}" && !i.starts_with('{') {
                    let i = i.strip_prefix('*').unwrap_or(i);
                    let id = CommandId::parse(i).unwrap();
                    assert!(entry(id.base()).is_some(), "{i}");
                    assert_eq!(id.to_string(), i, "layout ids are written canonically");
                }
            }
        }
        assert_eq!(Context::parse("nothing"), None);
    }

    #[test]
    fn groups_collapse_sections_in_the_menu_bar() {
        let app = app_menu(&Fake);
        let (_, view) = app.iter().find(|(m, _)| *m == Menu::View).unwrap();
        assert!(matches!(
            &view[0],
            AppRow::Item(MenuItem::Choices { title, .. }) if title == "Workspace"
        ));
        assert!(view.iter().any(|r| matches!(
            r,
            AppRow::Item(MenuItem::Choices { title, .. }) if title == "Areas"
        )));
        assert!(view.iter().any(|r| matches!(
            r,
            AppRow::Item(MenuItem::Choices { title, .. }) if title == "Display"
        )));
        assert!(!matches!(
            view.last(),
            Some(AppRow::Heading(_) | AppRow::Separator)
        ));
        let bar = menus(&Fake);
        let (_, view) = bar.iter().find(|(m, _)| *m == Menu::View).unwrap();
        assert!(
            !matches!(view[0], MenuItem::Separator),
            "no separator leads a menu"
        );
        assert!(view.iter().any(|i| matches!(i, MenuItem::Separator)));
        let (_, source) = bar.iter().find(|(m, _)| *m == Menu::Source).unwrap();
        assert!(source.iter().any(|i| matches!(
            i,
            MenuItem::Choices { title, .. } if title == "Transform"
        )));
        assert!(search(&Fake, " ").is_empty());
        let found = search(&Fake, "SETTINGS stream");
        assert!(found.iter().any(|i| matches!(i, MenuItem::Command { id, .. } if id.to_string() == "ui.settings.open?section=stream")));
    }

    /// The search finds single settings, after every command, with the section and the
    /// setting in the id (spec `settings-search.md`).
    #[test]
    fn search_finds_settings_after_the_commands() {
        let found = search(&Fake, "keyframe");
        let ids: Vec<String> = found
            .iter()
            .filter_map(|i| match i {
                MenuItem::Command { id, .. } => Some(id.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            [
                "ui.settings.open?section=recording&setting=encoding.keyframe-interval",
                "ui.settings.open?section=stream&setting=encoding.keyframe-interval",
            ]
        );
        let MenuItem::Command { title, .. } = &found[0] else {
            panic!("a command")
        };
        assert_eq!(title, "Settings: Recording › Encoding › Keyframe interval");
        // The menu's words count, as for sections.
        assert_eq!(search(&Fake, "settings keyframe").len(), 2);
        // A card.
        let found = search(&Fake, "replay buffer");
        assert!(found.iter().any(|i| matches!(i, MenuItem::Command { id, title, .. }
            if id.to_string() == "ui.settings.open?section=recording&setting=settings.recording.replay"
                && title == "Settings: Recording › Replay buffer")));
        // Commands come first.
        let found = search(&Fake, "record");
        let first_setting = found
            .iter()
            .position(
                |i| matches!(i, MenuItem::Command { id, .. } if id.param("setting").is_some()),
            )
            .unwrap_or(found.len());
        assert!(
            found[first_setting..].iter().all(
                |i| matches!(i, MenuItem::Command { id, .. } if id.param("setting").is_some())
            )
        );
        assert!(first_setting > 0);
        // A word in many commands still leaves room for settings.
        let found = search(&Fake, "e");
        assert_eq!(found.len(), MAX_SEARCH_RESULTS);
        let settings = found
            .iter()
            .filter(|i| matches!(i, MenuItem::Command { id, .. } if id.param("setting").is_some()))
            .count();
        assert!(settings >= SETTINGS_SEARCH_ROOM, "{settings}");
        // Ids parse back with both parameters.
        let id =
            CommandId::parse("ui.settings.open?section=stream&setting=encoding.b-frames").unwrap();
        assert_eq!(id.param("setting"), Some("encoding.b-frames"));
    }

    /// Every command has an icon from the set, so no menu item is blank (spec,
    /// "Icons": menus show an icon on every command).
    #[test]
    fn every_command_has_an_icon_from_the_set() {
        for e in ENTRIES {
            let name = e.icon.unwrap_or_else(|| panic!("{} has no icon", e.id));
            assert!(crate::icon::find(name).is_some(), "{}: {name}", e.id);
        }
        let layouts = MENU_LAYOUT
            .iter()
            .flat_map(|(_, ids)| ids.iter())
            .chain(CONTEXT_LAYOUT.iter().flat_map(|(_, ids)| ids.iter()));
        for text in layouts.filter(|t| **t != "|" && **t != "}" && !t.starts_with(['#', '{'])) {
            let text = text.strip_prefix('*').unwrap_or(text);
            let id = CommandId::parse(text).unwrap();
            let e = entry(id.base()).unwrap();
            let name = icon(&id, e).unwrap_or_else(|| panic!("{text} has no icon"));
            assert!(crate::icon::find(name).is_some(), "{text}: {name}");
        }
    }

    #[test]
    fn picker_choices_have_icons_except_while_checked() {
        let screen = CommandId::parse("source.add?kind=screen").unwrap();
        assert_eq!(choice_icon(&screen, false), Some("monitor"));
        assert_eq!(choice_icon(&screen, true), None);
        let off = CommandId::parse("source.monitor?monitor=off&source=1").unwrap();
        assert_eq!(choice_icon(&off, false), Some("headphones-off"));
        let theme = CommandId::parse("ui.theme.select?id=builtin%3Adark").unwrap();
        assert_eq!(choice_icon(&theme, false), Some("palette"));
    }

    #[test]
    fn an_inline_picker_lists_its_choices_as_items_or_says_why_it_cannot() {
        struct Types(bool);
        impl CommandState for Types {
            fn enabled(&self, _: &CommandId) -> Result<(), Disabled> {
                if self.0 {
                    Ok(())
                } else {
                    Err("The studio is not running.".into())
                }
            }
            fn checked(&self, _: &CommandId) -> bool {
                false
            }
            fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
                vec![("Blur".into(), "rbs.filter.blur".into(), false)]
            }
        }
        let target = [("source".to_owned(), "3".to_owned())];
        let items = context_menu(Context::AddFilter, &target, &Types(true));
        assert_eq!(ids(&items), ["filter.add?source=3&type=rbs.filter.blur"]);
        // The Audio tab's button says so in its target, and the choices carry it.
        let audio = [target[0].clone(), ("audio".to_owned(), "1".to_owned())];
        let items = context_menu(Context::AddFilter, &audio, &Types(true));
        assert_eq!(
            ids(&items),
            ["filter.add?audio=1&source=3&type=rbs.filter.blur"]
        );
        let items = context_menu(Context::AddFilter, &target, &Types(false));
        assert!(matches!(
            &items[0],
            MenuItem::Command { disabled: Some(r), .. } if r == "The studio is not running."
        ));
    }

    /// Fills targets, leaves out what does not apply, and tidies separators.
    struct Picky;

    impl CommandState for Picky {
        fn enabled(&self, id: &CommandId) -> Result<(), Disabled> {
            match id.base() {
                "scene.remove" => Err("The last scene cannot be removed.".into()),
                _ => Ok(()),
            }
        }
        fn checked(&self, id: &CommandId) -> bool {
            id.base() == "item.visible.toggle"
        }
        fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
            Vec::new()
        }
        fn title(&self, id: &CommandId) -> Option<String> {
            (id.base() == "scene.select").then(|| "Put on air".into())
        }
        fn applies(&self, id: &CommandId) -> bool {
            !matches!(id.base(), "source.muted.toggle" | "scene.select")
        }
    }

    fn ids(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i {
                MenuItem::Command { id, .. } => id.to_string(),
                MenuItem::Choices { title, .. } => format!("[{title}]"),
                MenuItem::Separator => "|".into(),
            })
            .collect()
    }

    #[test]
    fn context_menus_fill_targets_and_tidy_separators() {
        let t = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let scene = context_menu(Context::Scene, &t(&[("scene", "7")]), &Picky);
        assert_eq!(
            ids(&scene),
            [
                "scene.rename?scene=7",
                "scene.duplicate?scene=7",
                "scene.duplicate?scene=7&sources=copied",
                "scene.audio.own_levels.toggle?scene=7",
                "ui.scene.transition?scene=7",
                "|",
                "scene.move?direction=up&scene=7",
                "scene.move?direction=down&scene=7",
                "|",
                "scene.add",
                "|",
                "scene.remove?scene=7"
            ]
        );
        assert!(matches!(
            scene.iter().find(|i| matches!(i, MenuItem::Command { id, .. } if id.to_string().starts_with("scene.remove"))),
            Some(MenuItem::Command { disabled: Some(r), .. }) if r == "The last scene cannot be removed."
        ));
        let source = context_menu(
            Context::Source,
            &t(&[("item", "40"), ("source", "12")]),
            &Picky,
        );
        assert_eq!(
            ids(&source),
            [
                "[Transform]",
                "|",
                "[Arrange]",
                "|",
                "source.active.toggle?source=12",
                "item.visible.toggle?item=40",
                "item.locked.toggle?item=40",
                "|",
                "[Source]",
                "[Attributes]",
                "|",
                "item.remove?item=40",
                "source.delete?source=12"
            ]
        );
        assert!(matches!(
            &source[5],
            MenuItem::Command {
                checked: Some(true),
                ..
            }
        ));
        // A picker without its choice is a submenu; with it, one item with the
        // state's title.
        let preview = context_menu(Context::Preview, &t(&[("id", "rbs.program")]), &Picky);
        assert_eq!(
            ids(&preview),
            [
                "[Add source]",
                "|",
                "canvas.studio_mode.toggle",
                "ui.panel.maximize?id=rbs.program",
                "|",
                "ui.projector.open?canvas=main"
            ]
        );
        // The Preview panel shows the program outside studio mode, so it opens
        // the main canvas's projector too.
        let main_preview = context_menu(Context::Preview, &t(&[("id", "rbs.preview")]), &Picky);
        assert_eq!(
            ids(&main_preview).last().map(String::as_str),
            Some("ui.projector.open?canvas=main")
        );
        // A source on a picture offers the projector of that picture's canvas.
        let on_picture = context_menu(
            Context::Source,
            &t(&[("item", "1"), ("source", "2"), ("picture", "main")]),
            &Picky,
        );
        assert_eq!(
            ids(&on_picture).last().map(String::as_str),
            Some("ui.projector.open?canvas=main")
        );
        // Without the panel, Maximize has no required `id` and is left out.
        let bare = context_menu(Context::Preview, &[], &Picky);
        assert_eq!(
            ids(&bare),
            ["[Add source]", "|", "canvas.studio_mode.toggle"]
        );
        struct OnAir;
        impl CommandState for OnAir {
            fn enabled(&self, _: &CommandId) -> Result<(), Disabled> {
                Ok(())
            }
            fn checked(&self, _: &CommandId) -> bool {
                false
            }
            fn choices(&self, _: &CommandId) -> Vec<(String, String, bool)> {
                Vec::new()
            }
            fn title(&self, id: &CommandId) -> Option<String> {
                (id.base() == "scene.select").then(|| "Put on air".into())
            }
        }
        let other = context_menu(Context::Scene, &t(&[("scene", "3")]), &OnAir);
        assert!(matches!(
            &other[0],
            MenuItem::Command { id, title, .. }
                if id.to_string() == "scene.select?scene=3" && title == "Put on air"
        ));
    }

    #[test]
    fn a_picker_listed_twice_in_a_group_names_itself_in_each_choice() {
        let menus = app_menu(&Fake);
        let (_, output) = menus.iter().find(|(m, _)| *m == Menu::Output).unwrap();
        let vertical = output
            .iter()
            .find_map(|r| match r {
                AppRow::Item(MenuItem::Choices { title, choices, .. }) if title == "Vertical" => {
                    Some(choices)
                }
                _ => None,
            })
            .unwrap();
        let titles: Vec<&str> = vertical.iter().map(|(t, _, _)| t.as_str()).collect();
        assert_eq!(
            titles[..4],
            [
                "Recording: Edit",
                "Recording: Live show",
                "Going live: Edit",
                "Going live: Live show"
            ]
        );
        assert!(titles.contains(&"Vertical preview"));
        // A picker listed once keeps its choices as they are (View's Display group).
        let (_, view) = menus.iter().find(|(m, _)| *m == Menu::View).unwrap();
        assert!(view.iter().any(|r| matches!(
            r,
            AppRow::Item(MenuItem::Choices { choices, .. })
                if choices.iter().any(|(t, _, _)| t == "Live show")
        )));
    }

    #[test]
    fn the_vertical_panel_menu_hides_the_picture_and_removes_the_output_apart() {
        let target = [("id".to_owned(), VERTICAL_PANEL.to_owned())];
        let titles: Vec<String> = context_menu(Context::Panel, &target, &Fake)
            .into_iter()
            .filter_map(|i| match i {
                MenuItem::Command { title, .. } => Some(title),
                _ => None,
            })
            .collect();
        let n = titles.len();
        assert_eq!(
            titles[n - 2..],
            ["Hide vertical preview", "Remove vertical output"]
        );
        // Other panels keep Close and have no Remove vertical output.
        let mixer = [("id".to_owned(), "rbs.mixer".to_owned())];
        let titles: Vec<String> = context_menu(Context::Panel, &mixer, &Fake)
            .into_iter()
            .filter_map(|i| match i {
                MenuItem::Command { title, .. } => Some(title),
                _ => None,
            })
            .collect();
        assert_eq!(titles.last().map(String::as_str), Some("Close"));
    }

    struct Layouts;

    impl CommandState for Layouts {
        fn enabled(&self, _: &CommandId) -> Result<(), Disabled> {
            Ok(())
        }
        fn checked(&self, _: &CommandId) -> bool {
            false
        }
        fn choices(&self, id: &CommandId) -> Vec<(String, String, bool)> {
            if id.base() == "ui.mixer.orientation" {
                vec![
                    ("Automatic".into(), "auto".into(), true),
                    ("Rows".into(), "rows".into(), false),
                    ("Columns".into(), "columns".into(), false),
                ]
            } else {
                Vec::new()
            }
        }
    }

    fn layout_submenu(items: &[MenuItem]) -> Option<Vec<(&str, bool)>> {
        items.iter().find_map(|i| match i {
            MenuItem::Choices { title, choices, .. } if title == "Mixer layout" => {
                Some(choices.iter().map(|(t, _, on)| (t.as_str(), *on)).collect())
            }
            _ => None,
        })
    }

    #[test]
    fn the_mixer_menus_offer_the_layout_as_a_submenu() {
        let want = vec![("Automatic", true), ("Rows", false), ("Columns", false)];
        assert_eq!(
            layout_submenu(&context_menu(Context::Mixer, &[], &Layouts)),
            Some(want.clone())
        );
        assert_eq!(
            layout_submenu(&context_menu(
                Context::MixerSource,
                &[("source".into(), "1".into())],
                &Layouts,
            )),
            Some(want.clone())
        );
        assert_eq!(
            layout_submenu(&context_menu(
                Context::Panel,
                &[("id".into(), "rbs.mixer".into())],
                &Layouts,
            )),
            Some(want)
        );
        // Another panel's menu does not grow a Mixer layout.
        assert_eq!(
            layout_submenu(&context_menu(
                Context::Panel,
                &[("id".into(), "rbs.sources".into())],
                &Layouts,
            )),
            None
        );
        let MenuItem::Choices { choices, .. } = &context_menu(Context::Mixer, &[], &Layouts)[0]
        else {
            panic!("layout submenu");
        };
        assert_eq!(
            choices[0].1.to_string(),
            "ui.mixer.orientation?orientation=auto"
        );
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
