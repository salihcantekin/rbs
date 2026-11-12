//! The RBS menu (spec `ui-design-system.md`, "RBS menu"): the rows of its item
//! column, from [`rbs_ui::command::app_menu`] and [`rbs_ui::command::search`], for
//! `ui/app_menu.slint`. Built when the menu opens or its view changes, never while it
//! is closed (P13).

use rbs_ui::command::{AppRow, CommandId, CommandState, MenuItem, app_menu, search};

/// What a row is (`AppMenuRow.kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Command = 1,
    Heading = 2,
    Separator = 3,
    /// Opens its choices; its id is its row index in the category.
    Picker = 4,
    /// Back from a picker's choices.
    Back = 5,
}

/// One row as the toolkit gets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: Kind,
    pub title: String,
    pub icon: &'static str,
    pub shortcut: String,
    pub value: String,
    pub id: String,
    pub enabled: bool,
    pub checkable: bool,
    pub checked: bool,
    pub reason: String,
    /// The rows Up and Down move to: the nearest command, picker or Back row before and
    /// after it (`None` at either end).
    pub up: Option<usize>,
    pub down: Option<usize>,
}

impl Row {
    /// It can be the current row (Up, Down, Return).
    pub fn live(&self) -> bool {
        matches!(self.kind, Kind::Command | Kind::Picker | Kind::Back)
    }

    fn plain(kind: Kind, title: &str) -> Row {
        Row {
            kind,
            title: title.to_owned(),
            icon: "",
            shortcut: String::new(),
            value: String::new(),
            id: String::new(),
            enabled: true,
            checkable: false,
            checked: false,
            reason: String::new(),
            up: None,
            down: None,
        }
    }

    fn command(item: MenuItem, shortcut: &dyn Fn(&str) -> Option<String>) -> Option<Row> {
        let MenuItem::Command {
            id,
            title,
            disabled,
            checked,
            icon,
            ..
        } = item
        else {
            return None;
        };
        let id = id.to_string();
        Some(Row {
            kind: Kind::Command,
            title,
            icon: icon.unwrap_or(""),
            shortcut: shortcut(&id).unwrap_or_default(),
            value: String::new(),
            enabled: disabled.is_none(),
            reason: disabled.unwrap_or_default(),
            checkable: checked.is_some(),
            checked: checked.unwrap_or(false),
            id,
            up: None,
            down: None,
        })
    }
}

/// The menus' titles, in order (the category column).
pub fn categories(state: &dyn CommandState) -> Vec<&'static str> {
    app_menu(state)
        .into_iter()
        .map(|(m, _)| m.title())
        .collect()
}

/// [`rows_of`] with each row's `up` and `down` set.
pub fn rows(
    state: &dyn CommandState,
    category: usize,
    picker: &str,
    query: &str,
    shortcut: &dyn Fn(&str) -> Option<String>,
) -> Vec<Row> {
    let mut rows = rows_of(state, category, picker, query, shortcut);
    let live: Vec<usize> = (0..rows.len()).filter(|i| rows[*i].live()).collect();
    for (i, r) in rows.iter_mut().enumerate() {
        r.up = live.iter().rev().find(|j| **j < i).copied();
        r.down = live.iter().find(|j| **j > i).copied();
    }
    rows
}

/// The item column: the search results for `query` when it is not blank, else the
/// choices of the picker at row `picker` of menu `category` (with a Back row first)
/// when `picker` is not empty, else that menu's rows. `shortcut` gives the chord
/// bound to an id, as the platform writes it.
fn rows_of(
    state: &dyn CommandState,
    category: usize,
    picker: &str,
    query: &str,
    shortcut: &dyn Fn(&str) -> Option<String>,
) -> Vec<Row> {
    if !query.trim().is_empty() {
        return search(state, query)
            .into_iter()
            .filter_map(|i| Row::command(i, shortcut))
            .collect();
    }
    let menus = app_menu(state);
    let Some((_, menu)) = menus.into_iter().nth(category) else {
        return Vec::new();
    };
    if let Ok(at) = picker.parse::<usize>() {
        if let Some(AppRow::Item(MenuItem::Choices {
            title,
            icon,
            disabled,
            choices,
        })) = menu.into_iter().nth(at)
        {
            let mut out = vec![Row::plain(Kind::Back, &title)];
            let open = disabled.is_none();
            out.extend(choices.into_iter().map(|(t, id, current)| {
                let row_icon = rbs_ui::command::choice_icon(&id, current)
                    .or(icon)
                    .unwrap_or("");
                let id = id.to_string();
                Row {
                    kind: Kind::Command,
                    title: t,
                    icon: row_icon,
                    shortcut: shortcut(&id).unwrap_or_default(),
                    value: String::new(),
                    id,
                    enabled: open,
                    checkable: true,
                    checked: current,
                    reason: disabled.clone().unwrap_or_default(),
                    up: None,
                    down: None,
                }
            }));
            return out;
        }
        return Vec::new();
    }
    menu.into_iter()
        .enumerate()
        .filter_map(|(at, row)| match row {
            AppRow::Heading(h) => Some(Row::plain(Kind::Heading, h)),
            AppRow::Separator => Some(Row::plain(Kind::Separator, "")),
            AppRow::Item(MenuItem::Choices {
                title,
                icon,
                disabled,
                choices,
            }) => Some(Row {
                kind: Kind::Picker,
                value: choices
                    .iter()
                    .find(|c| c.2)
                    .map(|c| c.0.clone())
                    .unwrap_or_default(),
                enabled: disabled.is_none() && !choices.is_empty(),
                reason: disabled.unwrap_or_default(),
                icon: icon.unwrap_or(""),
                id: at.to_string(),
                ..Row::plain(Kind::Picker, &title)
            }),
            AppRow::Item(i) => Row::command(i, shortcut),
        })
        .collect()
}

/// The Output profiles as `(title, id to run, current)`: the choices of
/// `profile.select` (the bottom row's chips).
pub fn profiles(state: &dyn CommandState) -> Vec<(String, String, bool)> {
    let Ok(id) = CommandId::plain("profile.select") else {
        return Vec::new();
    };
    state
        .choices(&id)
        .into_iter()
        .filter_map(|(title, value, current)| {
            let run = CommandId::with_params("profile.select", &[("id", &value)]).ok()?;
            Some((title, run.to_string(), current))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    struct State;

    impl CommandState for State {
        fn enabled(&self, id: &CommandId) -> Result<(), String> {
            match id.base() {
                "edit.redo" => Err("Nothing to redo.".into()),
                _ => Ok(()),
            }
        }
        fn checked(&self, id: &CommandId) -> bool {
            id.param("area") == Some("left")
        }
        fn choices(&self, id: &CommandId) -> Vec<(String, String, bool)> {
            match id.base() {
                "ui.workspace.select" => vec![
                    ("Edit".into(), "Edit".into(), true),
                    ("Live".into(), "Live".into(), false),
                ],
                "profile.select" => vec![
                    ("Default".into(), "p1".into(), true),
                    ("Podcast".into(), "p2".into(), false),
                ],
                _ => Vec::new(),
            }
        }
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn categories_follow_the_menu_bar() {
        let c = categories(&State);
        assert_eq!(c[0], "File");
        assert!(c.contains(&"View") && c.contains(&"Help"));
    }

    #[test]
    fn a_menu_has_pickers_groups_and_commands() {
        let view = categories(&State)
            .iter()
            .position(|c| *c == "View")
            .unwrap();
        let r = rows(&State, view, "", "", &|id| {
            (id == "ui.area.hide_all").then(|| "Ctrl+Alt+0".to_owned())
        });
        assert_eq!(r[0].kind, Kind::Picker);
        assert_eq!(
            (r[0].title.as_str(), r[0].value.as_str()),
            ("Workspace", "Edit")
        );
        assert_eq!(r[0].icon, "layout");
        let areas = r.iter().find(|x| x.title == "Areas").unwrap();
        assert_eq!(areas.kind, Kind::Picker);
        let area_rows = rows(&State, view, &areas.id, "", &|id| {
            (id == "ui.area.hide_all").then(|| "Ctrl+Alt+0".to_owned())
        });
        assert_eq!(area_rows[0].kind, Kind::Back);
        let left = area_rows
            .iter()
            .find(|x| x.id == "ui.area.toggle?area=left")
            .unwrap();
        assert!(left.checkable && left.checked);
        let hide = area_rows
            .iter()
            .find(|x| x.id == "ui.area.hide_all")
            .unwrap();
        assert_eq!(hide.shortcut, "Ctrl+Alt+0");
        assert!(r.iter().any(|x| x.title == "Display"));

        assert_eq!((r[0].up, r[0].down), (None, Some(2)));
        assert_eq!(r[2].up, Some(0), "the separator is skipped");
        let choices = rows(&State, view, &r[0].id, "", &none);
        assert_eq!(choices[0].kind, Kind::Back);
        assert_eq!(choices[1].id, "ui.workspace.select?name=Edit");
        assert!(choices[1].checked && !choices[2].checked);
    }

    #[test]
    fn disabled_rows_say_why() {
        let edit = categories(&State)
            .iter()
            .position(|c| *c == "Edit")
            .unwrap();
        let r = rows(&State, edit, "", "", &none);
        let redo = r.iter().find(|x| x.id == "edit.redo").unwrap();
        assert!(!redo.enabled);
        assert_eq!(redo.reason, "Nothing to redo.");
    }

    #[test]
    fn search_finds_commands_choices_and_settings_sections() {
        let r = rows(&State, 0, "", "live", &none);
        assert!(r.iter().any(|x| x.title == "Workspace: Live"));
        let r = rows(&State, 0, "", "settings appear", &none);
        assert_eq!(r[0].id, "ui.settings.open?section=appearance");
        assert_eq!(r[0].title, "Settings: Appearance");
        let r = rows(&State, 0, "", "view reset", &none);
        assert!(r.iter().all(|x| x.kind == Kind::Command));
        assert!(r.iter().any(|x| x.id == "ui.layout.reset_sizes"));
        assert!(rows(&State, 0, "", "zzz", &none).is_empty());
    }

    #[test]
    fn profiles_run_their_choice() {
        let p = profiles(&State);
        assert_eq!(
            p[1],
            ("Podcast".into(), "profile.select?id=p2".into(), false)
        );
    }
}
