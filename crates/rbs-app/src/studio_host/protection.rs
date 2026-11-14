//! Recording protection on the control thread (ADR-0090 phase one, spec
//! `docs/specs/recording-protection.md`).
//!
//! While a recording or stream runs, the host keeps an `rbs_record::protection::Guard`
//! and one probe worker. Once a second it hands the guard the outputs' counters and
//! whatever the probe answered, ends the recordings the guard asks to stop through the
//! normal tail (`end_recording`, ADR-0045), tells the user about new warnings with the
//! existing error notice, and publishes `protection.changed`. With no output running
//! there is no guard, no worker and no wake-up (P13).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rbs_api::event::{EventPayload, StateKey, Topic};
use rbs_api::types as api;
use rbs_core::time::Timestamp;
use rbs_record::OutputId;
use rbs_record::protection::{
    AlertKind, Guard, GuardStatus, Level, OutputSample, ProbeOutcome, ProbeStep, ProbeWorker,
    Probed, ProtectionSettings, Sample, SystemProbe, VolumeKey, VolumeSpace,
};

use super::{Host, OutputCanvas, Update, slot};

/// How often the guard steps while an output runs.
const STEP_INTERVAL: Duration = Duration::from_secs(1);

/// A line for the developer log (stderr), as the app's other logs.
fn log(line: &str) {
    #[allow(clippy::print_stderr)]
    {
        eprintln!("rbs: {line}");
    }
}

/// Makes the probe for a new guard: the OS probe in the app, a fake in tests.
#[derive(Clone)]
pub struct SystemProbeFactory(Arc<dyn Fn() -> Box<dyn SystemProbe> + Send + Sync>);

impl SystemProbeFactory {
    /// A factory calling `make` for each guard.
    pub fn new(make: impl Fn() -> Box<dyn SystemProbe> + Send + Sync + 'static) -> Self {
        Self(Arc::new(make))
    }

    /// The operating system's probe (`rbs-system-probe`).
    pub fn os() -> Self {
        Self::new(|| Box::new(OsProbe))
    }
}

impl fmt::Debug for SystemProbeFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SystemProbeFactory")
    }
}

/// `rbs-system-probe` behind the port (ADR-0090 decision 11).
struct OsProbe;

impl SystemProbe for OsProbe {
    fn volume_space(&mut self, folder: &Path) -> Probed<VolumeSpace> {
        match rbs_system_probe::volume_space(folder) {
            Ok(s) => Probed::Value(VolumeSpace {
                volume: VolumeKey(s.volume),
                free_bytes: s.free_bytes,
            }),
            Err(rbs_system_probe::ProbeError::Unsupported) => Probed::Unsupported,
            Err(rbs_system_probe::ProbeError::Failed(_)) => Probed::Unknown,
        }
    }

    fn available_memory(&mut self) -> Probed<u64> {
        match rbs_system_probe::available_memory() {
            Ok(b) => Probed::Value(b),
            Err(rbs_system_probe::ProbeError::Unsupported) => Probed::Unsupported,
            Err(rbs_system_probe::ProbeError::Failed(_)) => Probed::Unknown,
        }
    }
}

/// The guard of the running take.
struct Running {
    guard: Guard,
    /// `None` without a probe factory or when the worker could not start: disk and
    /// memory stay unavailable, lag is still watched.
    probe: Option<ProbeWorker>,
    /// Each recording folder's volume, from the probe's last answer.
    volumes: HashMap<PathBuf, VolumeKey>,
    /// Alerts at `warning` or above already told to the user.
    told: BTreeSet<(AlertKind, Option<OutputId>, Level)>,
}

/// The host's protection state.
#[derive(Default)]
pub(super) struct ProtectionHost {
    factory: Option<SystemProbeFactory>,
    running: Option<Running>,
    due: Option<Timestamp>,
    /// `protection.changed` as last published.
    published: Option<api::ProtectionStatus>,
}

impl fmt::Debug for ProtectionHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtectionHost")
            .field("guarded", &self.running.is_some())
            .finish()
    }
}

impl ProtectionHost {
    pub(super) fn new(factory: Option<SystemProbeFactory>) -> Self {
        Self {
            factory,
            ..Self::default()
        }
    }
}

/// One running output as the host names it.
struct Watched {
    canvas: OutputCanvas,
    stream: bool,
    id: OutputId,
    folder: Option<PathBuf>,
}

fn level(l: Level) -> api::ProtectionLevel {
    match l {
        Level::Unavailable => api::ProtectionLevel::Unavailable,
        Level::Ok => api::ProtectionLevel::Ok,
        Level::Notice => api::ProtectionLevel::Notice,
        Level::Warning => api::ProtectionLevel::Warning,
        Level::Critical => api::ProtectionLevel::Critical,
    }
}

fn kind(k: AlertKind) -> api::ProtectionKind {
    match k {
        AlertKind::Disk => api::ProtectionKind::Disk,
        AlertKind::Lag => api::ProtectionKind::Lag,
        AlertKind::Render => api::ProtectionKind::Render,
        AlertKind::Audio => api::ProtectionKind::Audio,
        AlertKind::Memory => api::ProtectionKind::Memory,
    }
}

fn output_kind(stream: bool) -> api::OutputKind {
    if stream {
        api::OutputKind::Stream
    } else {
        api::OutputKind::Record
    }
}

/// Rounds seconds to whole minutes, so the published state changes once a minute.
fn to_minute(secs: u64) -> u64 {
    (secs + 30) / 60 * 60
}

/// Rounds bytes to a tenth of a GiB, for the same reason.
fn to_tenth_gib(bytes: u64) -> u64 {
    let step = (1u64 << 30) / 10;
    bytes / step * step
}

/// The API's view of `st`.
fn api_status(
    st: &GuardStatus,
    watched: &[Watched],
    volumes: &HashMap<PathBuf, VolumeKey>,
) -> api::ProtectionStatus {
    let of = |id: OutputId| watched.iter().find(|w| w.id == id);
    let disks = st
        .disks
        .iter()
        .map(|d| api::ProtectionDisk {
            level: level(d.level),
            free_bytes: d.free_bytes.map(to_tenth_gib),
            seconds_left: d.seconds_left.map(to_minute),
            recordings: watched
                .iter()
                .filter(|w| {
                    !w.stream
                        && w.folder
                            .as_ref()
                            .and_then(|f| volumes.get(f))
                            .is_some_and(|v| *v == d.volume)
                })
                .map(|w| w.canvas)
                .collect(),
        })
        .collect();
    let outputs = st
        .outputs
        .iter()
        .filter_map(|o| {
            of(o.output).map(|w| api::ProtectionOutput {
                canvas: w.canvas,
                output: output_kind(w.stream),
                level: level(o.level),
            })
        })
        .collect();
    let alerts = st
        .alerts
        .iter()
        .map(|a| {
            let w = a.output.and_then(of);
            api::ProtectionAlert {
                kind: kind(a.kind),
                level: level(a.level),
                text: a.text.clone(),
                canvas: w.map(|w| w.canvas),
                output: w.map(|w| output_kind(w.stream)),
            }
        })
        .collect();
    api::ProtectionStatus {
        guarded: true,
        level: level(st.level()),
        disks,
        outputs,
        render: level(st.render.unwrap_or(Level::Ok)),
        audio: level(st.audio.unwrap_or(Level::Ok)),
        memory: api::ProtectionMemory {
            level: level(st.memory.unwrap_or(Level::Unavailable)),
            available_bytes: st.available_memory.map(to_tenth_gib),
        },
        alerts,
    }
}

impl Host {
    /// The running outputs the guard watches, main canvas first.
    fn watched(&self) -> Vec<Watched> {
        let mut out = Vec::new();
        for c in OutputCanvas::ALL {
            let o = &self.outs[slot(c)];
            if let Some((path, id)) = &o.recording {
                out.push(Watched {
                    canvas: c,
                    stream: false,
                    id: *id,
                    folder: path.parent().map(Path::to_path_buf),
                });
            }
            if let Some((_, _, id)) = &o.streaming {
                out.push(Watched {
                    canvas: c,
                    stream: true,
                    id: *id,
                    folder: None,
                });
            }
        }
        out
    }

    /// One pass of the guard when it is due (ADR-0090 decision 1). Returns how long the
    /// loop may wait; `None` while no output runs, so nothing is armed.
    pub(super) fn drive_protection(
        &mut self,
        now: Timestamp,
        notify: &dyn Fn(Update),
    ) -> Option<Duration> {
        let watched = self.watched();
        if watched.is_empty() {
            if self.protection.running.take().is_some() {
                self.protection.due = None;
            }
            self.publish_protection(api::ProtectionStatus::idle());
            return None;
        }
        if self.protection.running.is_none() {
            let probe = self.protection.factory.as_ref().and_then(|f| {
                ProbeWorker::spawn((f.0)(), now)
                    .map_err(|e| {
                        log(&format!("protection probe did not start: {e}"));
                    })
                    .ok()
            });
            self.protection.running = Some(Running {
                guard: Guard::new(ProtectionSettings::default()),
                probe,
                volumes: HashMap::new(),
                told: BTreeSet::new(),
            });
        }
        let due = *self.protection.due.get_or_insert(now);
        if now < due {
            return Some(due.saturating_since(now));
        }
        self.protection.due = Some(now + STEP_INTERVAL);
        let folders: Vec<PathBuf> = {
            let mut f: Vec<PathBuf> = watched.iter().filter_map(|w| w.folder.clone()).collect();
            f.sort();
            f.dedup();
            f
        };
        let mut outputs = Vec::with_capacity(watched.len());
        let mut late = (0, 0);
        for w in &watched {
            if let Some(p) = self.studio.output_counters(w.id) {
                late = (p.late_render_ticks, p.late_audio_ticks);
                outputs.push((w, p));
            }
        }
        let Some(run) = self.protection.running.as_mut() else {
            return Some(STEP_INTERVAL);
        };
        let probe = match run.probe.as_mut().and_then(|p| p.poll(now, &folders)) {
            Some(ProbeOutcome::Reading(r)) => {
                for (folder, space) in &r.folders {
                    match space {
                        Probed::Value(s) => {
                            run.volumes.insert(folder.clone(), s.volume);
                        }
                        Probed::Unknown | Probed::Unsupported => {
                            run.volumes.remove(folder);
                        }
                    }
                }
                Some(ProbeStep::of(ProbeOutcome::Reading(r)))
            }
            Some(ProbeOutcome::TimedOut) => Some(ProbeStep::TimedOut),
            None => None,
        };
        let sample = Sample {
            outputs: outputs
                .into_iter()
                .map(|(w, counters)| OutputSample {
                    output: w.id,
                    stream: w.stream,
                    volume: w.folder.as_ref().and_then(|f| run.volumes.get(f).copied()),
                    counters,
                })
                .collect(),
            late_render_ticks: late.0,
            late_audio_ticks: late.1,
            probe,
        };
        let stops = run.guard.step(now, &sample);
        let status = run.guard.status().clone();
        // Tell the user once about each new warning or worse (decision 7). A stop says
        // why itself, below.
        let mut tell = Vec::new();
        let now_told: BTreeSet<_> = status
            .alerts
            .iter()
            .filter(|a| a.level >= Level::Warning)
            .map(|a| (a.kind, a.output, a.level))
            .collect();
        for a in &status.alerts {
            let key = (a.kind, a.output, a.level);
            if a.level >= Level::Warning && !run.told.contains(&key) {
                log(&format!("protection: {}", a.text));
                let stopping =
                    a.kind == AlertKind::Disk && a.level == Level::Critical && !stops.is_empty();
                if !stopping {
                    tell.push(a.text.clone());
                }
            }
        }
        run.told = now_told;
        let api = api_status(&status, &watched, &run.volumes);
        for t in tell {
            notify(Update::Error(t));
        }
        for stop in stops {
            let Some(w) = watched.iter().find(|w| w.id == stop.output && !w.stream) else {
                continue;
            };
            log(&format!("protection: {}", stop.reason));
            self.outs[slot(w.canvas)].protection_stop = Some(stop.reason.clone());
            notify(Update::Error(stop.reason));
            self.end_recording(w.canvas, notify);
        }
        self.publish_protection(api);
        // The probe is polled on these steps too: its five-second requests and its
        // timeout are seen within a second.
        Some(STEP_INTERVAL)
    }

    /// Publishes `protection.changed` when the state differs from the last one.
    fn publish_protection(&mut self, status: api::ProtectionStatus) {
        if self.protection.published.as_ref() == Some(&status) {
            return;
        }
        let payload = status.clone();
        self.events
            .changed(StateKey::topic(Topic::ProtectionChanged), || {
                EventPayload::ProtectionChanged(payload)
            });
        self.protection.published = Some(status);
    }

    /// `protection.status`: the state as last published (the idle state before any).
    pub(super) fn api_protection(&self) -> api::ProtectionStatus {
        self.protection
            .published
            .clone()
            .unwrap_or_else(api::ProtectionStatus::idle)
    }
}
