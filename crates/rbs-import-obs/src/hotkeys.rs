//! OBS hotkeys to RBS shortcuts (ADR-0131 decision 10).
//!
//! Frontend hotkeys are in the profile's `[Hotkeys]` section, each a JSON object
//! (`{"bindings":[...]}`; the replay entry is `{"ReplayBuffer.Save":[...]}`). Scene
//! hotkeys are in each scene's `hotkeys` object. A binding is
//! `{"key":"OBS_KEY_F1","control":true,"alt":false,"shift":false,"command":false}`.
//! Every imported binding takes the app scope.

use std::collections::HashMap;

use rbs_scene::model::SceneId;
use rbs_ui::shortcut::{
    Check, Chord, Key, Keymap, MAX_ENTRIES, MAX_PER_COMMAND, Mods, Named, Platform, Scope,
    ShortcutsDoc,
};
use serde_json::Value;

use crate::ini::Ini;
use crate::report::{Area, ImportReport, Outcome, ReportEntry};

/// One OBS hotkey binding brought across.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedShortcut {
    /// The RBS command id; for `scene.select`, see [`ImportedShortcut::command_for`].
    pub command: &'static str,
    /// The scene of `scene.select`.
    pub scene: Option<SceneId>,
    /// The chord.
    pub chord: Chord,
    /// The OBS action, for the report.
    pub obs_action: String,
}

impl ImportedShortcut {
    /// The command id with its parameters, the collection id put in.
    pub fn command_for(&self, collection_id: &str) -> String {
        match self.scene {
            Some(s) => format!(
                "{}?collection={collection_id}&scene={}",
                self.command,
                s.get()
            ),
            None => self.command.to_owned(),
        }
    }
}

/// OBS frontend actions: the first of a pair maps to the RBS toggle, the second is
/// reported (decision 10).
const FRONTEND: &[(&str, Option<&str>, &str)] = &[
    ("OBSBasic.StartStreaming", Some("stream.toggle"), ""),
    ("OBSBasic.StopStreaming", None, "OBSBasic.StartStreaming"),
    ("OBSBasic.StartRecording", Some("record.toggle"), ""),
    ("OBSBasic.StopRecording", None, "OBSBasic.StartRecording"),
    ("OBSBasic.PauseRecording", Some("record.pause_toggle"), ""),
    ("OBSBasic.UnpauseRecording", None, "OBSBasic.PauseRecording"),
    ("OBSBasic.StartReplayBuffer", Some("replay.toggle"), ""),
    (
        "OBSBasic.StopReplayBuffer",
        None,
        "OBSBasic.StartReplayBuffer",
    ),
    ("ReplayBuffer.Save", Some("replay.save"), ""),
];

/// The platform an OBS file was written on, from the paths in it: drive letters are
/// Windows, `/Users/` macOS, `/home/` Linux. `None` when no path tells.
pub fn file_platform(root: &Value) -> Option<Platform> {
    let mut counts = [0usize; 3];
    fn walk(v: &Value, counts: &mut [usize; 3]) {
        match v {
            Value::String(s) => {
                let b = s.as_bytes();
                if b.len() > 2
                    && b[0].is_ascii_alphabetic()
                    && b[1] == b':'
                    && matches!(b[2], b'/' | b'\\')
                {
                    counts[1] += 1;
                } else if s.starts_with("/Users/") {
                    counts[0] += 1;
                } else if s.starts_with("/home/") {
                    counts[2] += 1;
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, counts)),
            Value::Object(o) => o.values().for_each(|x| walk(x, counts)),
            _ => {}
        }
    }
    walk(root, &mut counts);
    let max = *counts.iter().max()?;
    if max == 0 || counts.iter().filter(|c| **c == max).count() > 1 {
        return None;
    }
    [Platform::MacOs, Platform::Windows, Platform::Linux]
        .into_iter()
        .zip(counts)
        .find(|(_, c)| *c == max)
        .map(|(p, _)| p)
}

fn key_of(name: &str) -> Result<Key, &'static str> {
    if name.starts_with("OBS_MOUSE") || name.starts_with("OBS_KEY_MOUSE") {
        return Err("a mouse button");
    }
    let k = name.strip_prefix("OBS_KEY_").ok_or("not a keyboard key")?;
    if k.starts_with("NUM") && k.len() > 3 {
        return Err("a number pad key");
    }
    if k.len() == 1 {
        let c = k.chars().next().ok_or("empty")?;
        if let Some(key) = Key::letter(c) {
            return Ok(key);
        }
        if let Some(d) = c.to_digit(10) {
            return Ok(Key::Digit(d as u8));
        }
    }
    if let Some(n) = k.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=24).contains(&n) {
            return Ok(Key::F(n));
        }
    }
    Ok(Key::Named(match k {
        "SPACE" => Named::Space,
        "RETURN" | "ENTER" => Named::Enter,
        "ESCAPE" => Named::Escape,
        "TAB" => Named::Tab,
        "BACKSPACE" => Named::Backspace,
        "DELETE" => Named::Delete,
        "INSERT" => Named::Insert,
        "HOME" => Named::Home,
        "END" => Named::End,
        "PAGEUP" => Named::PageUp,
        "PAGEDOWN" => Named::PageDown,
        "UP" => Named::Up,
        "DOWN" => Named::Down,
        "LEFT" => Named::Left,
        "RIGHT" => Named::Right,
        "MINUS" => Named::Minus,
        "EQUAL" => Named::Equal,
        "BRACKETLEFT" => Named::BracketLeft,
        "BRACKETRIGHT" => Named::BracketRight,
        "BACKSLASH" => Named::Backslash,
        "SEMICOLON" => Named::Semicolon,
        "APOSTROPHE" => Named::Quote,
        "COMMA" => Named::Comma,
        "PERIOD" => Named::Period,
        "SLASH" => Named::Slash,
        "QUOTELEFT" | "ASCIITILDE" => Named::Backquote,
        _ => return Err("a key RBS shortcuts do not use"),
    }))
}

/// One OBS binding to a chord. `from` is the platform the file was written on (its
/// modifier names), `here` the platform whose reserved chords apply.
fn chord_of(b: &Value, from: Platform, here: Platform) -> Result<Chord, String> {
    let flag = |k: &str| b.get(k).and_then(Value::as_bool).unwrap_or(false);
    let key = key_of(b.get("key").and_then(Value::as_str).unwrap_or(""))
        .map_err(|why| format!("The key is {why}."))?;
    let (ctrl, meta) = if from == Platform::MacOs {
        (flag("command"), flag("control"))
    } else {
        (flag("control"), flag("command"))
    };
    let mods = Mods {
        ctrl,
        alt: flag("alt"),
        shift: flag("shift"),
        meta,
    };
    let chord = Chord::new(mods, key).map_err(|e| format!("{e}."))?;
    if let Some(why) = chord.app_problem(here) {
        return Err(format!("{why}."));
    }
    Ok(chord)
}

fn bindings_of(v: &Value) -> &[Value] {
    v.get("bindings")
        .or(Some(v))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn hotkey(outcome: Outcome, name: &str, reason: impl Into<String>) -> ReportEntry {
    ReportEntry::new(outcome, Area::Hotkey, name, "", reason)
}

/// Converts the frontend hotkeys of the profile and the scene hotkeys of the
/// collection. `scenes` names each imported scene's OBS source index and RBS id.
pub(crate) fn read(
    ini: Option<&Ini>,
    root: &Value,
    scenes: &[(usize, SceneId, String)],
    here: Platform,
    report: &mut ImportReport,
) -> Vec<ImportedShortcut> {
    // The profile holds the hotkeys, so its paths tell first; then the collection's.
    let from = ini
        .and_then(|ini| {
            let values = ["Output", "SimpleOutput", "AdvOut"]
                .iter()
                .flat_map(|s| ini.section(s).map(|(_, v)| Value::String(v.to_owned())))
                .collect();
            file_platform(&Value::Array(values))
        })
        .or_else(|| file_platform(root))
        .unwrap_or(here);
    let mut out: Vec<ImportedShortcut> = Vec::new();
    let mut per_command: HashMap<String, usize> = HashMap::new();
    let mut add = |command: &'static str,
                   scene: Option<SceneId>,
                   action: &str,
                   list: &[Value],
                   report: &mut ImportReport| {
        for b in list {
            let chord = match chord_of(b, from, here) {
                Ok(c) => c,
                Err(why) => {
                    report.push(hotkey(Outcome::Skipped, action, why));
                    continue;
                }
            };
            let s = ImportedShortcut {
                command,
                scene,
                chord,
                obs_action: action.to_owned(),
            };
            let n = per_command.entry(s.command_for("")).or_default();
            if *n >= MAX_PER_COMMAND {
                report.push(hotkey(
                    Outcome::Skipped,
                    action,
                    format!("A command has at most {MAX_PER_COMMAND} shortcuts."),
                ));
                continue;
            }
            *n += 1;
            if !out
                .iter()
                .any(|o| o.chord == s.chord && o.command_for("") == s.command_for(""))
            {
                out.push(s);
            }
        }
    };

    if let Some(ini) = ini {
        for (action, value) in ini.section("Hotkeys") {
            let Ok(v) = serde_json::from_str::<Value>(value) else {
                continue;
            };
            // The replay entry holds its action as a key.
            let entries: Vec<(&str, &Value)> = match v.as_object() {
                Some(o) if !o.contains_key("bindings") => {
                    o.iter().map(|(k, v)| (k.as_str(), v)).collect()
                }
                _ => vec![(action, &v)],
            };
            for (action, v) in entries {
                let list = bindings_of(v);
                if list.is_empty() {
                    continue;
                }
                match FRONTEND.iter().find(|(a, ..)| *a == action) {
                    Some((_, Some(cmd), _)) => add(cmd, None, action, list, report),
                    Some((_, None, pair)) => report.push(hotkey(
                        Outcome::Skipped,
                        action,
                        format!(
                            "RBS has one shortcut that starts and stops, so only the {pair} hotkey was used."
                        ),
                    )),
                    None => report.push(hotkey(
                        Outcome::Skipped,
                        action,
                        "RBS has no command for this hotkey yet.",
                    )),
                }
            }
        }
    }

    let sources = root
        .get("sources")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for (i, s) in sources.iter().enumerate() {
        let Some(keys) = s.get("hotkeys").and_then(Value::as_object) else {
            continue;
        };
        let name = s.get("name").and_then(Value::as_str).unwrap_or("");
        let scene = scenes.iter().find(|(si, ..)| *si == i);
        for (action, v) in keys {
            let list = bindings_of(v);
            if list.is_empty() {
                continue;
            }
            let label = format!("{name}: {action}");
            match (action.as_str(), scene) {
                ("OBSBasic.SelectScene", Some((_, id, _))) => {
                    add("scene.select", Some(*id), &label, list, report);
                }
                ("OBSBasic.SelectScene", None) => report.push(hotkey(
                    Outcome::Skipped,
                    &label,
                    "The scene was not imported.",
                )),
                _ => report.push(
                    hotkey(
                        Outcome::Skipped,
                        &label,
                        "Source hotkeys are not imported yet.",
                    )
                    .with_hint("Bind it again in Settings › Shortcuts."),
                ),
            }
        }
    }
    out
}

/// Adds imported shortcuts to the user's shortcuts (ADR-0131 decision 9), in app
/// scope. A chord already held (by a user binding or a default) or refused on this
/// platform is not added, so an import never displaces a binding; neither is one past
/// the document's or a command's limit. Returns a report entry for each one left out.
pub fn merge_into(
    doc: &mut ShortcutsDoc,
    shortcuts: &[ImportedShortcut],
    collection_id: &str,
    platform: Platform,
) -> Vec<ReportEntry> {
    let mut out = Vec::new();
    for s in shortcuts {
        let command = s.command_for(collection_id);
        let keymap = Keymap::resolve(doc, platform);
        let skip = |why: String| hotkey(Outcome::Skipped, &s.obs_action, why);
        if doc.bindings.len() + doc.removed.len() >= MAX_ENTRIES {
            out.push(skip(format!(
                "The shortcuts file holds at most {MAX_ENTRIES} changes."
            )));
            continue;
        }
        if keymap.of(&command).count() >= MAX_PER_COMMAND {
            out.push(skip(format!(
                "A command has at most {MAX_PER_COMMAND} shortcuts."
            )));
            continue;
        }
        match keymap.check(&s.chord, Scope::App) {
            Check::Free | Check::Warn(_) => doc.add(&command, s.chord, Scope::App),
            Check::Taken {
                command: holder, ..
            } if holder == command => {}
            Check::Taken {
                command: holder, ..
            } => out.push(
                skip(format!("{} is already used by {holder}.", s.chord))
                    .with_hint("Pick another key in Settings › Shortcuts."),
            ),
            Check::Refused(why) => out.push(skip(why)),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn modifiers_follow_the_platform_the_file_came_from() {
        let b = json!({"key":"OBS_KEY_F5","control":true,"command":false});
        let win = chord_of(&b, Platform::Windows, Platform::Windows).unwrap();
        assert_eq!(win.to_string(), "Ctrl+F5");
        let mac = chord_of(&b, Platform::MacOs, Platform::Windows).unwrap();
        assert_eq!(mac.to_string(), "Meta+F5");
        let b = json!({"key":"OBS_KEY_MOUSE4"});
        assert!(chord_of(&b, Platform::Windows, Platform::Windows).is_err());
        let b = json!({"key":"OBS_KEY_NUM1"});
        assert!(chord_of(&b, Platform::Windows, Platform::Windows).is_err());
        let b = json!({"key":"OBS_KEY_1","shift":true});
        assert!(chord_of(&b, Platform::Windows, Platform::Windows).is_err());
    }

    #[test]
    fn merging_never_displaces_a_binding() {
        let mut doc = ShortcutsDoc::default();
        let taken = rbs_ui::shortcut::defaults()[0].1;
        let mk = |command, chord| ImportedShortcut {
            command,
            scene: None,
            chord,
            obs_action: "a".into(),
        };
        let free = Chord::parse("Ctrl+Alt+F9").unwrap();
        let out = merge_into(
            &mut doc,
            &[mk("stream.toggle", free), mk("replay.save", taken)],
            "c",
            Platform::Windows,
        );
        assert_eq!(doc.bindings.len(), 1);
        assert_eq!(doc.bindings[0].command, "stream.toggle");
        assert_eq!(out.len(), 1, "{out:?}");
        // A second import of the same binding changes nothing.
        let out = merge_into(
            &mut doc,
            &[mk("stream.toggle", free)],
            "c",
            Platform::Windows,
        );
        assert!(out.is_empty());
        assert_eq!(doc.bindings.len(), 1);
        doc.validate().unwrap();
    }

    #[test]
    fn the_file_platform_comes_from_its_paths() {
        assert_eq!(
            file_platform(&json!({"a":"C:\\x","b":["C:/y"]})),
            Some(Platform::Windows)
        );
        assert_eq!(
            file_platform(&json!({"a":"/Users/x/a.png"})),
            Some(Platform::MacOs)
        );
        assert_eq!(file_platform(&json!({"a":"rel"})), None);
        assert_eq!(file_platform(&json!({"a":"/Users/x","b":"/home/y"})), None);
    }

    #[test]
    fn pairs_map_to_toggles_and_scenes_get_their_id() {
        let ini = Ini::parse(concat!(
            "[Hotkeys]\n",
            "OBSBasic.StartStreaming={\"bindings\":[{\"key\":\"OBS_KEY_F9\",\"control\":true}]}\n",
            "OBSBasic.StopStreaming={\"bindings\":[{\"key\":\"OBS_KEY_F10\",\"control\":true}]}\n",
            "OBSBasic.Transition={\"bindings\":[{\"key\":\"OBS_KEY_T\",\"control\":true}]}\n",
            "OBSBasic.EnablePreview={\"bindings\":[]}\n",
            "ReplayBuffer={\"ReplayBuffer.Save\":[{\"key\":\"OBS_KEY_F8\",\"control\":true}]}\n",
        ));
        let root = json!({"sources":[
            {"id":"scene","name":"Main","hotkeys":{"OBSBasic.SelectScene":[{"key":"OBS_KEY_1","control":true,"alt":true}]}},
            {"id":"wasapi_input_capture","name":"Mic","hotkeys":{"libobs.mute":[{"key":"OBS_KEY_M","control":true}],"libobs.unmute":[]}}
        ]});
        let mut report = ImportReport::default();
        let out = read(
            Some(&ini),
            &root,
            &[(0, SceneId::from_raw(4), "Main".into())],
            Platform::Windows,
            &mut report,
        );
        let got: Vec<_> = out
            .iter()
            .map(|s| format!("{} {}", s.command_for("c1"), s.chord))
            .collect();
        assert_eq!(
            got,
            [
                "stream.toggle Ctrl+F9",
                "replay.save Ctrl+F8",
                "scene.select?collection=c1&scene=4 Ctrl+Alt+1",
            ]
        );
        let text = report.to_string();
        assert!(text.contains("OBSBasic.StopStreaming"), "{text}");
        assert!(text.contains("OBSBasic.Transition"), "{text}");
        assert!(!text.contains("EnablePreview"), "{text}");
        assert!(text.contains("Mic: libobs.mute"), "{text}");
        assert!(!text.contains("unmute"), "{text}");
    }
}
