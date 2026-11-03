//! The window's side of the studio (ADR-0041 decision 7): state shown in the `Studio`
//! global, commands to the control thread, the meters' 30 Hz refresh while the Mixer is
//! visible, the Program panel's preview layer, and recording with its output settings.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_app::record_profile::{self, AUDIO_KBPS, Format, RecordProfile, VIDEO_KBPS};
use rbs_app::studio_host::{
    Command, EncoderInfo, RecordRequest, RecordingUpdate, SourceKind, SourceRow, SourceStatus,
    StudioHost, Target, Update,
};
use rbs_audio::LevelMeter;
use rbs_scene::model::SourceId;
use rbs_ui::meter::{self, Meter, SCALE_MARKS_DB};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::ui::{KindData, MeterRow, ScaleMark, SourceData, StackData, Studio, TargetData};

/// Meter refresh while the Mixer is visible (U16: 30 Hz).
pub const METER_PERIOD: Duration = Duration::from_millis(33);
/// Accessible meter values change at most this often (U16).
const REPORT_EVERY: u32 = 30;

/// Codes of `Studio.rec-state`.
pub mod rec_state {
    pub const OFF: i32 = 0;
    pub const STARTING: i32 = 1;
    pub const RECORDING: i32 = 2;
    pub const STOPPING: i32 = 3;
}

/// Codes of `Studio.preview-state`.
pub mod preview_state {
    pub const SHOWN: i32 = 0;
    pub const STARTING: i32 = 1;
    pub const NO_PLATFORM: i32 = 2;
    pub const NO_SESSION: i32 = 3;
    pub const OTHER_WINDOW: i32 = 4;
    pub const STOPPED: i32 = 5;
}

struct MeterSlot {
    source: SourceId,
    level: Arc<LevelMeter>,
    ballistics: Meter,
}

/// What the window knows of the studio.
pub struct StudioUi {
    pub host: Option<StudioHost>,
    /// 0 starting, 1 running, 2 failed.
    pub state: i32,
    pub failure: String,
    rows: Vec<SourceRow>,
    sources: Rc<VecModel<SourceData>>,
    kind_list: Vec<SourceKind>,
    kinds: Rc<VecModel<KindData>>,
    target_list: Vec<Target>,
    targets: Rc<VecModel<TargetData>>,
    pub targets_kind: i32,
    slots: Vec<MeterSlot>,
    meters: Rc<VecModel<MeterRow>>,
    scale: Rc<VecModel<ScaleMark>>,
    meter_start: Instant,
    meter_ticks: u32,
    /// The preview endpoint arrived; the platform has a preview.
    pub preview_offered: bool,
    /// The preview layer could not be made in this session.
    pub preview_failed: bool,
    /// The layer exists.
    pub preview_ready: bool,
    pub deleted: String,
    pub deleted_serial: u64,
    /// [`rec_state`].
    pub rec_state: i32,
    /// The running recording's file and when it started.
    pub rec_file: Option<(PathBuf, Instant)>,
    /// The last recording's outcome, or why one did not start.
    pub rec_note: String,
    /// The note is a problem (shown as an error).
    pub rec_note_bad: bool,
    /// Why this system cannot record (`None`: it can).
    pub cannot_record: Option<String>,
    encoders: Vec<EncoderInfo>,
    /// The output settings and where they are saved.
    pub profile: RecordProfile,
    config: PathBuf,
    /// Why the settings could not be saved or loaded.
    pub profile_note: String,
    /// The folder field shows this; it changes only on load and on Return (typing is
    /// never overwritten by a sync).
    pub folder_serial: u64,
}

impl StudioUi {
    pub fn new(config: &Path) -> Self {
        let (profile, why) = record_profile::load(config);
        let scale = SCALE_MARKS_DB
            .iter()
            .map(|db| ScaleMark {
                label: format!("{db:.0}").into(),
                at: meter::position(*db),
            })
            .collect::<Vec<_>>();
        Self {
            host: None,
            state: 0,
            failure: String::new(),
            rows: Vec::new(),
            sources: Rc::new(VecModel::default()),
            kind_list: Vec::new(),
            kinds: Rc::new(VecModel::default()),
            target_list: Vec::new(),
            targets: Rc::new(VecModel::default()),
            targets_kind: -1,
            slots: Vec::new(),
            meters: Rc::new(VecModel::default()),
            scale: Rc::new(VecModel::from(scale)),
            meter_start: Instant::now(),
            meter_ticks: 0,
            preview_offered: false,
            preview_failed: false,
            preview_ready: false,
            deleted: String::new(),
            deleted_serial: 0,
            rec_state: rec_state::OFF,
            rec_file: None,
            rec_note: String::new(),
            rec_note_bad: false,
            cannot_record: Some("The studio is starting.".into()),
            encoders: Vec::new(),
            profile,
            config: config.to_owned(),
            profile_note: why
                .map(|w| format!("The saved recording settings were not used: {w}"))
                .unwrap_or_default(),
            folder_serial: 1,
        }
    }

    /// Gives a window's `Studio` global the shared models (once per window).
    pub fn attach_models(&self, g: &Studio<'_>) {
        g.set_sources(ModelRc::from(self.sources.clone()));
        g.set_kinds(ModelRc::from(self.kinds.clone()));
        g.set_targets(ModelRc::from(self.targets.clone()));
        g.set_meters(ModelRc::from(self.meters.clone()));
        g.set_scale(ModelRc::from(self.scale.clone()));
    }

    /// Pushes the scalar state into a window's `Studio` global.
    pub fn push(&self, g: &Studio<'_>, preview_state: i32, picture_panel: &str) {
        g.set_rec_state(self.rec_state);
        g.set_rec_file(
            self.rec_file
                .as_ref()
                .map(|(p, _)| file_name(p))
                .unwrap_or_default()
                .into(),
        );
        g.set_rec_note(self.rec_note.as_str().into());
        g.set_rec_note_bad(self.rec_note_bad);
        g.set_rec_unavailable(self.cannot_record.clone().unwrap_or_default().into());
        let p = &self.profile;
        g.set_out_format(Format::ALL.iter().position(|f| *f == p.format).unwrap_or(0) as i32);
        g.set_out_formats(strings(Format::ALL.iter().map(|f| f.label().to_owned())));
        let mut encoders = vec!["Automatic (best available)".to_owned()];
        encoders.extend(self.encoders.iter().map(|e| e.name.clone()));
        g.set_out_encoders(strings(encoders));
        g.set_out_encoder(self.encoder_index() as i32);
        let video = self.video_options();
        g.set_out_video_rates(strings(video.iter().map(|k| kbps(*k))));
        g.set_out_video_rate(video.iter().position(|k| *k == p.video_kbps).unwrap_or(0) as i32);
        g.set_out_audio_rates(strings(AUDIO_KBPS.iter().map(|k| kbps(*k))));
        g.set_out_audio_rate(
            AUDIO_KBPS
                .iter()
                .position(|k| *k == p.audio_kbps)
                .unwrap_or(0) as i32,
        );
        g.set_out_note(self.profile_note.as_str().into());
        if g.get_out_folder_serial() != self.folder_serial as i32 {
            g.set_out_folder(self.profile.folder.display().to_string().into());
            g.set_out_folder_serial(self.folder_serial as i32);
        }
        g.set_picture_panel(picture_panel.into());
        g.set_state(self.state);
        g.set_failure(self.failure.as_str().into());
        g.set_targets_kind(self.targets_kind);
        g.set_preview_state(preview_state);
        g.set_deleted(self.deleted.as_str().into());
    }

    fn send(&self, c: Command) {
        if let Some(h) = &self.host {
            h.send(c);
        }
    }

    pub fn list_targets(&mut self, code: i32) {
        let Some(kind) = SourceKind::ALL.get(code as usize).copied() else {
            return;
        };
        self.targets_kind = -1;
        self.send(Command::ListTargets(kind));
    }

    pub fn add_source(&mut self, code: i32, index: i32) {
        let kind = SourceKind::ALL.get(code as usize).copied();
        let target = self.target_list.get(index as usize).cloned();
        if let (Some(kind), Some(target)) = (kind, target) {
            self.send(Command::AddSource { kind, target });
        }
    }

    pub fn delete_source(&mut self, id: i32) {
        self.deleted.clear();
        self.send(Command::DeleteSource(SourceId::from_raw(id as u32)));
    }

    pub fn undo_delete(&mut self) {
        self.deleted.clear();
        self.send(Command::UndoDelete);
    }

    /// The picked encoder's place in the list (0: automatic, also when the saved one is
    /// not on this system).
    fn encoder_index(&self) -> usize {
        self.profile
            .encoder
            .as_ref()
            .and_then(|id| self.encoders.iter().position(|e| e.id == *id))
            .map_or(0, |i| i + 1)
    }

    /// The video bitrates offered: the presets, and the saved one if it is not one.
    fn video_options(&self) -> Vec<u32> {
        let mut v = VIDEO_KBPS.to_vec();
        if !v.contains(&self.profile.video_kbps) {
            v.push(self.profile.video_kbps);
            v.sort_unstable();
        }
        v
    }

    /// Starts recording with the output settings.
    pub fn record(&mut self) {
        if self.rec_state != rec_state::OFF {
            return;
        }
        if let Some(why) = &self.cannot_record {
            self.set_note(why.clone(), true);
            return;
        }
        let p = &self.profile;
        if let Some(why) = p.problem() {
            self.set_note(why, true);
            return;
        }
        let encoder = match self.encoder_index() {
            0 => None,
            i => self.encoders.get(i - 1).map(|e| e.id.clone()),
        };
        let request = RecordRequest {
            path: p.file_for(chrono::Local::now().naive_local()),
            container: p.format.container(),
            encoder,
            video_bitrate_bps: u64::from(p.video_kbps) * 1000,
            audio_bitrate_bps: u64::from(p.audio_kbps) * 1000,
        };
        self.rec_state = rec_state::STARTING;
        self.set_note(String::new(), false);
        self.send(Command::StartRecording(request));
    }

    pub fn stop_recording(&mut self) {
        if self.rec_state == rec_state::RECORDING {
            self.rec_state = rec_state::STOPPING;
            self.send(Command::StopRecording);
        }
    }

    fn set_note(&mut self, note: String, bad: bool) {
        self.rec_note = note;
        self.rec_note_bad = bad;
    }

    /// The folder field was confirmed.
    pub fn set_folder(&mut self, text: &str) {
        let folder = PathBuf::from(text.trim());
        if folder.is_absolute() {
            self.profile.folder = folder;
            self.save_profile();
        } else {
            self.profile_note = "The folder must be a full path, for example C:\\Videos.".into();
        }
        // The field shows the folder in use (also after a refused entry).
        self.folder_serial += 1;
    }

    pub fn set_format(&mut self, i: i32) {
        if let Some(f) = Format::ALL.get(i as usize) {
            self.profile.format = *f;
            self.save_profile();
        }
    }

    pub fn set_encoder(&mut self, i: i32) {
        self.profile.encoder = match i {
            0 => None,
            i => match self.encoders.get(i as usize - 1) {
                Some(e) => Some(e.id.clone()),
                None => return,
            },
        };
        self.save_profile();
    }

    pub fn set_video_rate(&mut self, i: i32) {
        if let Some(k) = self.video_options().get(i as usize) {
            self.profile.video_kbps = *k;
            self.save_profile();
        }
    }

    pub fn set_audio_rate(&mut self, i: i32) {
        if let Some(k) = AUDIO_KBPS.get(i as usize) {
            self.profile.audio_kbps = *k;
            self.save_profile();
        }
    }

    fn save_profile(&mut self) {
        self.profile_note = match record_profile::save(&self.config, &self.profile) {
            Ok(()) => String::new(),
            Err(e) => format!("The recording settings could not be saved: {e}"),
        };
    }

    /// The recording's duration so far, while one runs.
    pub fn recording_for(&self) -> Option<(Duration, String)> {
        self.rec_file
            .as_ref()
            .map(|(p, since)| (since.elapsed(), file_name(p)))
    }

    pub fn reset_clip(&mut self, i: i32) {
        if let Some(s) = self.slots.get_mut(i as usize) {
            s.ballistics.reset_clip();
        }
    }

    /// Takes an update from the control thread. Returns an error text for the error bar,
    /// and the preview endpoint when it arrives.
    pub fn apply(&mut self, u: Update) -> Applied {
        let mut out = Applied::default();
        match u {
            Update::Started {
                preview,
                kinds,
                notes,
                encoders,
                cannot_record,
            } => {
                self.encoders = encoders;
                self.cannot_record = cannot_record;
                for n in notes {
                    log(&n);
                }
                self.state = 1;
                self.kind_list = kinds;
                let rows: Vec<KindData> = self
                    .kind_list
                    .iter()
                    .map(|k| KindData {
                        code: SourceKind::ALL.iter().position(|a| a == k).unwrap_or(0) as i32,
                        label: k.label().into(),
                    })
                    .collect();
                self.kinds.set_vec(rows);
                self.preview_offered = preview.is_some();
                out.preview = preview;
            }
            Update::Failed(why) => {
                self.state = 2;
                self.cannot_record = Some(format!("The studio stopped: {why}"));
                if self.rec_state != rec_state::OFF {
                    // The studio's shutdown closed the file.
                    self.rec_state = rec_state::OFF;
                    if let Some((p, _)) = self.rec_file.take() {
                        self.set_note(
                            format!(
                                "The recording stopped: {why}. What was recorded is in {}.",
                                p.display()
                            ),
                            true,
                        );
                    }
                }
                self.failure = why.clone();
                out.error = Some(format!("The studio stopped: {why}"));
            }
            Update::Sources(rows) => self.set_rows(rows),
            Update::Targets(kind, list) => {
                self.targets_kind = SourceKind::ALL
                    .iter()
                    .position(|a| *a == kind)
                    .map_or(-1, |i| i as i32);
                self.targets.set_vec(
                    list.iter()
                        .map(|t| TargetData {
                            name: t.name.as_str().into(),
                            is_default: t.is_default,
                        })
                        .collect::<Vec<_>>(),
                );
                self.target_list = list;
            }
            Update::Deleted(name) => {
                self.deleted = name;
                self.deleted_serial += 1;
            }
            Update::Error(e) => out.error = Some(e),
            Update::Recording(r) => self.recording(r, &mut out),
        }
        out
    }

    fn recording(&mut self, r: RecordingUpdate, out: &mut Applied) {
        match r {
            RecordingUpdate::Started {
                path,
                video_encoder,
                audio_encoder,
            } => {
                log(&format!(
                    "recording to {} (video: {video_encoder}, audio: {})",
                    path.display(),
                    audio_encoder.as_deref().unwrap_or("none")
                ));
                self.rec_state = rec_state::RECORDING;
                self.rec_file = Some((path, Instant::now()));
                self.set_note(format!("Video encoder: {video_encoder}"), false);
            }
            RecordingUpdate::NotStarted(why) => {
                self.rec_state = rec_state::OFF;
                self.rec_file = None;
                self.set_note(format!("The recording did not start: {why}"), true);
            }
            RecordingUpdate::Stopped {
                path,
                failure,
                frames,
                dropped,
            } => {
                self.rec_state = rec_state::OFF;
                self.rec_file = None;
                log(&format!(
                    "recording stopped: {frames} frames, {dropped} dropped, failure: {failure:?}"
                ));
                match failure {
                    None => {
                        let mut note = format!("Saved {}", path.display());
                        if dropped > 0 {
                            note.push_str(&format!(
                                " ({dropped} of {} frames dropped)",
                                frames + dropped
                            ));
                        }
                        self.set_note(note, false);
                    }
                    Some(why) => {
                        let text = format!(
                            "The recording stopped: {why}. What was recorded is in {}.",
                            path.display()
                        );
                        out.error = Some(text.clone());
                        self.set_note(text, true);
                    }
                }
            }
        }
    }

    fn set_rows(&mut self, rows: Vec<SourceRow>) {
        let data: Vec<SourceData> = rows
            .iter()
            .map(|r| {
                let (status, ok) = match &r.status {
                    SourceStatus::Running => ("Running".to_owned(), true),
                    SourceStatus::Idle => ("Not shown, closed".to_owned(), true),
                    SourceStatus::Failed(why) => (format!("Not running: {why}"), false),
                };
                SourceData {
                    id: r.id.get() as i32,
                    name: r.name.as_str().into(),
                    kind: r.kind.map_or("Unknown type", SourceKind::label).into(),
                    status: status.into(),
                    ok,
                }
            })
            .collect();
        set_rows(&self.sources, data);
        // Meters: one per playing audio source, in list order; ballistics survive for
        // sources that stay.
        let mut old: Vec<MeterSlot> = std::mem::take(&mut self.slots);
        for r in &rows {
            let Some(level) = &r.meter else { continue };
            let slot = match old.iter().position(|s| s.source == r.id) {
                Some(i) if Arc::ptr_eq(&old[i].level, level) => old.swap_remove(i),
                _ => MeterSlot {
                    source: r.id,
                    level: Arc::clone(level),
                    ballistics: Meter::new(),
                },
            };
            self.slots.push(slot);
        }
        let names: Vec<SharedString> = rows
            .iter()
            .filter(|r| r.meter.is_some())
            .map(|r| r.name.as_str().into())
            .collect();
        let meters: Vec<MeterRow> = names
            .into_iter()
            .map(|name| MeterRow {
                name,
                value: "silent".into(),
                ..MeterRow::default()
            })
            .collect();
        set_rows(&self.meters, meters);
        self.rows = rows;
    }

    /// True while meters need refreshing.
    pub fn has_meters(&self) -> bool {
        !self.slots.is_empty()
    }

    /// One meter refresh: takes each source's peaks since the last call.
    pub fn tick_meters(&mut self) {
        let now = self.meter_start.elapsed();
        self.meter_ticks = self.meter_ticks.wrapping_add(1);
        let report = self.meter_ticks.is_multiple_of(REPORT_EVERY);
        for (i, s) in self.slots.iter_mut().enumerate() {
            let levels = s.level.take();
            let db = (0..levels.channels())
                .map(|ch| levels.peak_db(ch))
                .fold(f32::NEG_INFINITY, f32::max);
            s.ballistics.update(db, now);
            let view = s.ballistics.view(now);
            let Some(mut row) = self.meters.row_data(i) else {
                continue;
            };
            let [lo, mi, hi] = view.zones();
            row.lo_from = lo.0;
            row.lo_to = lo.1;
            row.mi_from = mi.0;
            row.mi_to = mi.1;
            row.hi_from = hi.0;
            row.hi_to = hi.1;
            row.peak = view.peak;
            row.peak_zone = if view.peak >= meter::position(meter::HIGH_FROM_DB) {
                2
            } else if view.peak >= meter::position(meter::MID_FROM_DB) {
                1
            } else {
                0
            };
            row.clipped = view.clipped;
            if report {
                row.value = s.ballistics.take_report().english().into();
            }
            self.meters.set_row_data(i, row);
        }
    }
}

/// What [`StudioUi::apply`] asks of the shell.
#[derive(Default)]
pub struct Applied {
    pub error: Option<String>,
    pub preview: Option<rbs_gpu::share::PreviewEndpoint>,
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn kbps(k: u32) -> String {
    format!("{k} kbit/s")
}

fn strings(items: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        items
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )))
}

/// Rows are replaced only where they changed, so unchanged rows keep their views.
fn set_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (i, r) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&r) {
            model.set_row_data(i, r);
        }
    }
}

/// The rectangle (dock-area coordinates, logical pixels) of the content of the stack
/// showing panel `id` as its active tab, if one does.
pub fn active_content(
    stacks: &VecModel<StackData>,
    id: &str,
    bar: f32,
    border: f32,
) -> Option<[f32; 4]> {
    stacks.iter().find_map(|s| {
        let tab = s.tabs.row_data(s.active as usize)?;
        (tab.id == id).then(|| {
            [
                s.x + border,
                s.y + bar + border,
                (s.w - 2.0 * border).max(0.0),
                (s.h - bar - 2.0 * border).max(0.0),
            ]
        })
    })
}

/// A line for the developer log (stderr; nothing is shown to the user).
pub fn log(line: &str) {
    #[allow(clippy::print_stderr)]
    {
        eprintln!("rbs: {line}");
    }
}
