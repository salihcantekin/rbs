//! The replay buffer in the host (ADR-0088): arming and disarming, saving a clip on a
//! worker thread, and the state the UI and the control API show.
//!
//! The buffer encodes like the saved recording profile ("Match the recording", the only
//! mode in this version) and keeps the length and memory limit of [`ReplaySetup`]. A
//! clip request is answered by the ring node on its next tick; this module collects it
//! on the host's next round and hands it to a short-lived writer thread, so capture, the
//! buffer and every output keep running while the file is written.

use std::path::PathBuf;
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_record::{AudioSettings, Clip, ClipWriter, ReplaySettings};

use super::{Host, Msg, Update};
use crate::replay_settings::ReplaySetup;

/// How often the buffer's state goes out while it is armed (no timer while disarmed:
/// ADR-0013), and how soon the host looks again for a requested clip.
const STATE_INTERVAL: Duration = Duration::from_secs(1);
const CLIP_POLL: Duration = Duration::from_millis(20);

/// What the buffer holds, for the UI and `replay.status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayState {
    /// Armed: the buffer runs and keeps the past.
    pub armed: bool,
    /// Usable past in milliseconds.
    pub held_ms: u64,
    /// The wanted length in seconds.
    pub length_secs: u32,
    /// Payload bytes held.
    pub bytes: u64,
    /// The memory limit in bytes.
    pub memory_limit: u64,
    /// Memory, not the length setting, is why the past is short.
    pub memory_limited: bool,
    /// A clip is being cut or written.
    pub saving: bool,
}

/// The host's replay state.
pub(super) struct ReplayHost {
    /// What the user chose.
    pub setup: ReplaySetup,
    /// Armed by the user: kept across studio restarts, which re-arm it.
    pub wanted: bool,
    /// A clip was requested and the ring node has not answered yet.
    pub pending: bool,
    /// Writer threads running.
    pub writing: u32,
    /// When the state goes out next; `None` while disarmed.
    pub due: Option<Timestamp>,
    /// The state last published.
    pub published: Option<ReplayState>,
}

impl ReplayHost {
    pub(super) fn new() -> Self {
        Self {
            setup: ReplaySetup::default(),
            wanted: false,
            pending: false,
            writing: 0,
            due: None,
            published: None,
        }
    }
}

impl Host {
    /// The studio's replay settings from the saved recording profile and the user's
    /// replay setup.
    fn replay_settings(&self) -> Result<ReplaySettings, String> {
        if let Some(why) = self.replay.setup.problem() {
            return Err(why);
        }
        let audio = self
            .replay
            .setup
            .audio
            .then(|| {
                self.adapters.audio_encoders.ids().next().map(|_| {
                    let mut a = AudioSettings::default();
                    a.bitrate_bps = self.profile.encoding().audio_bps();
                    a
                })
            })
            .flatten();
        Ok(ReplaySettings::new(
            self.profile.video_settings(),
            audio,
            self.replay.setup.length(),
            self.replay.setup.memory_limit(),
        ))
    }

    /// Arms the buffer. An error says why not; the buffer stays disarmed.
    pub(super) fn arm_replay(&mut self, notify: &dyn Fn(Update)) -> Result<(), String> {
        if self.studio.replay_armed() {
            return Ok(());
        }
        let settings = self.replay_settings()?;
        self.studio
            .arm_replay(&self.adapters, &settings)
            .map_err(|e| format!("The replay buffer did not start: {e}"))?;
        self.replay.wanted = true;
        self.replay.due = Some(self.clock.now());
        self.replay_changed(notify);
        Ok(())
    }

    /// Disarms the buffer and frees what it held.
    pub(super) fn disarm_replay(&mut self, notify: &dyn Fn(Update)) {
        self.studio.disarm_replay();
        self.replay.wanted = false;
        self.replay.pending = false;
        self.replay.due = None;
        self.replay_changed(notify);
    }

    /// Asks the buffer for a clip of the last `seconds` (all of it for `None`). The file
    /// is written when the ring node has cut it ([`Host::report_replay`]).
    pub(super) fn save_replay(&mut self, seconds: Option<u32>) -> Result<(), String> {
        if !self.studio.replay_armed() {
            return Err("The replay buffer is not armed.".into());
        }
        if self.replay.pending {
            return Err("A clip is being cut already.".into());
        }
        let length = seconds.map(|s| Duration::from_secs(u64::from(s)));
        match self.studio.request_replay_clip(length) {
            Some(Ok(())) => {
                self.replay.pending = true;
                Ok(())
            }
            Some(Err(_)) => Err("A clip is being cut already.".into()),
            None => Err("The replay buffer is not armed.".into()),
        }
    }

    /// Re-arms the buffer in a studio that was made again (a canvas or collection
    /// change); says why when it cannot.
    pub(super) fn rearm_replay(&mut self, notify: &dyn Fn(Update)) {
        if !self.replay.wanted {
            return;
        }
        self.replay.pending = false;
        if let Err(why) = self.arm_replay(notify) {
            self.replay.wanted = false;
            self.replay.due = None;
            notify(Update::Error(why));
            self.replay_changed(notify);
        }
    }

    /// The buffer as the UI and `replay.status` show it.
    pub(super) fn replay_state(&self) -> ReplayState {
        let status = self.studio.replay_status();
        let setup = &self.replay.setup;
        ReplayState {
            armed: status.is_some(),
            held_ms: status.map_or(0, |s| s.held_ms),
            length_secs: setup.length_secs,
            bytes: status.map_or(0, |s| s.bytes),
            memory_limit: setup.memory_limit() as u64,
            memory_limited: status.is_some_and(|s| s.memory_limited),
            saving: self.replay.pending || self.replay.writing > 0,
        }
    }

    /// Tells the UI when the state differs from the last one told.
    pub(super) fn replay_changed(&mut self, notify: &dyn Fn(Update)) {
        let now = self.replay_state();
        if self.replay.published.as_ref() == Some(&now) {
            return;
        }
        self.replay.published = Some(now.clone());
        notify(Update::Replay(now));
    }

    /// Once per round: collects a requested clip and starts its writer, and sends the
    /// state once a second while armed. Returns the time until it wants to be called
    /// again, `None` while there is nothing to do (no timer: ADR-0013).
    pub(super) fn report_replay(
        &mut self,
        now: Timestamp,
        notify: &dyn Fn(Update),
    ) -> Option<Duration> {
        if !self.studio.replay_armed() {
            if self.replay.wanted && self.replay.due.is_some() {
                // The studio lost the buffer (it stopped by itself): say so once.
                self.replay.wanted = false;
                self.replay.due = None;
                self.replay.pending = false;
                notify(Update::Error("The replay buffer stopped.".into()));
            }
            self.replay_changed(notify);
            return None;
        }
        let mut wait = None;
        if self.replay.pending {
            match self.studio.take_replay_clip() {
                Some(result) => {
                    self.replay.pending = false;
                    match result {
                        Ok(clip) => self.write_clip(clip, notify),
                        Err(e) => notify(Update::ReplaySaved(Err(format!(
                            "The clip could not be cut: {e}"
                        )))),
                    }
                }
                None => wait = Some(CLIP_POLL),
            }
        }
        let due = *self.replay.due.get_or_insert(now);
        if now >= due {
            self.replay.due = Some(now + STATE_INTERVAL);
            self.replay_changed(notify);
        }
        let until = self
            .replay
            .due
            .map_or(STATE_INTERVAL, |d| d.saturating_since(now));
        Some(wait.map_or(until, |w| w.min(until)))
    }

    /// Starts the worker that writes `clip` to a new file; the answer comes back as a
    /// message to the host.
    fn write_clip(&mut self, clip: Clip, notify: &dyn Fn(Update)) {
        let result = self.clip_job(clip);
        match result {
            Ok(job) => {
                self.replay.writing += 1;
                let inbox = std::sync::Weak::clone(&self.inbox);
                let spawned = std::thread::Builder::new()
                    .name("rbs-replay-clip".into())
                    .spawn(move || {
                        let answer = job();
                        if let Some(tx) = inbox.upgrade() {
                            let _ = tx.send(Msg::ReplayWritten(answer));
                        }
                    });
                if let Err(e) = spawned {
                    self.replay.writing -= 1;
                    notify(Update::ReplaySaved(Err(format!(
                        "The clip writer could not start: {e}"
                    ))));
                }
            }
            Err(why) => notify(Update::ReplaySaved(Err(why))),
        }
        self.replay_changed(notify);
    }

    /// Everything the writer thread needs, chosen now on the control thread: the
    /// adapters, the streams and a free file name.
    fn clip_job(
        &self,
        clip: Clip,
    ) -> Result<Box<dyn FnOnce() -> Result<PathBuf, String> + Send>, String> {
        let streams = self
            .studio
            .replay_streams()
            .ok_or("The replay buffer is not armed.")?
            .to_vec();
        let folder = self
            .replay
            .setup
            .folder
            .clone()
            .unwrap_or_else(|| self.profile.folder.clone());
        std::fs::create_dir_all(&folder)
            .map_err(|e| format!("Cannot make the folder {}: {e}", folder.display()))?;
        let stamp = chrono::Local::now()
            .naive_local()
            .format("%Y-%m-%d %H-%M-%S");
        let ext = self.profile.format.extension();
        let mut path = folder.join(format!("RBS Replay {stamp}.{ext}"));
        let mut n = 2;
        while path.exists() {
            path = folder.join(format!("RBS Replay {stamp} ({n}).{ext}"));
            n += 1;
        }
        let container = self.profile.format.container();
        let writer = ClipWriter::new(
            &self.adapters,
            &rbs_transport_file::file_url(&path),
            container,
        )
        .map_err(|e| e.to_string())?;
        Ok(Box::new(move || {
            match writer.write(&clip, &streams) {
                Ok(_) => Ok(path),
                Err(e) => {
                    // A half-written file is not a clip.
                    if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
                        let _ = std::fs::remove_file(&path);
                    }
                    Err(format!("The clip could not be written: {e}"))
                }
            }
        }))
    }

    /// A writer thread finished.
    pub(super) fn replay_written(
        &mut self,
        result: Result<PathBuf, String>,
        notify: &dyn Fn(Update),
    ) {
        self.replay.writing = self.replay.writing.saturating_sub(1);
        notify(Update::ReplaySaved(result));
        self.replay_changed(notify);
    }
}
