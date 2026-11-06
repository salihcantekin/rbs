//! The user's output profile and scene collection in use (ADR-0051): opened at start
//! (migrating the files older versions kept), and saved by one writer thread that
//! applies every write in order and keeps only the latest snapshot of each entry.
//! Toolkit-free and tested without a window.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_scene::SceneCollection;
use rbs_scene::model::SceneScope;
use rbs_user_data::{
    Current, DataError, Document, Entry, EntryId, Library, OutputProfile, UserData, WriterLock,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::canvas_profile::{self, CanvasProfile};
use crate::record_profile;
use crate::stream_profile;

/// Name of the entries made when there are none, and of the migrated ones.
pub const DEFAULT_NAME: &str = "Default";

/// The profile section of the recording settings ([`record_profile::RecordProfile`]).
pub const RECORDING: &str = "recording";
/// The profile section of the stream settings ([`stream_profile::StreamProfile`]).
pub const STREAM: &str = "stream";

/// Reports a failed save, on the writer thread. Must not block.
pub type OnError = Arc<dyn Fn(String) + Send + Sync>;

/// A save the writer thread applies.
enum Job {
    Collection(EntryId, String, SceneCollection),
    Profile(EntryId, String, OutputProfile),
    Current(Current),
}

impl Job {
    /// A newer job of the same entry replaces one still waiting at the end of the queue.
    fn replaces(&self, older: &Job) -> bool {
        match (self, older) {
            (Job::Collection(a, ..), Job::Collection(b, ..)) => a == b,
            (Job::Profile(a, ..), Job::Profile(b, ..)) => a == b,
            (Job::Current(_), Job::Current(_)) => true,
            _ => false,
        }
    }
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    busy: bool,
    stop: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
}

impl Shared {
    fn push(&self, job: Job) {
        let Ok(mut q) = self.queue.lock() else {
            return;
        };
        if q.jobs.back().is_some_and(|last| job.replaces(last)) {
            q.jobs.pop_back();
        }
        q.jobs.push_back(job);
        self.changed.notify_all();
    }
}

/// The one writer of user data (ADR-0051 decision 8): a thread that applies saves in
/// the order they were asked for. A save still waiting at the end of the queue is
/// replaced by a newer save of the same entry, so a burst of edits is one write. Holds
/// the writer lock. Dropping it writes what is waiting, then stops the thread.
pub struct Saver {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    fn start(data: UserData, lock: WriterLock, on_error: OnError) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            changed: Condvar::new(),
        });
        let thread = std::thread::Builder::new()
            .name("rbs-user-data".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || write_jobs(&shared, &data, &lock, &*on_error)
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Waits until every save asked for so far is written (or has failed).
    pub fn flush(&self) {
        let Ok(mut q) = self.shared.queue.lock() else {
            return;
        };
        while !q.jobs.is_empty() || q.busy {
            q = match self.shared.changed.wait(q) {
                Ok(q) => q,
                Err(_) => return,
            };
        }
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        if let Ok(mut q) = self.shared.queue.lock() {
            q.stop = true;
            self.shared.changed.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn write_jobs(shared: &Shared, data: &UserData, lock: &WriterLock, on_error: &dyn Fn(String)) {
    loop {
        let job = {
            let Ok(mut q) = shared.queue.lock() else {
                return;
            };
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    q.busy = true;
                    break job;
                }
                if q.stop {
                    return;
                }
                q = match shared.changed.wait(q) {
                    Ok(q) => q,
                    Err(_) => return,
                };
            }
        };
        let failed = match &job {
            Job::Collection(id, name, col) => data
                .scene_collections()
                .save(lock, id, &Entry::new(name.clone(), col.clone()))
                .err()
                .map(|e| format!("The scene collection {name:?} could not be saved: {e}")),
            Job::Profile(id, name, p) => data
                .profiles()
                .save(lock, id, &Entry::new(name.clone(), p.clone()))
                .err()
                .map(|e| format!("The output profile {name:?} could not be saved: {e}")),
            Job::Current(c) => data.set_current(lock, c).err().map(|e| {
                format!(
                    "Which output profile and scene collection are in use could not be saved: {e}"
                )
            }),
        };
        if let Some(why) = failed {
            on_error(why);
        }
        if let Ok(mut q) = shared.queue.lock() {
            q.busy = false;
            shared.changed.notify_all();
        }
    }
}

/// The output profile and scene collection in use.
pub struct UserSession {
    saver: Option<Saver>,
    /// The output profile's id.
    pub profile_id: EntryId,
    /// Its name.
    pub profile_name: String,
    /// Its sections as last saved or loaded.
    pub profile: OutputProfile,
    /// The scene collection's id.
    pub collection_id: EntryId,
    /// Its name.
    pub collection_name: String,
    /// What the user should know: entries that could not be read, read-only mode.
    pub notes: Vec<String>,
}

impl UserSession {
    /// True if this app holds the writer lock and saves changes.
    pub fn writable(&self) -> bool {
        self.saver.is_some()
    }

    /// A profile section; `Err` with the reason if it is there but cannot be used.
    pub fn section<S: DeserializeOwned>(&self, name: &str) -> Result<Option<S>, String> {
        self.profile.get(name).map_err(|e| e.to_string())
    }

    /// Replaces a profile section and saves the profile. Refused, with the reason,
    /// when the saved section was written by a newer RBS, or when another RBS holds the
    /// user data.
    pub fn set_section<S: Serialize>(&mut self, name: &str, value: &S) -> Result<(), String> {
        let Some(saver) = &self.saver else {
            return Err(read_only_note());
        };
        match self.profile.set(name, value) {
            Ok(()) => {}
            Err(DataError::Newer { .. }) => {
                return Err(
                    "they were saved by a newer RBS, so this version does not change them".into(),
                );
            }
            Err(e) => return Err(e.to_string()),
        }
        saver.shared.push(Job::Profile(
            self.profile_id.clone(),
            self.profile_name.clone(),
            self.profile.clone(),
        ));
        Ok(())
    }

    /// What saves the scene collection after a change, for the studio's control
    /// thread; `None` when changes are not saved (read-only).
    pub fn collection_saver(&self) -> Option<CollectionSaver> {
        self.saver.as_ref().map(|s| CollectionSaver {
            shared: Arc::clone(&s.shared),
            id: self.collection_id.clone(),
            name: self.collection_name.clone(),
        })
    }

    /// Waits until every save asked for so far is written.
    pub fn flush(&self) {
        if let Some(s) = &self.saver {
            s.flush();
        }
    }
}

/// Saves one scene collection through the writer thread; cheap to clone and send.
#[derive(Clone)]
pub struct CollectionSaver {
    shared: Arc<Shared>,
    id: EntryId,
    name: String,
}

impl std::fmt::Debug for CollectionSaver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectionSaver")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl CollectionSaver {
    /// Asks the writer thread to save `collection` (replacing a save of it still
    /// waiting).
    pub fn save(&self, collection: &SceneCollection) {
        self.shared.push(Job::Collection(
            self.id.clone(),
            self.name.clone(),
            collection.clone(),
        ));
    }
}

fn read_only_note() -> String {
    "another RBS is running with the same settings, so changes here are not saved".into()
}

/// A collection with one canvas of `canvas` and one shared scene on air: what a new
/// user starts with.
pub fn starter_collection(canvas: &CanvasProfile) -> SceneCollection {
    let mut col = SceneCollection::new();
    let made = match col.add_canvas("Main", canvas.size(), canvas.frame_rate()) {
        Ok((c, _)) => Some(c),
        Err(_) => {
            let d = CanvasProfile::new();
            col.add_canvas("Main", d.size(), d.frame_rate())
                .ok()
                .map(|(c, _)| c)
        }
    };
    if let (Some(c), Ok((scene, _))) = (made, col.add_scene("Scene", SceneScope::Shared)) {
        let _ = col.set_program(c, Some(scene));
    }
    col
}

/// The id of the entry to use: `wanted` if it is listed, else the first one listed,
/// else a new "Default" made by `make` (when writable).
fn pick<T: Document>(
    lib: &Library<T>,
    lock: Option<&WriterLock>,
    wanted: Option<&EntryId>,
    what: &str,
    make: impl FnOnce() -> T,
    notes: &mut Vec<String>,
) -> Option<(EntryId, String)> {
    let listing = match lib.list() {
        Ok(l) => l,
        Err(e) => {
            notes.push(format!("The {what}s could not be listed: {e}"));
            return None;
        }
    };
    for (path, why) in &listing.problems {
        notes.push(format!("{} was not loaded: {why}", path.display()));
    }
    if let Some(found) = wanted
        .and_then(|w| listing.entries.iter().find(|e| &e.id == w))
        .or_else(|| listing.entries.first())
    {
        return Some((found.id.clone(), found.name.clone()));
    }
    let lock = lock?;
    match lib.create(lock, DEFAULT_NAME, &make()) {
        Ok(id) => Some((id, DEFAULT_NAME.to_owned())),
        Err(e) => {
            notes.push(format!("A new {what} could not be made: {e}"));
            None
        }
    }
}

/// The output profile made from the files RBS kept before profiles existed
/// (`recording.toml`, `stream.toml`): a section for each file that exists and loads.
fn migrated_profile(config: &Path, notes: &mut Vec<String>) -> OutputProfile {
    let mut p = OutputProfile::new();
    if config.join("recording.toml").exists() {
        match record_profile::load(config) {
            (rec, None) => {
                if let Err(e) = p.set(RECORDING, &rec) {
                    notes.push(format!("The old recording settings were not moved: {e}"));
                }
            }
            (_, Some(w)) => notes.push(format!("The old recording settings were not moved: {w}")),
        }
    }
    if config.join("stream.toml").exists() {
        match stream_profile::load(config) {
            (st, None) => {
                if let Err(e) = p.set(STREAM, &st) {
                    notes.push(format!("The old stream settings were not moved: {e}"));
                }
            }
            (_, Some(w)) => notes.push(format!("The old stream settings were not moved: {w}")),
        }
    }
    p
}

/// Opens the user data in `config`: takes the writer lock (else read-only, with a
/// note), picks the entries named in `current.toml` (else the first listed, else new
/// "Default" ones; when the library's folder does not exist yet, they are made from
/// the files older versions kept: `recording.toml` and `stream.toml` for the profile,
/// `video.toml` for the collection's canvas; those files stay) and loads them. A collection that cannot be loaded is replaced by a starter
/// collection, with a note; its file is moved aside when it is first saved over
/// (ADR-0051). `on_error` reports failed saves later, on the writer thread.
pub fn open(config: &Path, on_error: OnError) -> (UserSession, SceneCollection) {
    let data = UserData::new(config);
    let mut notes = Vec::new();
    let lock = match data.try_lock() {
        Ok(Some(l)) => Some(l),
        Ok(None) => {
            notes.push(format!(
                "Output profiles and scene collections: {}.",
                read_only_note()
            ));
            None
        }
        Err(e) => {
            notes.push(format!(
                "Output profiles and scene collections cannot be saved: {e}"
            ));
            None
        }
    };
    let (legacy_canvas, _) = canvas_profile::load(config);
    let profiles = data.profiles();
    let collections = data.scene_collections();
    // The old files are used only for a library that never existed (ADR-0051 decision
    // 9): a user who deleted every profile gets a new, default one.
    let fresh_profiles = !profiles.dir().exists();
    let fresh_collections = !collections.dir().exists();
    let new_canvas = if fresh_collections {
        legacy_canvas
    } else {
        CanvasProfile::new()
    };
    let (current, why) = data.current();
    if let Some(e) = why {
        notes.push(format!("current.toml was not used: {e}"));
    }
    let profile_entry = pick(
        &profiles,
        lock.as_ref(),
        current.profile.as_ref(),
        "output profile",
        || {
            if fresh_profiles {
                migrated_profile(config, &mut notes_sink())
            } else {
                OutputProfile::new()
            }
        },
        &mut notes,
    );
    let collection_entry = pick(
        &collections,
        lock.as_ref(),
        current.scene_collection.as_ref(),
        "scene collection",
        || starter_collection(&new_canvas),
        &mut notes,
    );
    let fallback = || (EntryId::from_name(DEFAULT_NAME), DEFAULT_NAME.to_owned());
    let (profile_id, profile_name) = profile_entry.unwrap_or_else(fallback);
    let (collection_id, collection_name) = collection_entry.unwrap_or_else(fallback);
    let profile = match profiles.load(&profile_id) {
        Ok(e) => e.document,
        Err(DataError::NotFound(_)) => OutputProfile::new(),
        Err(e) => {
            notes.push(format!(
                "The output profile {profile_name:?} could not be loaded, so default settings are used: {e}"
            ));
            OutputProfile::new()
        }
    };
    let collection = match collections.load(&collection_id) {
        Ok(e) => e.document,
        Err(DataError::NotFound(_)) => starter_collection(&new_canvas),
        Err(e) => {
            notes.push(format!(
                "The scene collection {collection_name:?} could not be loaded, so a new one is used; the file is kept beside it: {e}"
            ));
            starter_collection(&new_canvas)
        }
    };
    let saver = lock.and_then(|lock| {
        let wanted = Current::new(Some(profile_id.clone()), Some(collection_id.clone()));
        match Saver::start(data.clone(), lock, on_error) {
            Ok(s) => {
                if wanted != current {
                    s.shared.push(Job::Current(wanted));
                }
                Some(s)
            }
            Err(e) => {
                notes.push(format!("Changes cannot be saved: {e}"));
                None
            }
        }
    });
    (
        UserSession {
            saver,
            profile_id,
            profile_name,
            profile,
            collection_id,
            collection_name,
            notes,
        },
        collection,
    )
}

/// Migration notes are about files the user cannot act on any more; they go nowhere.
fn notes_sink() -> Vec<String> {
    Vec::new()
}

/// The first canvas's size and rate; the starter canvas's when there is none.
pub fn first_canvas(collection: &SceneCollection) -> (Size, FrameRate) {
    collection.canvases().next().map_or_else(
        || {
            let d = CanvasProfile::new();
            (d.size(), d.frame_rate())
        },
        |c| (c.size, c.frame_rate),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::record_profile::RecordProfile;
    use crate::stream_profile::StreamProfile;
    use rbs_core::media::Size;

    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let d = std::env::temp_dir()
                .join(format!("rbs-app-session-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn quiet() -> OnError {
        Arc::new(|_| {})
    }

    #[test]
    fn a_first_start_makes_default_entries_from_the_old_files() {
        let d = Dir::new("migrate");
        let mut rec = RecordProfile::new();
        rec.folder = d.0.join("videos-old");
        record_profile::save(&d.0, &rec).unwrap();
        let mut st = StreamProfile::new();
        st.server = "rtmp://127.0.0.1/live".into();
        stream_profile::save(&d.0, &st).unwrap();
        let mut canvas = CanvasProfile::new();
        canvas.width = 1280;
        canvas.height = 720;
        canvas_profile::save(&d.0, &canvas).unwrap();

        let (s, col) = open(&d.0, quiet());
        assert!(s.writable());
        assert_eq!(s.profile_name, "Default");
        assert_eq!(s.profile_id.as_str(), "default");
        assert_eq!(s.section::<RecordProfile>(RECORDING).unwrap().unwrap(), rec);
        assert_eq!(s.section::<StreamProfile>(STREAM).unwrap().unwrap(), st);
        assert_eq!(first_canvas(&col).0, Size::new(1280, 720));
        let canvas_id = col.canvases().next().unwrap().id;
        assert!(col.canvas(canvas_id).unwrap().program.is_some());
        s.flush();
        drop(s);
        // The old files stay; the next start uses the entries, not the files.
        assert!(d.0.join("recording.toml").exists());
        std::fs::remove_file(d.0.join("video.toml")).unwrap();
        let (s, col2) = open(&d.0, quiet());
        assert_eq!(col2, col);
        assert_eq!(s.section::<RecordProfile>(RECORDING).unwrap().unwrap(), rec);
        assert!(s.notes.is_empty(), "{:?}", s.notes);
    }

    #[test]
    fn old_files_are_not_used_again_once_the_libraries_exist() {
        let d = Dir::new("no-remigrate");
        let mut rec = RecordProfile::new();
        rec.folder = d.0.join("videos-old");
        record_profile::save(&d.0, &rec).unwrap();
        let (s, _) = open(&d.0, quiet());
        s.flush();
        drop(s);
        // The user deletes the only profile: the next one is new, not the old file.
        std::fs::remove_file(d.0.join("profiles").join("default.toml")).unwrap();
        let (s, _) = open(&d.0, quiet());
        assert!(s.section::<RecordProfile>(RECORDING).unwrap().is_none());
    }

    #[test]
    fn changes_are_saved_and_a_second_app_is_read_only() {
        let d = Dir::new("save");
        let (mut s, mut col) = open(&d.0, quiet());
        let mut rec = RecordProfile::new();
        rec.folder = d.0.join("videos-new");
        s.set_section(RECORDING, &rec).unwrap();
        let saver = s.collection_saver().unwrap();
        let canvas = col.canvases().next().unwrap().id;
        col.rename_canvas(canvas, "Horizontal").unwrap();
        for _ in 0..5 {
            saver.save(&col);
        }

        // A second app while the first runs: loads, saves nothing.
        let (mut other, seen) = open(&d.0, quiet());
        assert!(!other.writable());
        assert!(other.collection_saver().is_none());
        assert!(other.set_section(RECORDING, &rec).is_err());
        assert_eq!(other.notes.len(), 1, "{:?}", other.notes);
        let _ = seen;
        drop(other);

        s.flush();
        drop(s);
        let (s, loaded) = open(&d.0, quiet());
        assert_eq!(loaded, col);
        assert_eq!(s.section::<RecordProfile>(RECORDING).unwrap().unwrap(), rec);
    }

    #[test]
    fn a_collection_that_does_not_load_is_kept_and_replaced() {
        let d = Dir::new("broken");
        let (s, _) = open(&d.0, quiet());
        let path = d.0.join("scenes").join("default.toml");
        s.flush();
        drop(s);
        let broken = "format = 1\nkind = \"scene-collection\"\nname = \"Default\"\ntypo = 1\n";
        std::fs::write(&path, broken).unwrap();
        let (s, col) = open(&d.0, quiet());
        assert!(s.notes.iter().any(|n| n.contains("could not be loaded")));
        assert_eq!(col.canvases().count(), 1);
        s.collection_saver().unwrap().save(&col);
        s.flush();
        assert_eq!(
            std::fs::read_to_string(d.0.join("scenes").join("default.toml.unreadable.bak"))
                .unwrap(),
            broken
        );
    }

    #[test]
    fn failed_saves_are_reported() {
        let d = Dir::new("fail");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let (s, col) = open(&d.0, Arc::new(move |why| sink.lock().unwrap().push(why)));
        s.flush();
        // A directory where the file should be: the save fails.
        let path = d.0.join("scenes").join("default.toml");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        s.collection_saver().unwrap().save(&col);
        s.flush();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("could not be saved"), "{seen:?}");
    }
}
