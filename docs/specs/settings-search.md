# Spec: Settings search

- Status: Draft
- Owner crate(s): `rbs-ui` (`settings_index`, `command::search`), `rbs-app` (`find:` keys in `ui/`, `menu_bar::open_settings`, the Settings window)
- Related ADRs: ADR-0050 (command registry), ADR-0069 (RBS menu and its search), ADR-0078 (foldable cards), ADR-0113 (experience levels), ADR-0013 (zero cost when unused)
- Principles: U17 (one Settings window with a search field; a setting is never more than one section away), U2, U7

## Purpose

Find a Settings card or a single setting by words and show it: the section, the card opened, the row scrolled into view and marked for a moment. The same search answers in the RBS menu (Ctrl/Cmd+K), in the Settings window's own search field, and through the control API, so a Stream Deck button can open Settings at one setting.

## Scope

In scope: the index of cards and settings, how it is kept in sync with the pages, the search, its results in the RBS menu, `ui.settings.open {section?, setting?}`, and showing a setting in the window.

Out of scope: searching setting values ("6000 kbit/s"), the Source settings window, Properties, and the theme editor's token fields.

## Steps

| Step | What | State |
| --- | --- | --- |
| 1 | Index, sync test, results in the RBS menu, `ui.settings.open {setting}` | This change |
| 2 | Showing the setting: section, card opened, level revealed, row scrolled into view and marked; Shortcuts at a command | Planned |
| 3 | The search field and its results in the Settings window, with the Shortcuts page's commands | Planned; visual proposal to the owner first (rule 16) |

Until step 2, opening Settings at a setting shows its section.

## Contract

### The index (`rbs_ui::settings_index`)

Slint has no document tree to search, and a section that is not shown has no rows at all, so the index is a static table kept beside the `.slint` pages.

```rust
pub enum Level { Simple, Advanced, Expert }
pub struct Place { pub section: &'static str, pub card: &'static str }
pub struct Setting {
    pub key: &'static str,      // stable; a card's key is its card id
    pub title: &'static str,    // as the page writes it
    pub words: &'static str,    // more words to match; never shown
    pub places: &'static [Place],
    pub level: Level,
}
pub const SETTINGS: &[Setting];
pub fn find(key: &str) -> Option<&'static Setting>;
pub fn place(key: &str, section: Option<&str>) -> Option<Place>;
pub fn search(query: &str, context: &str, section_title: &dyn Fn(&str) -> String) -> Vec<Hit>;
```

- **Entries.** Every foldable card (`SettingsCard` with an `id: "settings.…"`, ADR-0078) is an entry whose key is that card id. A setting row is an entry when a user would look for it by name. Status lines, empty states, list items and the fields of an Add or Create form are left out; their card is the entry.
- **Keys** are stable once released: `ui.settings.open {setting}` names them. A row key is `<area>.<name>` in lower case with `-` (`encoding.video-encoder`, `replay.folder`). Keys are unique.
- **Places.** A card has one place, itself. A row shown in more than one section lists each place in section list order: the Encoding card's rows (`EncoderFields`) are in Recording and Stream.
- **Level** is the ADR-0113 level the row is drawn at: `Advanced` rows at Standard and Pro, `Expert` rows at Pro, and both once the card's "More options are hidden" line was answered.
- **Order** follows the section list (`rbs_ui::command::SETTINGS_SECTIONS`), each card before its rows.

### Keeping it in sync

Each indexed row in `crates/rbs-app/ui` carries its key, `find: "<key>";` (a property of `SettingRow` and `SettingBlock` that draws nothing). A row with a fixed label that search leaves out says why, inside its block: `// find: none (<why>).`

`crates/rbs-app/tests/settings_index.rs` reads every `.slint` file except the component library (`components.slint`, `settings.slint`), the test probes and the Source settings window, and fails when:

- a row with a fixed `@tr("…")` label has neither a key nor a `find: none` comment;
- a key is not in the index, a row key of the index is named by no row, or a row names a card key;
- a fixed label differs from the index title (a computed label, such as the bitrate row's, is not compared);
- the level differs from the `advanced` or `expert` branch the row is drawn in;
- the row's card is not among its places;
- a card is not in the index, its fixed title differs, or an index card is not in the pages.

`rbs-ui` tests check the table itself: unique keys, known sections, every place's card is a card entry, a card has one place, and the section order.

### Search

`search` splits the query at white space and matches without case. An entry matches in a place when every word is in the text made of `context`, the section title, the card title (for a row), its own title and its words. One hit per matching place. Hits whose own title holds every word come first, then the others, each group in index order. A blank query has no hits.

The cost is a scan of the table (under 100 entries, one short string built per place) per call; nothing is kept between calls.

### Results in the RBS menu

`rbs_ui::command::search` (ADR-0069) adds the index hits after every command and choice, so a common word does not push commands past the limit of 40 results. Each hit is a `ui.settings.open` command:

- id `ui.settings.open?section=<section>&setting=<key>`;
- title `Settings: <section> › <card> › <setting>` for a row and `Settings: <section> › <card>` for a card (`command::settings_hit_title`);
- the Settings command's icon, and its disabled state.

The menu's words count as context ("File Settings"), as they do for sections: "settings keyframe" finds the keyframe interval.

### `ui.settings.open {section?, setting?}` (protocol 1.56)

- `setting` (optional) is an index key. With `section`, the key must show in that section (`invalid_params` otherwise); without one, Settings opens at the key's first place.
- In the Shortcuts section, `setting` is a command id (`record.main.toggle`) instead: Shortcuts shows that command's row.
- An unknown key or command id is `invalid_params`. From a menu, toolbar or shortcut, such an id runs nothing.
- The registry entry `ui.settings.open` declares the optional text parameter `setting` beside `section`.

### Showing a setting (step 2)

Opening at a setting shows its section; when the key is a row, the card holding it opens if it was folded (the fold is saved, as a click would), a row above the shown level makes its card show all its rows (as answering "More options are hidden" does), the row scrolls into view through the page's `ScrollArea`, takes the keyboard focus on its control, and is marked for `duration.find-mark` with an outline that fades out. A row the page does not draw now (a WAV recording hides the video encoder rows) shows its card instead, marked the same way. Details, tokens and the timing contract (a row checks the request when it is made, as a section switch makes it) are written in that step.

## Errors and failure behaviour

Search never fails. A bad `setting` is `invalid_params` (API) or runs nothing (menu, toolbar, shortcut). A key whose row was removed fails the sync test, never a user's search.

## Performance requirements

Zero cost when unused (ADR-0013): the index is static data in the binary (a few KiB); nothing is allocated, polled or scheduled until a query runs, and the Settings window's state is untouched until a hit is chosen. A query is well under a millisecond.

## Conformance tests

- `rbs-ui`: `settings_index::tests` (table rules, search order, places), `command::tests::search_finds_settings_after_the_commands`.
- `rbs-app`: `tests/settings_index.rs` (sync with the pages), `menu_bar` `settings_open_at_a_setting`.

## Open questions

- Translated titles (Project Fluent, U-principles): the index titles are the English source strings; when translations come, search matches the translated title and the English one.
