//! The window's side of the studio (ADR-0041 decision 7): state shown in the `Studio`
//! global, commands to the control thread, the meters' 30 Hz refresh while the Mixer is
//! visible, and the Program panel's preview layer.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rbs_app::studio_host::{
    Command, SourceKind, SourceRow, SourceStatus, StudioHost, Target, Update,
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
}

impl StudioUi {
    pub fn new() -> Self {
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
            } => {
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
        }
        out
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
