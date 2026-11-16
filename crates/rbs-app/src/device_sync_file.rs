//! `device-sync.toml`: the sync offset remembered for each audio device on this
//! computer (ADR-0126 decision 7, spec `docs/specs/user-data.md`).
//!
//! The file sits beside `app.toml` in the config directory, outside the `UserData`
//! library, like `calibration.toml`: device ids belong to this computer, so it is never
//! part of a profile or collection and never exported, imported or synced. It is read
//! once at app start and written when a committed offset change updates an entry
//! (decision 9). Only device-backed audio types have entries ([`remembers`]).

use std::path::{Path, PathBuf};

use rbs_sources::types::{AUDIO_INPUT_SOURCE, AUDIO_OUTPUT_SOURCE};
use serde::{Deserialize, Serialize};

/// The file's format version.
pub const FORMAT: u32 = 1;

/// The file's `kind`.
pub const KIND: &str = "device-sync";

/// At most this many devices are kept; the least recently set go first.
pub const MAX_ENTRIES: usize = 256;

/// Which device an entry is for: the source type and its `adapter` and `target`
/// settings, kept as separate strings because a device id may contain any character.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceKey {
    /// Source type id (`org.rbs.source.audio-input` or `org.rbs.source.audio-output`).
    #[serde(rename = "type")]
    pub type_id: String,
    /// Adapter id.
    pub adapter: String,
    /// The adapter's device id.
    pub target: String,
}

/// True for the source types whose offset is remembered with their device: audio
/// input (camera microphone companions included) and audio output. Application audio
/// targets a process, not a device.
pub fn remembers(type_id: &str) -> bool {
    type_id == AUDIO_INPUT_SOURCE || type_id == AUDIO_OUTPUT_SOURCE
}

/// How an entry's value was set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Applied from a measurement.
    Measured,
    /// Typed or dragged in the Sync offset row, or set through the control API.
    Manual,
}

/// The reference picture a measured value was measured against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    /// The reference source's device, when it has `adapter` and `target` settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<DeviceKey>,
    /// The reference source's display name when measured.
    pub name: String,
}

/// One device's remembered offset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// The device.
    #[serde(flatten)]
    pub key: DeviceKey,
    /// Offset in milliseconds, as the source's `sync_offset_ms`.
    pub offset_ms: i32,
    /// How it was set.
    pub origin: Origin,
    /// For a measured value, what it was measured against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,
    /// When it was set, in seconds since the Unix epoch (UTC).
    pub set_at: u64,
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSyncFile {
    /// [`FORMAT`].
    pub format: u32,
    /// [`KIND`].
    pub kind: String,
    /// One entry per device, at most [`MAX_ENTRIES`].
    #[serde(default)]
    pub devices: Vec<DeviceEntry>,
}

impl Default for DeviceSyncFile {
    fn default() -> Self {
        Self {
            format: FORMAT,
            kind: KIND.to_owned(),
            devices: Vec::new(),
        }
    }
}

impl DeviceSyncFile {
    /// The entry of `key`.
    pub fn get(&self, key: &DeviceKey) -> Option<&DeviceEntry> {
        self.devices.iter().find(|e| e.key == *key)
    }

    /// The remembered offset of `key`, 0 without an entry.
    pub fn offset_ms(&self, key: &DeviceKey) -> i32 {
        self.get(key).map_or(0, |e| e.offset_ms)
    }

    /// Sets the entry of its key, replacing any other. Keeps the newest
    /// [`MAX_ENTRIES`]. `false` when the same value was already there (only `set_at`
    /// would change), so the caller need not write the file.
    pub fn set(&mut self, entry: DeviceEntry) -> bool {
        match self.devices.iter_mut().find(|e| e.key == entry.key) {
            Some(old)
                if old.offset_ms == entry.offset_ms
                    && old.origin == entry.origin
                    && old.reference == entry.reference =>
            {
                false
            }
            Some(old) => {
                *old = entry;
                true
            }
            None => {
                self.devices.push(entry);
                if self.devices.len() > MAX_ENTRIES {
                    self.devices.sort_by_key(|e| std::cmp::Reverse(e.set_at));
                    self.devices.truncate(MAX_ENTRIES);
                }
                true
            }
        }
    }

    /// Removes the entry of `key`; `false` when there was none.
    pub fn forget(&mut self, key: &DeviceKey) -> bool {
        let before = self.devices.len();
        self.devices.retain(|e| e.key != *key);
        self.devices.len() != before
    }

    /// The offset a source of device `new` gets when it is retargeted from `old` with
    /// `current` (ADR-0126 decision 8): the new device's value (0 without an entry)
    /// when `current` is the old device's value (0 without an entry, or when `old`
    /// remembers nothing), else `current`, kept.
    pub fn retarget_offset_ms(
        &self,
        old: Option<&DeviceKey>,
        new: Option<&DeviceKey>,
        current: i32,
    ) -> i32 {
        let was = old.map_or(0, |k| self.offset_ms(k));
        if current != was {
            return current;
        }
        new.map_or(0, |k| self.offset_ms(k))
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("device-sync.toml")
}

/// The remembered offsets; an empty file when there is none. A file that cannot be
/// read, is of another kind or a newer format gives an empty file and a message; it
/// is replaced when an entry is next written.
pub fn load(config: &Path) -> (DeviceSyncFile, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (DeviceSyncFile::default(), None);
        }
        Err(e) => {
            return (
                DeviceSyncFile::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<DeviceSyncFile>(&text) {
        Ok(f) if f.format != FORMAT || f.kind != KIND => (
            DeviceSyncFile::default(),
            Some(format!(
                "{}: {} format {} is not supported",
                path.display(),
                f.kind,
                f.format
            )),
        ),
        Ok(f) => (f, None),
        Err(e) => (
            DeviceSyncFile::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the file (replacing it in one step).
pub fn save(config: &Path, file_data: &DeviceSyncFile) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(file_data).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(target: &str) -> DeviceKey {
        DeviceKey {
            type_id: AUDIO_INPUT_SOURCE.to_owned(),
            adapter: "org.rbs.audio.cpal".to_owned(),
            target: target.to_owned(),
        }
    }

    fn entry(target: &str, offset_ms: i32, set_at: u64) -> DeviceEntry {
        DeviceEntry {
            key: key(target),
            offset_ms,
            origin: Origin::Manual,
            reference: None,
            set_at,
        }
    }

    #[test]
    fn only_device_audio_types_are_remembered() {
        assert!(remembers(AUDIO_INPUT_SOURCE));
        assert!(remembers(AUDIO_OUTPUT_SOURCE));
        assert!(!remembers(rbs_sources::types::APPLICATION_AUDIO_SOURCE));
        assert!(!remembers(rbs_sources::types::CAMERA_SOURCE));
    }

    #[test]
    fn set_replaces_by_key_and_reports_a_real_change() {
        let mut f = DeviceSyncFile::default();
        assert!(f.set(entry("a", 40, 1)));
        assert!(!f.set(entry("a", 40, 2)), "same value: nothing to write");
        assert!(f.set(entry("a", -20, 3)));
        assert_eq!(f.devices.len(), 1);
        assert_eq!(f.offset_ms(&key("a")), -20);
        assert_eq!(f.offset_ms(&key("b")), 0);
        assert!(f.forget(&key("a")));
        assert!(!f.forget(&key("a")));
    }

    #[test]
    fn the_least_recently_set_devices_go_first() {
        let mut f = DeviceSyncFile::default();
        for i in 0..=MAX_ENTRIES as u64 {
            f.set(entry(&format!("d{i}"), 1, i));
        }
        assert_eq!(f.devices.len(), MAX_ENTRIES);
        assert!(f.get(&key("d0")).is_none());
        assert!(f.get(&key(&format!("d{MAX_ENTRIES}"))).is_some());
    }

    #[test]
    fn retarget_follows_the_device_only_when_the_offset_was_the_old_devices() {
        let mut f = DeviceSyncFile::default();
        f.set(entry("old", 30, 1));
        f.set(entry("new", -15, 1));
        let (old, new, none) = (key("old"), key("new"), key("none"));
        // The old device's value moves to the new device's.
        assert_eq!(f.retarget_offset_ms(Some(&old), Some(&new), 30), -15);
        // A value of its own is kept.
        assert_eq!(f.retarget_offset_ms(Some(&old), Some(&new), 12), 12);
        // No entry for the old device: 0 counts as its value.
        assert_eq!(f.retarget_offset_ms(Some(&none), Some(&new), 0), -15);
        assert_eq!(f.retarget_offset_ms(Some(&none), Some(&new), 5), 5);
        // A device without an entry gives 0.
        assert_eq!(f.retarget_offset_ms(Some(&old), Some(&none), 30), 0);
        // A source type that remembers nothing on either side.
        assert_eq!(f.retarget_offset_ms(None, None, 0), 0);
    }

    #[test]
    fn the_file_round_trips_and_a_newer_format_is_ignored() {
        let dir = std::env::temp_dir().join(format!("rbs-device-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), (DeviceSyncFile::default(), None));
        let mut f = DeviceSyncFile::default();
        f.set(entry("a\"b]=c", 40, 1));
        f.set(DeviceEntry {
            origin: Origin::Measured,
            reference: Some(Reference {
                key: Some(DeviceKey {
                    type_id: rbs_sources::types::CAMERA_SOURCE.to_owned(),
                    adapter: "org.rbs.capture.v4l2".to_owned(),
                    target: "/dev/video0".to_owned(),
                }),
                name: "Camera".to_owned(),
            }),
            ..entry("m", -25, 2)
        });
        assert!(save(&dir, &f).is_ok());
        assert_eq!(load(&dir), (f, None));
        let text = std::fs::read_to_string(dir.join("device-sync.toml")).unwrap_or_default();
        assert!(
            text.contains("type = \"org.rbs.source.audio-input\""),
            "{text}"
        );
        assert!(text.contains("origin = \"measured\""), "{text}");

        let newer = "format = 2\nkind = \"device-sync\"\n";
        assert!(std::fs::write(dir.join("device-sync.toml"), newer).is_ok());
        let (f, message) = load(&dir);
        assert_eq!(f, DeviceSyncFile::default());
        assert!(message.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
