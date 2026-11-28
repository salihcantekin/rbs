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

mod context;
mod errors;
mod id;
mod labels;
mod layout;
mod search;
pub use context::*;
pub use errors::{CommandIdError, Disabled};
pub use labels::{choice_icon, icon, icon_only, title};
pub use layout::{MENU_LAYOUT, NOT_IN_MENU};
pub use search::{
    MAX_SEARCH_RESULTS, SETTINGS_SEARCH_ROOM, search, settings_hit_title, settings_section_title,
};

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
    ("source.sends", Category::Audio),
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
    ("media.", Category::Source),
    ("item.", Category::Source),
    ("filter.", Category::Source),
    ("ui.source.", Category::Source),
    ("ui.group.", Category::Source),
    ("ui.properties.", Category::Source),
    ("stream.", Category::Output),
    ("record.", Category::Output),
    ("output.", Category::Output),
    ("replay.", Category::Output),
    ("vcam.", Category::Output),
    ("marker.", Category::Output),
    ("canvas.", Category::Output),
    ("ui.guest.", Category::Output),
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
    ("ui.window.", Category::App),
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
        id: "ui.panel.full_height",
        title: "Full window height",
        tooltip: "Let the side area that holds a panel run the whole height of the window, down beside the bottom area",
        icon: Some("panel-right"),
        shortcut: None,
        menu: Menu::Window,
        kind: action("ui.panel.full_height"),
        params: &[PANEL_ID],
        checkable: true,
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
        id: "scene.active.toggle",
        title: "Active",
        tooltip: "An inactive scene is skipped by Next, Previous and the number keys; it can still be chosen",
        icon: Some("power"),
        shortcut: None,
        menu: Menu::Scene,
        // ADR-0195: both halves are `scene.set_active`, the window passes `active`.
        kind: Kind::Toggle {
            on: "scene.set_active",
            off: "scene.set_active",
        },
        params: &[target("scene"), COLLECTION],
        checkable: true,
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
        id: "mixer.track.add",
        title: "Add track",
        tooltip: "Add a mix beside Master that sources can send to, for a recording's extra audio tracks or the stream",
        icon: Some("plus"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("bus.add"),
        // Without `name`: the UI asks for it.
        params: &[target("name")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "mixer.track.rename",
        title: "Rename track",
        tooltip: "Give the track another name; the recording and stream settings follow",
        icon: Some("pencil"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("bus.set"),
        // `track` is the track's place in the Mixer; without `rename` the UI asks.
        params: &[target("track"), target("rename")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "mixer.track.remove",
        title: "Remove track",
        tooltip: "Remove the track and the sends to it; refused while a recording or stream setting uses it",
        icon: Some("trash"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("bus.remove"),
        params: &[target("track")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "source.sends",
        title: "Send to tracks",
        tooltip: "Which mixes the source is heard in: Master and each track",
        icon: Some("speaker"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "source.set_sends",
            param: "bus",
        },
        params: &[target("source"), choice("bus"), COLLECTION],
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
        id: "media.next",
        title: "Next picture",
        tooltip: "Show the slide show's next picture with its transition",
        icon: Some("chevron-right"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("media.next"),
        params: &[target("source"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "media.previous",
        title: "Previous picture",
        tooltip: "Show the slide show's previous picture with its transition",
        icon: Some("chevron-left"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("media.previous"),
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
        id: "item.group",
        title: "Group",
        tooltip: "Group the selected sources so they move, resize, hide and lock together",
        icon: Some("layout"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("group.create"),
        params: &[COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.group.visible.toggle",
        title: "Visible",
        tooltip: "Show or hide every source of the group in the scene",
        icon: Some("eye"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "ui.group.set_visible",
            off: "ui.group.set_visible",
        },
        params: &[target("group"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.group.locked.toggle",
        title: "Locked",
        tooltip: "Lock the pictures of the whole group so the preview does not select, move or resize them",
        icon: Some("lock"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Toggle {
            on: "ui.group.set_locked",
            off: "ui.group.set_locked",
        },
        params: &[target("group"), COLLECTION],
        checkable: true,
        fixed: &[],
    },
    Entry {
        id: "ui.group.rename",
        title: "Rename group",
        tooltip: "Give the group another name",
        icon: Some("pencil"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("ui.group.rename"),
        params: &[target("group"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.group.remove",
        title: "Remove from scene",
        tooltip: "Remove every source of the group from this scene; a source is deleted only when no other scene has it",
        icon: Some("close"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("ui.group.remove"),
        params: &[target("group"), COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.ungroup",
        title: "Ungroup",
        tooltip: "Dissolve the group of the selected source; its sources stay where they are",
        icon: Some("expand"),
        shortcut: None,
        menu: Menu::Source,
        kind: action("group.remove"),
        params: &[COLLECTION],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.align_items",
        title: "Align",
        tooltip: "Line the selected sources up on their left, right, top or bottom edge or on a centre line",
        icon: Some("center"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "item.align_items",
            param: "edge",
        },
        params: &[COLLECTION, choice("edge")],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "item.distribute",
        title: "Distribute",
        tooltip: "Spread three or more selected sources so the gaps between them are equal",
        icon: Some("expand"),
        shortcut: None,
        menu: Menu::Source,
        kind: Kind::Picker {
            method: "item.distribute",
            param: "axis",
        },
        params: &[COLLECTION, choice("axis")],
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
    // The guest window (ADR-0139 phase 1d): run in the app, which opens the window; the
    // pairing itself goes through the `guest.*` control-API methods.
    Entry {
        id: "ui.guest.share",
        title: "Share program...",
        tooltip: "Send the program to another RBS over a direct connection (ADR-0139)",
        icon: Some("link"),
        shortcut: None,
        menu: Menu::Output,
        kind: action("ui.guest.share"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.guest.add",
        title: "Add guest...",
        tooltip: "Receive another RBS's program as a source, from its invite code (ADR-0139)",
        icon: Some("user"),
        shortcut: None,
        menu: Menu::Output,
        kind: action("ui.guest.add"),
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
    // The main window (ADR-0180 decision 5): run in the app, never by the studio; a
    // call that cannot act now changes nothing.
    Entry {
        id: "ui.window.minimize",
        title: "Minimize",
        tooltip: "Minimize the main window",
        icon: Some("window-minimize"),
        shortcut: None,
        menu: Menu::Window,
        kind: action("ui.window.minimize"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.window.zoom",
        title: "Maximize",
        tooltip: "Maximize the main window, or restore it",
        icon: Some("window-maximize"),
        shortcut: None,
        menu: Menu::Window,
        kind: action("ui.window.zoom"),
        params: &[],
        checkable: false,
        fixed: &[],
    },
    Entry {
        id: "ui.window.full_screen",
        title: "Full screen",
        tooltip: "Show the main window full screen, or leave full screen",
        icon: Some("expand"),
        shortcut: None,
        menu: Menu::View,
        kind: action("ui.window.full_screen"),
        params: &[],
        checkable: true,
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
mod tests;
