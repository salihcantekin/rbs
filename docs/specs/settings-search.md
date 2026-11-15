# Spec: Settings search

- Status: Draft
- Owner crate(s): `rbs-ui` (`settings_index`, `command::search`), `rbs-app` (`find:` keys in `ui/`, `SettingsFind`, `menu_bar::open_settings`, the Settings window's field and `show-setting`)
- Related ADRs: ADR-0050 (command registry), ADR-0069 (RBS menu and its search; this spec extends its decision 2 results, "in menu order", with index hits after the commands, as its placeholder "Type a command, setting or panel" foresaw), ADR-0078 (foldable cards), ADR-0113 (experience levels), ADR-0013 (zero cost when unused)
- Principles: U17 (one Settings window with a search field; a setting is never more than one section away), U2, U7

## Purpose

Find a Settings card or a single setting by words and show it: the section, the card opened, the row scrolled into view and marked for a moment. The same search answers in the RBS menu (Ctrl/Cmd+K), in the Settings window's own search field, and through the control API, so a Stream Deck button can open Settings at one setting.

## Scope

In scope: the index of cards and settings, how it is kept in sync with the pages, the search, its results in the RBS menu, `ui.settings.open {section?, setting?}`, and showing a setting in the window.

Out of scope: searching setting values ("6000 kbit/s"), the Source settings window, Properties, and the theme editor's token fields.

## Steps

| Step | What | State |
| --- | --- | --- |
| 1 | Index, sync test, results in the RBS menu, `ui.settings.open {setting, command}` | This change |
| 2 | Showing the setting: section, card opened, level revealed, row scrolled into view and marked; Shortcuts at a command | Built; owner approval of the screenshots pending (rule 16) |
| 3 | The search field and its results in the Settings window, with the Shortcuts page's commands | Built; owner approval of the screenshots pending (rule 16) |

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
- **Keys** are stable once released: `ui.settings.open {setting}` names them. Keys are unique. The prefix `settings.` is reserved for card ids. A card key is the card's fold-state id (ADR-0078), so renaming a card id breaks both its saved fold state and API callers; don't. A row key is opaque: it reads `<area>.<name>` in lower case with `-` (`encoding.video-encoder`, `replay.folder`), but the area is not a section id (`vcam`, `remote`) and the key does not change when a row moves to another section.
- **Places.** A card has one place, itself. A row shown in more than one section lists each place in section list order: the Encoding card's rows (`EncoderFields`) are in Recording and Stream.
- **Level** is the ADR-0113 level the row is drawn at: `Advanced` rows at Standard and Pro, `Expert` rows at Pro, and both once the card's "More options are hidden" line was answered.
- **Order** follows the section list (`rbs_ui::command::SETTINGS_SECTIONS`), each card before its rows.

### Keeping it in sync

Each indexed row in `crates/rbs-app/ui` carries its key, `find: "<key>";` (a property of `SettingRow` and `SettingBlock` that draws nothing). A row with a fixed label that search leaves out says why, inside its block: `// find: none (<why>).`

`crates/rbs-app/tests/settings_index.rs` reads every `.slint` file except the component library (`components.slint`, `settings.slint`), the test probes and the Source settings window, and fails when:

- a row with a fixed `@tr("…")` label has neither a key nor a `find: none` comment;
- a row writes properties on the line of its `{` (the scan reads properties on their own lines), a row sits in a card before the card's `id:` line, or a block header mentions a level in a form the scan does not know (only `if root.advanced:`, `if root.expert:`, `if Experience.advanced:`, `if Experience.expert:` and the Connection card's `if Experience.advanced || conn-card.show-all:`), so the scan never guesses;
- a key is not in the index, a row key of the index is named by no row, or a row names a card key;
- a fixed label differs from the index title (a computed label, such as the bitrate row's, is not compared);
- the level differs from the `advanced` or `expert` branch the row is drawn in;
- the row's card is not among its places;
- a card is not in the index, its fixed title differs, or an index card is not in the pages.

`rbs-ui` tests check the table itself: unique keys, known sections, every place's card is a card entry, a card has one place, and the section order.

### Search

`search` splits the query at white space and matches without case. An entry's own text is its section title, its card title (for a row), its own title and its words. An entry matches in a place when at least one query word is in its own text and every word is in its own text or in `context`. So the menu's "File Settings" never matches every entry by itself, and "settings keyframe" still finds the keyframe interval. One hit per matching place. Hits whose own title holds every word come first, then the others, each group in index order. A blank query has no hits.

The cost is a scan of the table (under 100 entries, one short string built per place) per call; nothing is kept between calls.

### Results in the RBS menu

`rbs_ui::command::search` (ADR-0069) adds the index hits after every command and choice, so a common word does not push commands past the limit of 40 results. When commands alone would fill the list, the last `SETTINGS_SEARCH_ROOM` (5) places go to the first index hits, title matches first. Each hit is a `ui.settings.open` command:

- id `ui.settings.open?section=<section>&setting=<key>`;
- title `Settings: <section> › <card> › <setting>` for a row and `Settings: <section> › <card>` for a card (`command::settings_hit_title`);
- the Settings command's icon, and its disabled state.

The menu's words count as context ("File Settings"), as they do for sections: "settings keyframe" finds the keyframe interval.

### `ui.settings.open {section?, setting?, command?}` (protocol 1.56)

- `setting` (optional) is an index key. With `section`, the key must show in that section (`invalid_params` otherwise); without one, Settings opens at the key's first place.
- `command` (optional) is a command id of the registry without parameters (`record.main.toggle`): Settings opens at Shortcuts and shows that command's row. `section` may be absent or `shortcuts`. A parameterised id (a picker choice) has no row of its own and is `invalid_params`; so are an unknown id, `command` with another section, and `setting` and `command` together.
- An unknown key is `invalid_params`. From a menu, toolbar or shortcut, an id with a bad `setting` or `command` runs nothing.
- The registry entry `ui.settings.open` declares the optional text parameters `setting` and `command` beside `section`.
- Routed `ui.*` methods are not served over the API yet ([status](../status.md)); until they are, this path is specified only, and the registry path (menus, toolbar, shortcuts, `menu_bar::open_settings`) is built and tested.

### Showing a setting (step 2)

`ui.settings.open` with a target, from any caller (the RBS menu, the window's field, a shortcut, the API), first shows its section, then:

- **A setting or card** (`SettingsWindow.show-setting(key, card, level)`, called by `rbs-app` with the place in the shown section): the card opens if it was folded (`Sections.set-closed`, saved as a click would be); a row above the level drawn makes its card show all its rows (`Experience.show`, as answering "More options are hidden" does); then the global `SettingsFind` gets the key and steps its `serial`.
- **Answer.** The `SettingRow`, `SettingBlock` or `SettingsCard` whose key (`find`, or the card's `id`) equals `SettingsFind.key` answers when the serial steps and when it is made (a section switched to at the same moment makes its rows then): it sets `found`, reports its box through `FocusReveal.row`, so the page's `ScrollArea` brings it into view, and becomes the marked target. A row that a `Shown` hides without removing it answers only while `findable` (its condition) holds.
- **Fallback.** When no row answered within `delay.find-fallback` (the format hides it: WAV has no video encoder rows; a Remote control row needs another choice), the window asks for the card instead, which answers the same way.
- **Mark.** The target draws `FindMark`: a `size.focus-ring` outline in `color.border.focus`, `space.2` outside its sides (a card's on all four, with its radius grown to match), at full opacity until `delay.find-mark` has passed, then fading over `motion.normal` (no fade under reduce motion). Nothing else changes: no focus is moved and no value is touched.
- **A command** (`ui.settings.open {command}`): the Shortcuts page's search takes the command's title, so its row is listed.
- **Cost.** One `string` comparison per row when a target is asked for. While `SettingsFind.key` is empty nothing runs: the two timers run only during a jump, and the mark is a transparent rectangle.

### The search field (step 3)

- **Place.** In the Settings window's section list, under the title row: a `TextField` "Search settings" (placeholder and accessible name) across the list, with a Clear button (`IconButton`, `close`, "Clear the search") while it has text. The collapsed section list (the icon rail) has no field; expanding it brings the field back with its text.
- **Results.** While the field has text, its results cover the section list (on `color.surface.raised`, scrolling in a `ScrollArea`): one `ListRow` per result, with the section's icon, the title, and under it where it is ("Recording › Encoding", or the section for a card, "Shortcuts" for a command; nothing for a section). The accessible name is "<title> in <where>". No match: "Nothing matches “<query>”." in `type.caption`, `color.text.secondary`.
- **Order** (`settings_index::window_search`): sections whose title holds every word, then the index hits as in "Search" (at most `MAX_WINDOW_SETTINGS`, 30), then registry commands whose title holds every word (at most `MAX_WINDOW_COMMANDS`, 8).
- **Choosing.** A click (or Return on a focused row) opens the result as `ui.settings.open` does (section, card, setting or command) and keeps the field and its results, so another result is one click away. Return in the field opens the first result. Clear empties the field and shows the section list again.
- Each keystroke runs the search once on the UI thread (well under a millisecond) and replaces the result model; nothing runs while the field is empty.

## Errors and failure behaviour

Search never fails. A bad `setting` is `invalid_params` (API) or runs nothing (menu, toolbar, shortcut). A key whose row was removed fails the sync test, never a user's search.

## Performance requirements

Zero cost when unused (ADR-0013): the index is static data in the binary (a few KiB); nothing is allocated, polled or scheduled until a query runs, and the Settings window's state is untouched until a hit is chosen. A query is well under a millisecond.

## Conformance tests

- `rbs-ui`: `settings_index::tests` (table rules, search order, places), `command::tests::search_finds_settings_after_the_commands`.
- `rbs-ui`: `settings_index::tests::the_window_lists_sections_settings_then_commands`.
- `rbs-app`: `tests/settings_index.rs` (sync with the pages, `findable` in `Shown`), `menu_bar` `settings_open_at_a_setting`.
- Manual (Linux, Xvfb, dark and light): a menu hit and a field hit open the row in another section, scroll to it and mark it; a hidden row (PCM with MKV) marks its card; a Shortcuts command opens Shortcuts filtered to it.

## Open questions

- Translated titles (Project Fluent, U-principles): the index titles are the English source strings; when translations come, search matches the translated title and the English one.

## Review

- 2026-11-15, step 1, independent AI review (a fresh session given only the change, specs, ADRs and code; ADR-0014): approve with changes, no blockers. Fixed in the same change: a separate `command` parameter instead of overloading `setting` (only bare ids), the menu's words no longer match every entry, the sync scan refuses forms it cannot read (properties on the row's `{` line, an unknown level condition, rows before the card's `id:`), room for 5 settings in a full result list, key rules written down (reserved `settings.` prefix, opaque row keys, card keys are fold-state ids), the API path stated as specified only until `ui.*` routing exists, ADR-0069 extension named, and level-hidden hits in step 1 stated as expected. Owner approval of the contract is pending.
