//! Countdown and stopwatch values kept between runs (ADR-0129 decision 11; spec
//! `docs/specs/user-data.md`, "Text timers"), in `text-timers.toml`. Run state is live
//! state, not the scene collection: entries are keyed by collection id and source id,
//! at most [`MAX_ENTRIES`], the oldest change dropped first. Every timer loads stopped
//! at its saved value.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_record::TextTimer;
use rbs_scene::model::SourceId;
use rbs_sources::text::TextKind;
use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// Most entries the file keeps.
pub const MAX_ENTRIES: usize = 256;

/// One timer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The scene collection's id.
    pub collection: String,
    /// The text source's id.
    pub source: u32,
    /// `countdown` or `stopwatch`.
    pub kind: String,
    /// `running` or `stopped` (a saved running timer still loads stopped).
    pub state: String,
    /// Time counted, in milliseconds (a countdown's is the time gone).
    pub value_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    format_version: u32,
    #[serde(default)]
    timers: Vec<Entry>,
}

fn file(config: &Path) -> PathBuf {
    config.join("text-timers.toml")
}

/// The saved entries, oldest change first; none when there is no file. A file that
/// cannot be read or has another format gives none and the reason (the next save
/// rewrites it).
pub fn load(config: &Path) -> (Vec<Entry>, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(e) => return (Vec::new(), Some(format!("{}: {e}", path.display()))),
    };
    match toml::from_str::<File>(&text) {
        Ok(f) if f.format_version == FORMAT => {
            let mut timers = f.timers;
            let extra = timers.len().saturating_sub(MAX_ENTRIES);
            timers.drain(..extra);
            (timers, None)
        }
        Ok(f) => (
            Vec::new(),
            Some(format!(
                "{}: format version {} is not supported",
                path.display(),
                f.format_version
            )),
        ),
        Err(e) => (Vec::new(), Some(format!("{}: {e}", path.display()))),
    }
}

/// Writes the entries (replacing the file in one step).
pub fn save(config: &Path, timers: &[Entry]) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(&File {
        format_version: FORMAT,
        timers: timers.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// Puts `collection`'s timers now into `entries`: a changed timer moves to the end, a
/// source that `exists` and has no timer left (reset) loses its entry, and entries of
/// deleted sources stay until the cap pushes them out. True when anything changed.
pub fn merge(
    entries: &mut Vec<Entry>,
    collection: &str,
    timers: &[TextTimer],
    exists: impl Fn(SourceId) -> bool,
) -> bool {
    let before = entries.clone();
    entries.retain(|e| {
        e.collection != collection
            || !exists(SourceId::from_raw(e.source))
            || timers.iter().any(|t| t.source.get() == e.source)
    });
    for t in timers {
        let entry = Entry {
            collection: collection.to_owned(),
            source: t.source.get(),
            kind: match t.kind {
                TextKind::Countdown => "countdown",
                _ => "stopwatch",
            }
            .to_owned(),
            state: if t.running { "running" } else { "stopped" }.to_owned(),
            value_ms: u64::try_from(t.value.as_millis()).unwrap_or(u64::MAX),
        };
        let same = |e: &Entry| e.collection == collection && e.source == entry.source;
        match entries.iter().position(same) {
            Some(i) if entries[i] == entry => {}
            Some(i) => {
                entries.remove(i);
                entries.push(entry);
            }
            None => entries.push(entry),
        }
    }
    let extra = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..extra);
    *entries != before
}

/// `collection`'s saved timers, stopped at their values, for sources that still are
/// timers of the saved kind (`kind_of` answers from the collection).
pub fn restore(
    entries: &[Entry],
    collection: &str,
    kind_of: impl Fn(SourceId) -> Option<TextKind>,
) -> Vec<TextTimer> {
    entries
        .iter()
        .filter(|e| e.collection == collection)
        .filter_map(|e| {
            let source = SourceId::from_raw(e.source);
            let kind = kind_of(source)?;
            let saved = match e.kind.as_str() {
                "countdown" => TextKind::Countdown,
                "stopwatch" => TextKind::Stopwatch,
                _ => return None,
            };
            (kind == saved).then_some(TextTimer {
                source,
                kind,
                running: false,
                value: Duration::from_millis(e.value_ms),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    fn timer(source: u32, kind: TextKind, running: bool, ms: u64) -> TextTimer {
        TextTimer {
            source: SourceId::from_raw(source),
            kind,
            running,
            value: Duration::from_millis(ms),
        }
    }

    #[test]
    fn timers_round_trip_and_load_stopped() {
        let dir = std::env::temp_dir().join(format!("rbs-text-timers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), (Vec::new(), None));
        let mut entries = Vec::new();
        let now = [
            timer(1, TextKind::Countdown, true, 1500),
            timer(2, TextKind::Stopwatch, false, 42),
        ];
        assert!(merge(&mut entries, "a", &now, |_| true));
        assert!(!merge(&mut entries, "a", &now, |_| true), "nothing changed");
        save(&dir, &entries).unwrap();
        let (loaded, note) = load(&dir);
        assert_eq!((loaded.clone(), note), (entries, None));
        let kinds = |s: SourceId| match s.get() {
            1 => Some(TextKind::Countdown),
            2 => Some(TextKind::Clock),
            _ => None,
        };
        // Stopped at its value; a source that is no longer a stopwatch is skipped.
        assert_eq!(
            restore(&loaded, "a", kinds),
            vec![timer(1, TextKind::Countdown, false, 1500)]
        );
        assert!(restore(&loaded, "b", kinds).is_empty());

        std::fs::write(file(&dir), "format_version = 9\n").unwrap();
        assert!(load(&dir).1.unwrap().contains("format version 9"));
        std::fs::write(file(&dir), "not toml [").unwrap();
        assert_eq!(load(&dir).0, Vec::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resets_leave_deleted_sources_stay_and_the_cap_drops_the_oldest() {
        let mut entries = Vec::new();
        merge(
            &mut entries,
            "a",
            &[
                timer(1, TextKind::Stopwatch, false, 1),
                timer(2, TextKind::Stopwatch, false, 2),
            ],
            |_| true,
        );
        // Source 1 was reset, source 2 deleted: 1 goes, 2 stays.
        merge(&mut entries, "a", &[], |s| s.get() == 1);
        assert_eq!(entries.iter().map(|e| e.source).collect::<Vec<_>>(), [2]);
        for n in 0..300 {
            merge(
                &mut entries,
                &format!("c{n}"),
                &[timer(5, TextKind::Stopwatch, false, n)],
                |_| true,
            );
        }
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0].collection, "c44");
        assert_eq!(entries.last().unwrap().collection, "c299");
    }
}
