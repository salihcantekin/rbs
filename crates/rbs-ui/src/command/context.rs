//! Context menus (ADR-0050 decision 4): where they open and what each holds.

use super::*;

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
    /// A group's row in the Sources panel (target `group`, ADR-0182): its commands act
    /// on every member.
    Group,
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
    pub const ALL: [Context; 19] = [
        Context::Scene,
        Context::Scenes,
        Context::Source,
        Context::Sources,
        Context::Group,
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
            Context::Group => "group",
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
            "|",
            "item.group",
            "item.ungroup",
            "item.align_items",
            "item.distribute",
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
    (
        Context::Group,
        &[
            "ui.group.visible.toggle",
            "ui.group.locked.toggle",
            "|",
            "ui.group.rename",
            "item.ungroup",
            "|",
            "ui.group.remove",
        ],
    ),
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
