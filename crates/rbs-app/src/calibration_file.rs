//! `calibration.toml`: what *Test this computer* measured on this computer (ADR-0087
//! decision 4, specs `docs/specs/output-estimate.md#calibrationtoml` and
//! `docs/specs/user-data.md`).
//!
//! The file sits beside `app.toml` in the config directory, outside the `UserData`
//! library: never part of a profile or collection, never exported, imported or
//! synced. It is read once at app start and kept until a calibration rewrites it, so
//! no start waits for it. Entries that no longer describe this computer are ignored
//! when read ([`CalibrationFile::for_estimate`]) and dropped when the file is next
//! written ([`CalibrationFile::prune`]).

use std::path::{Path, PathBuf};

use rbs_record::estimate::{
    Calibration, EncoderMeasure, FpsClass, RateClass, SizeClass, StopReason,
};
use serde::{Deserialize, Serialize};

/// The file's format version.
pub const FORMAT: u32 = 1;

/// The file's `kind`.
pub const KIND: &str = "calibration";

/// At most this many entries of each kind are kept; the oldest go first.
pub const MAX_ENTRIES: usize = 64;

/// One encoder result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderEntry {
    /// Adapter id.
    pub adapter: String,
    /// The adapter's `adapter.version` when it reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Codec name.
    pub codec: String,
    /// Size class (`720`, `1080`, `1440`, `2160`, `above-2160`).
    pub size: String,
    /// Frame-rate class (`30`, `60`, `above-60`).
    pub fps: String,
    /// Rate-control class (`cbr`, `vbr`, `cq`).
    pub rate: String,
    /// Highest session count that ran with no late frame.
    pub sessions: u32,
    /// Highest pixel rate that ran with no late frame.
    pub pixels_per_second: u64,
    /// Encode time of one frame with one session, in microseconds.
    pub frame_time_us: u64,
    /// When it was measured, in seconds since the Unix epoch.
    pub measured_at: u64,
    /// The series ended before a step failed or an open was refused (the time
    /// limit, a cancel or an error after step 1): `sessions` is a lower bound.
    #[serde(default)]
    pub partial: bool,
    /// Why the series stopped (`refused`, `late`, `ceiling`, `limit`, `cancelled`,
    /// `error`). Absent in a file written by hand: `limit` when `partial`, else
    /// `late`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<String>,
}

impl EncoderEntry {
    /// What [`EncoderEntry::stopped_by`] says, with the reading for an absent or
    /// unknown value.
    pub fn stop_reason(&self) -> StopReason {
        match self.stopped_by.as_deref() {
            Some("refused") => StopReason::Refused,
            Some("late") => StopReason::Late,
            Some("ceiling") => StopReason::Ceiling,
            Some("limit") => StopReason::Limit,
            Some("cancelled") => StopReason::Cancelled,
            Some("error") => StopReason::Error,
            _ if self.partial => StopReason::Limit,
            _ => StopReason::Late,
        }
    }
}

/// The file name of a [`StopReason`].
pub fn stop_reason_name(r: StopReason) -> &'static str {
    match r {
        StopReason::Refused => "refused",
        StopReason::Late => "late",
        StopReason::Ceiling => "ceiling",
        StopReason::Limit => "limit",
        StopReason::Cancelled => "cancelled",
        StopReason::Error => "error",
    }
}

/// One ISO export result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportEntry {
    /// Size class.
    pub size: String,
    /// Export time of one frame, in microseconds.
    pub time_us: u64,
    /// When it was measured, in seconds since the Unix epoch.
    pub measured_at: u64,
    /// The run stopped at its time limit before this entry finished.
    #[serde(default)]
    pub partial: bool,
}

/// One recording folder's write speed. The path stays in this local file only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderEntry {
    /// The recording folder.
    pub path: PathBuf,
    /// Bytes per second written.
    pub write_speed: u64,
    /// When it was measured, in seconds since the Unix epoch.
    pub measured_at: u64,
    /// The run stopped at its time limit before this entry finished.
    #[serde(default)]
    pub partial: bool,
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationFile {
    /// [`FORMAT`].
    pub format: u32,
    /// [`KIND`].
    pub kind: String,
    /// The RBS version that last wrote the file; entries of adapters without
    /// `adapter.version` hold only while it is unchanged.
    #[serde(default)]
    pub rbs_version: String,
    /// Encoder results.
    #[serde(default)]
    pub encoders: Vec<EncoderEntry>,
    /// ISO export results.
    #[serde(default)]
    pub exports: Vec<ExportEntry>,
    /// Folder write speeds.
    #[serde(default)]
    pub folders: Vec<FolderEntry>,
}

impl Default for CalibrationFile {
    fn default() -> Self {
        Self {
            format: FORMAT,
            kind: KIND.to_owned(),
            rbs_version: String::new(),
            encoders: Vec::new(),
            exports: Vec::new(),
            folders: Vec::new(),
        }
    }
}

/// What this computer is now: the adapters that probe, with their `adapter.version`,
/// and the running RBS version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThisComputer {
    /// Adapter id and version of every adapter that probes.
    pub adapters: Vec<(String, Option<String>)>,
    /// The running RBS version.
    pub rbs_version: String,
}

impl ThisComputer {
    fn encoder_holds(&self, e: &EncoderEntry, file_version: &str) -> bool {
        self.adapters.iter().any(|(id, version)| {
            *id == e.adapter
                && *version == e.version
                && (version.is_some() || file_version == self.rbs_version)
        })
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("calibration.toml")
}

/// The saved results; an empty file when there is none. A file that cannot be read,
/// is of another kind or a newer format gives an empty file and a message; it is
/// replaced when a calibration next writes.
pub fn load(config: &Path) -> (CalibrationFile, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (CalibrationFile::default(), None);
        }
        Err(e) => {
            return (
                CalibrationFile::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<CalibrationFile>(&text) {
        Ok(f) if f.format != FORMAT || f.kind != KIND => (
            CalibrationFile::default(),
            Some(format!(
                "{}: {} format {} is not supported",
                path.display(),
                f.kind,
                f.format
            )),
        ),
        Ok(f) => (f, None),
        Err(e) => (
            CalibrationFile::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the file (replacing it in one step). Call [`CalibrationFile::prune`] first.
pub fn save(config: &Path, calibration: &CalibrationFile) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(calibration).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

fn encoder_measure(e: &EncoderEntry) -> Option<EncoderMeasure> {
    Some(EncoderMeasure {
        adapter: e.adapter.clone(),
        codec: e.codec.clone(),
        size: SizeClass::parse(&e.size)?,
        fps: FpsClass::parse(&e.fps)?,
        rate: RateClass::parse(&e.rate)?,
        sessions: e.sessions,
        pixels_per_second: e.pixels_per_second,
        frame_time: std::time::Duration::from_micros(e.frame_time_us),
        stopped_by: e.stop_reason(),
    })
}

/// Keeps the newest [`MAX_ENTRIES`] by `measured_at`.
fn keep_newest<T>(entries: &mut Vec<T>, at: impl Fn(&T) -> u64) {
    if entries.len() > MAX_ENTRIES {
        entries.sort_by_key(|e| std::cmp::Reverse(at(e)));
        entries.truncate(MAX_ENTRIES);
    }
}

impl CalibrationFile {
    /// The results that still describe this computer, for the estimate of a start
    /// that records into `folder`. Entries with a class name this build does not know
    /// are skipped. Export results hold while the RBS version is unchanged.
    pub fn for_estimate(&self, now: &ThisComputer, folder: &Path) -> Calibration {
        let same_rbs = self.rbs_version == now.rbs_version;
        Calibration {
            encoders: self
                .encoders
                .iter()
                .filter(|e| now.encoder_holds(e, &self.rbs_version))
                .filter_map(encoder_measure)
                .collect(),
            exports: if same_rbs {
                self.exports
                    .iter()
                    .filter_map(|e| {
                        Some((
                            SizeClass::parse(&e.size)?,
                            std::time::Duration::from_micros(e.time_us),
                        ))
                    })
                    .collect()
            } else {
                Vec::new()
            },
            write_speed: self
                .folders
                .iter()
                .find(|f| f.path == folder)
                .map(|f| f.write_speed),
        }
    }

    /// Drops what no longer describes this computer before the file is written:
    /// encoder entries of adapters that no longer probe or whose version changed (and
    /// of adapters without a version once the RBS version changed), export entries
    /// after an RBS version change, folders that no longer exist; then keeps the
    /// newest [`MAX_ENTRIES`] of each kind and stamps the RBS version.
    pub fn prune(&mut self, now: &ThisComputer, folder_exists: impl Fn(&Path) -> bool) {
        let file_version = std::mem::take(&mut self.rbs_version);
        self.encoders
            .retain(|e| now.encoder_holds(e, &file_version));
        if file_version != now.rbs_version {
            self.exports.clear();
        }
        self.folders.retain(|f| folder_exists(&f.path));
        keep_newest(&mut self.encoders, |e| e.measured_at);
        keep_newest(&mut self.exports, |e| e.measured_at);
        keep_newest(&mut self.folders, |e| e.measured_at);
        self.rbs_version.clone_from(&now.rbs_version);
    }

    /// Takes a run's results (ADR-0087 step D1): drops what no longer holds first (so
    /// the new entries are stamped with this RBS version), then merges. A new encoder
    /// entry replaces the one of the same adapter, version and class, except that a
    /// lower bound (`limit`, `cancelled`, `error`) replaces it only with a higher
    /// count. A complete folder entry replaces the old one; a partial one only a
    /// partial one.
    pub fn merge_run(
        &mut self,
        now: &ThisComputer,
        encoders: Vec<EncoderEntry>,
        folder: Option<FolderEntry>,
        folder_exists: impl Fn(&Path) -> bool,
    ) {
        self.prune(now, &folder_exists);
        for new in encoders {
            let same = |e: &EncoderEntry| {
                e.adapter == new.adapter
                    && e.version == new.version
                    && e.codec == new.codec
                    && e.size == new.size
                    && e.fps == new.fps
                    && e.rate == new.rate
            };
            match self.encoders.iter_mut().find(|e| same(e)) {
                Some(old) => {
                    if !new.stop_reason().lower_bound() || new.sessions > old.sessions {
                        *old = new;
                    }
                }
                None => self.encoders.push(new),
            }
        }
        if let Some(new) = folder {
            match self.folders.iter_mut().find(|f| f.path == new.path) {
                Some(old) => {
                    if !new.partial || old.partial {
                        *old = new;
                    }
                }
                None => self.folders.push(new),
            }
        }
        self.prune(now, &folder_exists);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoder(adapter: &str, version: Option<&str>, at: u64) -> EncoderEntry {
        EncoderEntry {
            adapter: adapter.to_owned(),
            version: version.map(str::to_owned),
            codec: "h264".to_owned(),
            size: "1080".to_owned(),
            fps: "60".to_owned(),
            rate: "cbr".to_owned(),
            sessions: 3,
            pixels_per_second: 1_000,
            frame_time_us: 2_000,
            measured_at: at,
            partial: false,
            stopped_by: None,
        }
    }

    #[test]
    fn a_lower_bound_replaces_only_with_a_higher_count() {
        let now = computer("1.0");
        let mut f = CalibrationFile::default();
        let late = EncoderEntry {
            stopped_by: Some("late".into()),
            ..encoder("vt", None, 1)
        };
        f.merge_run(&now, vec![late.clone()], None, |_| true);
        let same = EncoderEntry {
            stopped_by: Some("cancelled".into()),
            partial: true,
            ..encoder("vt", None, 2)
        };
        f.merge_run(&now, vec![same], None, |_| true);
        assert_eq!(f.encoders, vec![late]);
        let higher = EncoderEntry {
            stopped_by: Some("limit".into()),
            partial: true,
            sessions: 5,
            ..encoder("vt", None, 3)
        };
        f.merge_run(&now, vec![higher.clone()], None, |_| true);
        assert_eq!(f.encoders, vec![higher]);
        let full = EncoderEntry {
            stopped_by: Some("refused".into()),
            sessions: 2,
            ..encoder("vt", None, 4)
        };
        f.merge_run(&now, vec![full.clone()], None, |_| true);
        assert_eq!(f.encoders, vec![full]);
    }

    #[test]
    fn a_partial_folder_entry_does_not_replace_a_complete_one() {
        let now = computer("1.0");
        let mut f = CalibrationFile::default();
        let folder = |partial, speed| FolderEntry {
            path: PathBuf::from("/rec"),
            write_speed: speed,
            measured_at: 1,
            partial,
        };
        f.merge_run(&now, vec![], Some(folder(false, 100)), |_| true);
        f.merge_run(&now, vec![], Some(folder(true, 50)), |_| true);
        assert_eq!(f.folders[0].write_speed, 100);
        f.merge_run(&now, vec![], Some(folder(false, 70)), |_| true);
        assert_eq!(f.folders[0].write_speed, 70);
    }

    fn computer(rbs: &str) -> ThisComputer {
        ThisComputer {
            adapters: vec![
                ("nv".to_owned(), Some("560".to_owned())),
                ("vt".to_owned(), None),
            ],
            rbs_version: rbs.to_owned(),
        }
    }

    fn sample() -> CalibrationFile {
        CalibrationFile {
            rbs_version: "0.1.0".to_owned(),
            encoders: vec![
                encoder("nv", Some("560"), 1),
                encoder("nv", Some("555"), 2),
                encoder("vt", None, 3),
                encoder("gone", None, 4),
            ],
            exports: vec![ExportEntry {
                size: "1080".to_owned(),
                time_us: 900,
                measured_at: 5,
                partial: false,
            }],
            folders: vec![FolderEntry {
                path: PathBuf::from("/rec"),
                write_speed: 200,
                measured_at: 6,
                partial: false,
            }],
            ..CalibrationFile::default()
        }
    }

    #[test]
    fn only_entries_that_still_describe_this_computer_reach_the_estimate() {
        let f = sample();
        let c = f.for_estimate(&computer("0.1.0"), Path::new("/rec"));
        let adapters: Vec<&str> = c.encoders.iter().map(|e| e.adapter.as_str()).collect();
        assert_eq!(adapters, ["nv", "vt"]);
        assert_eq!(
            c.encoders[0].frame_time,
            std::time::Duration::from_millis(2)
        );
        assert_eq!(
            c.exports,
            [(SizeClass::L1080, std::time::Duration::from_micros(900))]
        );
        assert_eq!(c.write_speed, Some(200));
        assert_eq!(
            f.for_estimate(&computer("0.1.0"), Path::new("/other"))
                .write_speed,
            None
        );

        // A new RBS version: entries without an adapter version and exports go.
        let c = f.for_estimate(&computer("0.2.0"), Path::new("/rec"));
        let adapters: Vec<&str> = c.encoders.iter().map(|e| e.adapter.as_str()).collect();
        assert_eq!(adapters, ["nv"]);
        assert!(c.exports.is_empty());
    }

    #[test]
    fn prune_drops_stale_entries_and_keeps_the_newest_sixty_four() {
        let mut f = sample();
        f.prune(&computer("0.1.0"), |p| p != Path::new("/rec"));
        assert_eq!(f.encoders.len(), 2);
        assert!(f.folders.is_empty());
        assert_eq!(f.exports.len(), 1);

        let mut f = CalibrationFile {
            rbs_version: "0.1.0".to_owned(),
            encoders: (0..70).map(|i| encoder("nv", Some("560"), i)).collect(),
            ..CalibrationFile::default()
        };
        f.prune(&computer("0.1.0"), |_| true);
        assert_eq!(f.encoders.len(), MAX_ENTRIES);
        assert!(f.encoders.iter().all(|e| e.measured_at >= 6));
        assert_eq!(f.rbs_version, "0.1.0");
    }

    #[test]
    fn the_file_round_trips_and_a_newer_format_is_ignored() {
        let dir = std::env::temp_dir().join(format!("rbs-calibration-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), (CalibrationFile::default(), None));
        let f = sample();
        assert!(save(&dir, &f).is_ok());
        assert_eq!(load(&dir), (f, None));

        let newer = "format = 2\nkind = \"calibration\"\n";
        assert!(std::fs::write(dir.join("calibration.toml"), newer).is_ok());
        let (f, message) = load(&dir);
        assert_eq!(f, CalibrationFile::default());
        assert!(message.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
