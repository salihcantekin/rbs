//! The user's output profile and scene collection in use (ADR-0051): opened at start
//! (migrating the files older versions kept), and saved by one writer thread that
//! applies every write in order and keeps only the latest snapshot of each entry.
//! Toolkit-free and tested without a window.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_scene::SceneCollection;
use rbs_scene::model::SceneScope;
use rbs_ui::tokens::ThemeBody;
use rbs_user_data::{
    Appearance, Current, DataError, Document, Entry, EntryId, Library, OutputProfile, UserData,
    WriterLock,
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
/// The vertical canvas's recording settings in an output profile (ADR-0093 decision
/// 6): the fields of [`RECORDING`]; absent until saved, and then the main ones are used.
pub const RECORDING_VERTICAL: &str = "recording-vertical";
/// The vertical canvas's stream settings in an output profile: the fields of
/// [`STREAM`]; absent until saved, and then the vertical stream has no server.
pub const STREAM_VERTICAL: &str = "stream-vertical";

/// Reports a failed save, on the writer thread. Must not block.
pub type OnError = Arc<dyn Fn(String) + Send + Sync>;

/// Work for the writer thread: library changes (create, rename, delete, ...) that
/// need the writer lock, run in order with the saves.
type Task = Box<dyn FnOnce(&UserData, &WriterLock, &Names) + Send>;

/// The names entries are saved with, kept by the writer thread: a rename changes the
/// name for the saves after it, never for those before.
#[derive(Default)]
pub struct Names {
    profiles: Mutex<HashMap<EntryId, String>>,
    collections: Mutex<HashMap<EntryId, String>>,
}

impl Names {
    fn of(map: &Mutex<HashMap<EntryId, String>>, id: &EntryId) -> String {
        map.lock()
            .ok()
            .and_then(|m| m.get(id).cloned())
            .unwrap_or_else(|| id.as_str().to_owned())
    }

    fn set(map: &Mutex<HashMap<EntryId, String>>, id: &EntryId, name: &str) {
        if let Ok(mut m) = map.lock() {
            m.insert(id.clone(), name.to_owned());
        }
    }
}

/// A save or task the writer thread applies.
enum Job {
    Collection(EntryId, SceneCollection),
    Profile(EntryId, OutputProfile),
    Current(Current),
    Run(Task),
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
    names: Names,
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
            names: Names::default(),
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
        let names = &shared.names;
        let failed = match job {
            Job::Collection(id, col) => {
                let name = Names::of(&names.collections, &id);
                data.scene_collections()
                    .save(lock, &id, &Entry::new(name.clone(), col))
                    .err()
                    .map(|e| format!("The scene collection {name:?} could not be saved: {e}"))
            }
            Job::Profile(id, p) => {
                let name = Names::of(&names.profiles, &id);
                data.profiles()
                    .save(lock, &id, &Entry::new(name.clone(), p))
                    .err()
                    .map(|e| format!("The output profile {name:?} could not be saved: {e}"))
            }
            Job::Current(c) => data.set_current(lock, &c).err().map(|e| {
                format!(
                    "Which output profile and scene collection are in use could not be saved: {e}"
                )
            }),
            Job::Run(task) => {
                task(data, lock, names);
                None
            }
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
    data: UserData,
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
        saver
            .shared
            .push(Job::Profile(self.profile_id.clone(), self.profile.clone()));
        Ok(())
    }

    /// What saves the scene collection after a change, for the studio's control
    /// thread; `None` when changes are not saved (read-only).
    pub fn collection_saver(&self) -> Option<CollectionSaver> {
        self.saver.as_ref().map(|s| CollectionSaver {
            shared: Arc::clone(&s.shared),
            id: self.collection_id.clone(),
        })
    }

    /// Waits until every save asked for so far is written.
    pub fn flush(&self) {
        if let Some(s) = &self.saver {
            s.flush();
        }
    }

    /// Runs `task` on the writer thread after every save asked for so far, and waits
    /// for its result. Refused when changes are not saved (read-only).
    fn run<R: Send + 'static>(
        &self,
        task: impl FnOnce(&UserData, &WriterLock, &Names) -> R + Send + 'static,
    ) -> Result<R, String> {
        let saver = self.saver.as_ref().ok_or_else(read_only_note)?;
        let (tx, rx) = mpsc::channel();
        saver
            .shared
            .push(Job::Run(Box::new(move |data, lock, names| {
                let _ = tx.send(task(data, lock, names));
            })));
        rx.recv()
            .map_err(|_| "the user data writer stopped".to_owned())
    }

    fn save_current(&self) {
        if let Some(s) = &self.saver {
            s.shared.push(Job::Current(Current::new(
                Some(self.profile_id.clone()),
                Some(self.collection_id.clone()),
            )));
        }
    }

    /// The output profiles, by name.
    pub fn profiles(&self) -> Vec<(EntryId, String)> {
        listed(&self.data.profiles())
    }

    /// The scene collections, by name.
    pub fn collections(&self) -> Vec<(EntryId, String)> {
        listed(&self.data.scene_collections())
    }

    /// Uses the output profile `id` from now on (its sections replace
    /// [`UserSession::profile`]) and saves the choice.
    pub fn switch_profile(&mut self, id: &EntryId) -> Result<(), String> {
        // A save of the profile in use still waiting is written first.
        self.flush();
        let entry = self.data.profiles().load(id).map_err(|e| e.to_string())?;
        if let Some(s) = &self.saver {
            Names::set(&s.shared.names.profiles, id, &entry.name);
        }
        self.profile_id = id.clone();
        self.profile_name = entry.name;
        self.profile = entry.document;
        self.save_current();
        Ok(())
    }

    /// Makes an output profile named `name` with default settings, or a copy of the
    /// one in use (`copy`), and returns its id. The stream key is not copied: it
    /// belongs to one profile.
    pub fn new_profile(&self, name: &str, copy: bool) -> Result<EntryId, String> {
        let name = name.to_owned();
        let from = copy.then(|| self.profile_id.clone());
        self.run(move |data, lock, _| {
            let lib = data.profiles();
            match from {
                Some(from) => lib.duplicate(lock, &from, &name),
                None => lib.create(lock, &name, &OutputProfile::new()),
            }
            .map_err(|e| e.to_string())
        })?
    }

    /// Renames the output profile in use.
    pub fn rename_profile(&mut self, name: &str) -> Result<(), String> {
        let id = self.profile_id.clone();
        let name = rename(self, id, name, true)?;
        self.profile_name = name;
        Ok(())
    }

    /// Deletes the output profile `id`, which must not be the one in use (ADR-0051:
    /// its file is moved to `deleted/`). The caller removes its stream key.
    pub fn delete_profile(&self, id: &EntryId) -> Result<(), String> {
        if *id == self.profile_id {
            return Err("the output profile in use cannot be deleted".into());
        }
        let id = id.clone();
        self.run(move |data, lock, _| {
            data.profiles()
                .delete(lock, &id)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })?
    }

    /// Loads the scene collection `id`, to run it.
    pub fn load_collection(&self, id: &EntryId) -> Result<(String, SceneCollection), String> {
        // A save of it still waiting is written first.
        self.flush();
        let entry = self
            .data
            .scene_collections()
            .load(id)
            .map_err(|e| e.to_string())?;
        Ok((entry.name, entry.document))
    }

    /// What saves the scene collection `id` (named `name`) after a change, once it
    /// runs; `None` when changes are not saved.
    pub fn saver_for(&self, id: &EntryId, name: &str) -> Option<CollectionSaver> {
        self.saver.as_ref().map(|s| {
            Names::set(&s.shared.names.collections, id, name);
            CollectionSaver {
                shared: Arc::clone(&s.shared),
                id: id.clone(),
            }
        })
    }

    /// The scene collection `id` named `name` runs now: saves the choice.
    pub fn collection_switched(&mut self, id: &EntryId, name: &str) {
        self.collection_id = id.clone();
        self.collection_name = name.to_owned();
        self.save_current();
    }

    /// Makes a scene collection named `name`, new (one canvas of `canvas` and one
    /// scene) or a copy of the one in use (`copy`, as last saved), and returns its id.
    pub fn new_collection(
        &self,
        name: &str,
        canvas: &CanvasProfile,
        copy: bool,
    ) -> Result<EntryId, String> {
        let name = name.to_owned();
        let from = copy.then(|| self.collection_id.clone());
        let starter = starter_collection(canvas);
        self.run(move |data, lock, _| {
            let lib = data.scene_collections();
            match from {
                Some(from) => lib.duplicate(lock, &from, &name),
                None => lib.create(lock, &name, &starter),
            }
            .map_err(|e| e.to_string())
        })?
    }

    /// Renames the scene collection in use.
    pub fn rename_collection(&mut self, name: &str) -> Result<(), String> {
        let id = self.collection_id.clone();
        let name = rename(self, id, name, false)?;
        self.collection_name = name;
        Ok(())
    }

    /// Writes the output profile in use, as last saved, to `to` (one file), without
    /// what belongs to this computer only ([`scrub_profile`]).
    pub fn export_profile(&self, to: &Path) -> Result<(), String> {
        self.flush();
        self.data
            .profiles()
            .export_with(&self.profile_id, to, scrub_profile)
            .map_err(|e| e.to_string())
    }

    /// Writes the scene collection in use, as last saved, to `to` (one file).
    pub fn export_collection(&self, to: &Path) -> Result<(), String> {
        self.flush();
        self.data
            .scene_collections()
            .export(&self.collection_id, to)
            .map_err(|e| e.to_string())
    }

    /// Adds the output profile in the file `from` (a clashing name gets a number);
    /// returns its name.
    pub fn import_profile(&self, from: &Path) -> Result<String, String> {
        let from = from.to_owned();
        self.run(move |data, lock, _| {
            data.profiles()
                .import(lock, &from)
                .map(|(_, name)| name)
                .map_err(|e| e.to_string())
        })?
    }

    /// Adds the scene collection in the file `from` (a clashing name gets a number);
    /// returns its name.
    pub fn import_collection(&self, from: &Path) -> Result<String, String> {
        let from = from.to_owned();
        self.run(move |data, lock, _| {
            data.scene_collections()
                .import(lock, &from)
                .map(|(_, name)| name)
                .map_err(|e| e.to_string())
        })?
    }

    /// Deletes the scene collection `id`, which must not be the one in use (its file
    /// is moved to `deleted/`).
    pub fn delete_collection(&self, id: &EntryId) -> Result<(), String> {
        if *id == self.collection_id {
            return Err("the scene collection in use cannot be deleted".into());
        }
        let id = id.clone();
        self.run(move |data, lock, _| {
            data.scene_collections()
                .delete(lock, &id)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })?
    }
}

/// User themes and the appearance choice (ADR-0057). Writes go through the writer
/// thread like every other user data write.
impl UserSession {
    /// The user data (for reading).
    pub fn data(&self) -> &UserData {
        &self.data
    }

    /// The user themes, by name.
    pub fn themes(&self) -> Vec<(EntryId, String)> {
        listed(&self.data.themes())
    }

    /// Adds a theme; returns its id.
    pub fn create_theme(&self, name: &str, theme: ThemeBody) -> Result<EntryId, String> {
        let name = name.to_owned();
        self.run(move |data, lock, _| {
            data.themes()
                .create(lock, &name, &theme)
                .map_err(|e| e.to_string())
        })?
    }

    /// Saves a theme under `name` (a rename when it differs).
    pub fn save_theme(&self, id: &EntryId, name: &str, theme: ThemeBody) -> Result<(), String> {
        let (id, name) = (id.clone(), name.to_owned());
        self.run(move |data, lock, _| {
            data.themes()
                .save(lock, &id, &Entry::new(name, theme))
                .map_err(|e| e.to_string())
        })?
    }

    /// Renames a theme.
    pub fn rename_theme(&self, id: &EntryId, name: &str) -> Result<(), String> {
        let (id, name) = (id.clone(), name.to_owned());
        self.run(move |data, lock, _| {
            data.themes()
                .rename(lock, &id, &name)
                .map_err(|e| e.to_string())
        })?
    }

    /// Moves a theme to `deleted/`.
    pub fn delete_theme(&self, id: &EntryId) -> Result<(), String> {
        let id = id.clone();
        self.run(move |data, lock, _| {
            data.themes()
                .delete(lock, &id)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })?
    }

    /// Adds the theme in the file `from` (a clashing name gets a number); returns its
    /// id.
    pub fn import_theme(&self, from: &Path) -> Result<EntryId, String> {
        let from = from.to_owned();
        self.run(move |data, lock, _| {
            data.themes()
                .import(lock, &from)
                .map(|(id, _)| id)
                .map_err(|e| e.to_string())
        })?
    }

    /// Writes a theme to `to` (one file; nothing to scrub).
    pub fn export_theme(&self, id: &EntryId, to: &Path) -> Result<(), String> {
        self.flush();
        self.data.themes().export(id, to).map_err(|e| e.to_string())
    }

    /// Records the user's shortcut changes (ADR-0060).
    pub fn set_shortcuts(&self, doc: &rbs_ui::shortcut::ShortcutsDoc) -> Result<(), String> {
        let doc = doc.clone();
        self.run(move |data, lock, _| data.set_shortcuts(lock, &doc).map_err(|e| e.to_string()))?
    }

    /// Records the appearance choice.
    pub fn set_appearance(&self, appearance: &Appearance) -> Result<(), String> {
        let appearance = appearance.clone();
        self.run(move |data, lock, _| {
            data.set_appearance(lock, &appearance)
                .map_err(|e| e.to_string())
        })?
    }
}

/// Removes what belongs to this computer only from a profile being exported
/// (ADR-0051 decision 5): the recording folder, and "a stream key is saved" (the key
/// itself is never in a profile).
pub fn scrub_profile(p: &mut OutputProfile) {
    for name in [RECORDING, RECORDING_VERTICAL] {
        if let Ok(Some(mut rec)) = p.get::<toml::Table>(name) {
            rec.remove("folder");
            let _ = p.set(name, &rec);
        }
    }
    for name in [STREAM, STREAM_VERTICAL] {
        if let Ok(Some(mut st)) = p.get::<toml::Table>(name) {
            st.insert("key_saved".into(), toml::Value::Boolean(false));
            let _ = p.set(name, &st);
        }
    }
}

/// Renames entry `id` of the profiles (`profile`) or the collections on the writer
/// thread, so saves asked for before keep the old name and later ones the new.
/// Returns the name as saved (trimmed).
fn rename(s: &UserSession, id: EntryId, name: &str, profile: bool) -> Result<String, String> {
    let name = name.trim().to_owned();
    let given = name.clone();
    s.run(move |data, lock, names| {
        let done = if profile {
            data.profiles().rename(lock, &id, &given)
        } else {
            data.scene_collections().rename(lock, &id, &given)
        };
        done.map_err(|e| e.to_string())?;
        let map = if profile {
            &names.profiles
        } else {
            &names.collections
        };
        Names::set(map, &id, &given);
        Ok::<(), String>(())
    })??;
    Ok(name)
}

/// A library's entries by name; nothing when it cannot be listed.
fn listed<T: Document>(lib: &Library<T>) -> Vec<(EntryId, String)> {
    lib.list()
        .map(|l| l.entries.into_iter().map(|e| (e.id, e.name)).collect())
        .unwrap_or_default()
}

/// Saves one scene collection through the writer thread; cheap to clone and send.
#[derive(Clone)]
pub struct CollectionSaver {
    shared: Arc<Shared>,
    id: EntryId,
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
        self.shared
            .push(Job::Collection(self.id.clone(), collection.clone()));
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
        let data = data.clone();
        let wanted = Current::new(Some(profile_id.clone()), Some(collection_id.clone()));
        match Saver::start(data, lock, on_error) {
            Ok(s) => {
                Names::set(&s.shared.names.profiles, &profile_id, &profile_name);
                Names::set(
                    &s.shared.names.collections,
                    &collection_id,
                    &collection_name,
                );
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
            data,
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

    /// ADR-0111: the with-main choice is kept in the output profile across a restart.
    #[test]
    fn the_vertical_output_choice_is_saved_with_the_profile() {
        use crate::vertical_outputs::{SECTION, VerticalFlags};
        use rbs_api::types::OutputKind;
        let d = Dir::new("vertical-flags");
        let (mut s, _) = open(&d.0, quiet());
        let off = VerticalFlags::default().set(OutputKind::Record, false);
        s.set_section(SECTION, &off).unwrap();
        s.flush();
        drop(s);
        let (s, _) = open(&d.0, quiet());
        assert_eq!(VerticalFlags::from_section(s.section(SECTION)), off);
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

    #[test]
    fn a_rename_applies_to_saves_after_it_only() {
        let d = Dir::new("rename");
        let (mut s, mut col) = open(&d.0, quiet());
        let saver = s.collection_saver().unwrap();
        let canvas = col.canvases().next().unwrap().id;
        col.rename_canvas(canvas, "Before").unwrap();
        saver.save(&col);
        s.rename_collection("  Show  ").unwrap();
        assert_eq!(s.collection_name, "Show");
        col.rename_canvas(canvas, "After").unwrap();
        saver.save(&col);
        s.flush();
        let (name, saved) = s.load_collection(&s.collection_id.clone()).unwrap();
        assert_eq!(name, "Show");
        assert_eq!(saved, col);
        // Names are unique ignoring case.
        let other = s
            .new_collection("Other", &CanvasProfile::new(), false)
            .unwrap();
        assert_ne!(other, s.collection_id);
        assert!(s.rename_collection("other").is_err());
        assert_eq!(s.collection_name, "Show");
    }

    #[test]
    fn output_profiles_are_made_switched_and_deleted() {
        let d = Dir::new("profiles");
        let (mut s, _) = open(&d.0, quiet());
        let mut rec = RecordProfile::new();
        rec.folder = d.0.join("videos-a");
        s.set_section(RECORDING, &rec).unwrap();
        let first = s.profile_id.clone();

        let copy = s.new_profile("Copy", true).unwrap();
        let fresh = s.new_profile("Fresh", false).unwrap();
        s.switch_profile(&copy).unwrap();
        assert_eq!(s.profile_name, "Copy");
        assert_eq!(s.section::<RecordProfile>(RECORDING).unwrap().unwrap(), rec);
        s.switch_profile(&fresh).unwrap();
        assert!(s.section::<RecordProfile>(RECORDING).unwrap().is_none());
        assert_eq!(s.profiles().len(), 3);

        assert!(s.delete_profile(&fresh).is_err(), "the one in use stays");
        s.delete_profile(&first).unwrap();
        let names: Vec<_> = s.profiles().into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, ["Copy", "Fresh"]);
        // The choice is saved: the next start uses it.
        s.flush();
        drop(s);
        let (s, _) = open(&d.0, quiet());
        assert_eq!(s.profile_id, fresh);
        // A new profile never gets the deleted one's id (its stream key is gone).
        assert!(s.new_profile(DEFAULT_NAME, false).unwrap() != first);
    }

    #[test]
    fn a_read_only_app_does_not_change_the_libraries() {
        let d = Dir::new("read-only-library");
        let (s, _) = open(&d.0, quiet());
        let (mut other, _) = open(&d.0, quiet());
        assert!(!other.writable());
        assert!(other.new_profile("New", false).is_err());
        assert!(other.rename_profile("New").is_err());
        assert_eq!(other.profile_name, DEFAULT_NAME);
        drop(s);
    }

    #[test]
    fn an_exported_profile_leaves_this_computer_out_and_imports_with_a_new_name() {
        let d = Dir::new("export");
        let (mut s, _) = open(&d.0, quiet());
        let mut rec = RecordProfile::new();
        rec.folder = d.0.join("my-videos");
        rec.video_kbps = 12_000;
        s.set_section(RECORDING, &rec).unwrap();
        let mut st = StreamProfile::new();
        st.server = "rtmp://127.0.0.1/live".into();
        st.key_saved = true;
        s.set_section(STREAM, &st).unwrap();
        // The vertical canvas's sections are scrubbed the same way.
        let mut vrec = rec.clone();
        vrec.folder = d.0.join("my-shorts");
        s.set_section(RECORDING_VERTICAL, &vrec).unwrap();
        s.set_section(STREAM_VERTICAL, &st).unwrap();
        let file = d.0.join("shared.toml");
        s.export_profile(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("my-videos"), "{text}");
        assert!(!text.contains("my-shorts"), "{text}");

        assert_eq!(s.import_profile(&file).unwrap(), "Default 2");
        let (id, _) = s
            .profiles()
            .into_iter()
            .find(|(_, n)| n == "Default 2")
            .unwrap();
        s.switch_profile(&id).unwrap();
        let got: RecordProfile = s.section(RECORDING).unwrap().unwrap();
        assert_eq!(got.video_kbps, 12_000);
        assert_eq!(got.folder, RecordProfile::new().folder);
        let got: StreamProfile = s.section(STREAM).unwrap().unwrap();
        assert_eq!(got.server, st.server);
        assert!(!got.key_saved);
        let got: StreamProfile = s.section(STREAM_VERTICAL).unwrap().unwrap();
        assert!(!got.key_saved);
        let got: RecordProfile = s.section(RECORDING_VERTICAL).unwrap().unwrap();
        assert_eq!(got.folder, RecordProfile::new().folder);

        // A scene collection goes out and comes back whole.
        let col_file = d.0.join("show.toml");
        s.export_collection(&col_file).unwrap();
        assert_eq!(s.import_collection(&col_file).unwrap(), "Default 2");
    }
}
