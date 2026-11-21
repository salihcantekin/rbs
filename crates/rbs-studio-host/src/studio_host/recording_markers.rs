//! A recording's markers until they are written into its manifest (ADR-0159 decision 8).

use super::*;

/// The markers of a running recording and where its manifest goes (ADR-0089
/// decisions 1 and 5). The recording's state on the control thread owns them.
#[derive(Debug, Clone)]
pub(super) struct RecordingMarkers {
    pub(super) log: rbs_manifest::MarkerLog,
    /// The manifest to write: the start manifest of a recording folder, or the
    /// master alone for a plain recording.
    pub(super) manifest: rbs_manifest::RecordingManifest,
    /// Its folder and file name.
    pub(super) dir: PathBuf,
    pub(super) name: String,
    /// A recording folder's manifest exists from the start; a plain recording
    /// writes one only when it has markers.
    pub(super) folder: bool,
    /// The manifest changed since start for another reason than markers (the
    /// pictures of image tracks, the stop fields), so the stop writes it.
    pub(super) dirty: bool,
    /// What each file holds, once the recording stopped (ADR-0089 decision 6): the
    /// master's, then each separate file's by name.
    pub(super) ended: Option<(Option<rbs_record::MediaSpan>, SeparateMedia)>,
    /// The editor project to write after the stop rewrite (ADR-0089 decision 10).
    pub(super) project: Option<crate::record_profile::EditorProject>,
    /// The automatic markers this recording takes (ADR-0089 decision 3).
    pub(super) settings: crate::record_profile::MarkerSettings,
    /// The program scene last marked (or on air at the start), for scene markers.
    pub(super) scene: Option<SceneId>,
    /// When the recording stopped, from `t0` (master time): the end of a pause still
    /// open then (ADR-0124 decision 10).
    pub(super) stopped_ns: u64,
    /// When the marker flush runs (host clock, ns): armed by the first marker not
    /// yet on disk, `None` while nothing waits (ADR-0089 decision 5).
    pub(super) flush_at: Option<u64>,
    /// A flush wrote the manifest during the recording.
    pub(super) flushed: bool,
    /// The master is Matroska and the root writes chapters: a clean stop adds the
    /// markers as chapters (ADR-0089 decision 14).
    pub(super) chapters: Option<Arc<dyn ChapterWriter>>,
}

impl RecordingMarkers {
    /// The markers' state for a recording that started at `t0_ns` into `path`, or into
    /// the recording folder of `iso`.
    pub(super) fn new(
        path: &Path,
        t0_ns: u64,
        t0_utc: String,
        iso: Option<&IsoFiles>,
        project: Option<crate::record_profile::EditorProject>,
        settings: crate::record_profile::MarkerSettings,
        scene: Option<SceneId>,
    ) -> Option<Self> {
        let log = rbs_manifest::MarkerLog::new(t0_ns, settings.lead()).ok()?;
        if let Some(iso) = iso {
            return Some(Self {
                log,
                manifest: iso.manifest.clone(),
                dir: iso.folder.clone(),
                name: rbs_manifest::FOLDER_MANIFEST.to_owned(),
                folder: true,
                dirty: false,
                ended: None,
                project,
                settings,
                scene,
                stopped_ns: 0,
                flush_at: None,
                flushed: false,
                chapters: None,
            });
        }
        let file = path.file_name()?.to_str()?;
        let name = rbs_manifest::manifest_name_for(file)?;
        let mut manifest = rbs_manifest::RecordingManifest::new(&t0_utc, t0_ns);
        manifest
            .files
            .push(rbs_manifest::ManifestFile::listed("master", file));
        Some(Self {
            log,
            manifest,
            dir: path.parent()?.to_path_buf(),
            name,
            folder: false,
            dirty: false,
            ended: None,
            project,
            settings,
            scene,
            stopped_ns: 0,
            flush_at: None,
            flushed: false,
            chapters: None,
        })
    }

    /// Keeps what the closed files hold, for the stop rewrite in [`Self::write`].
    pub(super) fn set_ended(
        &mut self,
        master: Option<rbs_record::MediaSpan>,
        separate: SeparateMedia,
    ) {
        self.ended = Some((master, separate));
        self.dirty = true;
    }

    /// The stop rewrite (ADR-0089 decision 6): each file's offset, length and format,
    /// and `complete = true`. A file that wrote nothing keeps no timing. When a file
    /// began before `t0` (a plain recording's first audio packet before the moment its
    /// markers count from), `t0` moves to it so no offset is negative.
    pub(super) fn apply_ended(&mut self) {
        let Some((master, separate)) = self.ended.take() else {
            return;
        };
        let span_of = |file: &rbs_manifest::ManifestFile| {
            if file.role == "master" {
                master
            } else {
                separate
                    .iter()
                    .find(|(name, _)| *name == file.file_name)
                    .and_then(|(_, span)| *span)
            }
        };
        let earliest = self
            .manifest
            .files
            .iter()
            .filter_map(span_of)
            .map(|s| s.first_ns)
            .min();
        if let Some(first) = earliest {
            self.manifest.move_origin(first);
        }
        let t0 = self.manifest.t0_ns;
        let timings: Vec<_> = self
            .manifest
            .files
            .iter()
            .map(|f| span_of(f).map(|s| s.timing(t0)))
            .collect();
        for (file, timing) in self.manifest.files.iter_mut().zip(timings) {
            if file.role != "image" {
                file.timing = timing;
            }
        }
        // Without the master's timing (a take that did not keep it, or wrote nothing)
        // the manifest stays incomplete, so no project is ever guessed.
        if master.is_some() {
            self.manifest.complete();
        }
    }

    /// Writes the editor project next to the manifest, when the setting asked for
    /// one and the stop rewrite made the manifest complete. `Ok(None)`: none asked.
    pub(super) fn write_project(&self) -> Result<Option<String>, Refusal> {
        let Some(project) = self.project else {
            return Ok(None);
        };
        let format = match project {
            crate::record_profile::EditorProject::Fcpxml => rbs_project::Format::Fcpxml,
            crate::record_profile::EditorProject::Xmeml => rbs_project::Format::Xmeml,
        };
        let mut options = rbs_project::Options::default();
        options.pause_markers = self.settings.pause_markers;
        rbs_project::export_with(&self.dir, &self.name, format, false, options)
            .map(|done| {
                done.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .map_err(|e| Refusal::Failed(e.to_string()))
    }

    /// The markers as chapters inside a Matroska master (ADR-0089 decision 14), after
    /// the stop rewrite of a clean stop. Only when the master's timing is known (the
    /// span is tracked for separate files and the editor project setting): a chapter
    /// is at `t_ns - start_offset_ns`, from the file's first sample. A marker before it
    /// is on it; one after the master's end is left out (the manifest keeps it). A file
    /// that cannot take chapters is left as it is; only a read or write failure is an
    /// error.
    pub(super) fn write_chapters(&self) -> Result<(), Refusal> {
        let Some(writer) = self
            .chapters
            .as_ref()
            .filter(|_| !self.log.markers().is_empty())
        else {
            return Ok(());
        };
        let Some((file, timing)) = self
            .manifest
            .files
            .iter()
            .find(|f| f.role == "master")
            .and_then(|f| f.timing.map(|t| (f, t)))
        else {
            return Ok(());
        };
        let mut chapters: Vec<Chapter<'_>> = self
            .log
            .markers()
            .iter()
            .map(|m| Chapter {
                start_ns: m.t_ns.saturating_sub(timing.start_offset_ns),
                title: &m.label,
            })
            .filter(|c| c.start_ns <= timing.duration_ns)
            .collect();
        if chapters.is_empty() {
            return Ok(());
        }
        chapters.sort_by_key(|c| c.start_ns);
        let mut media = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join(&file.file_name))
            .map_err(|e| Refusal::Failed(e.to_string()))?;
        // `NotSupported`: the file takes no chapters, and the manifest keeps the markers.
        writer
            .append(&mut media, &chapters)
            .map(|_| ())
            .map_err(Refusal::Failed)
    }

    /// The recording paused (`true`) or resumed at `at_ns` on the master clock: later
    /// markers and the manifest are in file time. A pause changes the manifest. A
    /// pause cut at a shared output records how far its join may be off.
    pub(super) fn paused(&mut self, pause: bool, at_ns: u64, join_error: Option<Duration>) {
        if pause {
            self.log
                .pause_cut(at_ns, join_error.map(|e| e.as_nanos() as u64));
        } else {
            self.log.resume(at_ns);
        }
        self.dirty = true;
    }

    /// Writes the manifest with the markers, at stop. Nothing to write is `Ok`.
    pub(super) fn write(&mut self) -> Result<(), Refusal> {
        // A plain recording writes a manifest only when it has markers or writes an
        // editor project (ADR-0089 decision 5, owner choice 1).
        if !self.folder && self.log.markers().is_empty() && self.project.is_none() {
            // A flush wrote one for markers since taken back: it goes.
            if self.flushed {
                self.flushed = false;
                let _ = std::fs::remove_file(self.dir.join(&self.name));
            }
            return Ok(());
        }
        if !self.dirty && !self.log.has_pending() {
            return Ok(());
        }
        self.manifest.markers = self.log.markers().to_vec();
        self.manifest.pauses = self.log.pauses().rows(self.stopped_ns);
        self.apply_ended();
        rbs_manifest::write(&self.dir, &self.name, &self.manifest)
            .map_err(|e| Refusal::Failed(e.to_string()))?;
        self.log.mark_flushed();
        self.dirty = false;
        Ok(())
    }

    /// The marker flush (ADR-0089 decision 5): the manifest as it stands, with the
    /// markers so far, rewritten while the recording runs, so a crash loses at most
    /// the last 5 seconds of markers. `now_ns` is the host clock. The stop still
    /// rewrites it whole.
    pub(super) fn flush(&mut self, now_ns: u64) -> Result<(), Refusal> {
        self.flush_at = None;
        if !self.log.has_pending() {
            return Ok(());
        }
        if !self.folder && self.log.markers().is_empty() && self.project.is_none() {
            // Every marker was taken back: a plain recording keeps no manifest.
            self.log.mark_flushed();
            if self.flushed {
                self.flushed = false;
                return std::fs::remove_file(self.dir.join(&self.name))
                    .map_err(|e| Refusal::Failed(e.to_string()));
            }
            return Ok(());
        }
        self.manifest.markers = self.log.markers().to_vec();
        let ran = now_ns.saturating_sub(self.log.t0_ns());
        self.manifest.pauses = self.log.pauses().rows(ran);
        rbs_manifest::write(&self.dir, &self.name, &self.manifest)
            .map_err(|e| Refusal::Failed(e.to_string()))?;
        self.log.mark_flushed();
        self.flushed = true;
        // The stop rewrite still runs: an open pause and the stop fields change.
        self.dirty = true;
        Ok(())
    }

    /// Lists the pictures each image track wrote, with its counters, for the stop
    /// write (ADR-0117 decision 6).
    pub(super) fn set_images(&mut self, tracks: &[rbs_record::IsoImageTrack]) {
        for track in tracks {
            let source = rbs_manifest::SourceRef::new(track.source.get());
            let Some(file) = self
                .manifest
                .files
                .iter_mut()
                .find(|f| f.role == "image" && f.source == Some(source))
            else {
                continue;
            };
            file.images = track.files.entries();
            file.image_counts = Some(rbs_manifest::ImageCounts {
                coalesced: track
                    .stats
                    .coalesced
                    .load(std::sync::atomic::Ordering::Relaxed),
                over_cap: track
                    .stats
                    .over_cap
                    .load(std::sync::atomic::Ordering::Relaxed),
            });
            self.dirty = true;
        }
    }
}

pub(super) fn marker_color(
    c: Option<rbs_api::request::MarkerColor>,
) -> Option<rbs_manifest::MarkerColor> {
    use rbs_api::request::MarkerColor as A;
    use rbs_manifest::MarkerColor as M;
    c.map(|c| match c {
        A::Blue => M::Blue,
        A::Red => M::Red,
        A::Orange => M::Orange,
        A::Yellow => M::Yellow,
        A::Green => M::Green,
        A::Purple => M::Purple,
    })
}
