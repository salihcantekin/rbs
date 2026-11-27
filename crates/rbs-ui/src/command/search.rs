//! The RBS menu's search (ADR-0050): commands, picker choices, Settings sections and
//! the settings index's cards and settings that hold every word of a query.

use super::{AppRow, CommandState, Menu, MenuItem, SETTINGS_SECTIONS, app_menu};

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
