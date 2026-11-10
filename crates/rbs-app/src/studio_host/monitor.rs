//! The monitor output in the host (ADR-0097 F1; control API 1.30): which source is
//! heard on it, its device and volume, solo, and when the device opens.
//!
//! The device is opened on this (the control) thread only while the studio wants it (a
//! monitored or soloed source of the program scene) and closed when it does not; a
//! failed device is closed and opened again with back-off ([`MonitorControl`]). The mix
//! and the outputs never wait for it. The device and the volume are this computer's
//! settings (`monitor.toml`); solo is session state and never saved.

use std::collections::BTreeSet;
use std::time::Duration;

use rbs_api::types as api;
use rbs_core::time::Timestamp;
use rbs_scene::model::{SourceId, SourceMonitor};

use super::{Host, Refusal};
use crate::monitor_settings::{self, MonitorControl, MonitorSettings, MonitorStep};

/// What the host keeps of the monitor output.
pub(super) struct MonitorHost {
    settings: MonitorSettings,
    control: MonitorControl,
    /// Why the device last failed to open or play, until it plays again.
    problem: Option<String>,
    /// The status `monitor.changed` carried last.
    api_published: Option<api::MonitorStatus>,
}

impl MonitorHost {
    pub(super) fn new(settings: MonitorSettings) -> Self {
        Self {
            settings,
            control: MonitorControl::default(),
            problem: None,
            api_published: None,
        }
    }
}

/// The API's mode for a source's saved one.
pub(super) fn api_mode(m: SourceMonitor) -> api::MonitorMode {
    match m {
        SourceMonitor::Off => api::MonitorMode::Off,
        SourceMonitor::Only => api::MonitorMode::Only,
        SourceMonitor::Also => api::MonitorMode::Also,
    }
}

/// The saved mode for the API's.
pub(super) fn scene_mode(m: api::MonitorMode) -> SourceMonitor {
    match m {
        api::MonitorMode::Off => SourceMonitor::Off,
        api::MonitorMode::Only => SourceMonitor::Only,
        api::MonitorMode::Also => SourceMonitor::Also,
    }
}

impl Host {
    /// Opens, closes and retries the monitor device as the studio needs it (ADR-0097
    /// decisions 4 and 7), then publishes `monitor.changed` if the API's view changed.
    /// Returns the time until the next attempt, if one waits.
    pub(super) fn drive_monitor(&mut self, now: Timestamp) -> Option<Duration> {
        let wanted = self.studio.monitor_wanted();
        let state = self.studio.monitor();
        let failure = state.as_ref().and_then(|m| m.failure.clone());
        let (step, wait) =
            self.monitor
                .control
                .step(wanted, state.is_some(), failure.is_some(), now);
        let wait = match step {
            MonitorStep::Keep => {
                if state.is_some() && failure.is_none() {
                    self.monitor.problem = None;
                }
                wait
            }
            MonitorStep::Close => {
                if failure.is_some() {
                    self.monitor.problem = failure;
                }
                self.studio.stop_monitor();
                wait
            }
            MonitorStep::Open => {
                let opened = self
                    .adapters
                    .sources
                    .open_monitor(self.monitor.settings.device.as_deref())
                    .map_err(|e| e.to_string())
                    .and_then(|(output, _, _)| {
                        self.studio
                            .start_monitor(output, self.monitor.settings.volume)
                            .map_err(|e| e.to_string())
                    });
                match opened {
                    Ok(()) => None,
                    Err(why) => {
                        self.monitor.problem = Some(why);
                        Some(self.monitor.control.failed(now))
                    }
                }
            }
        };
        self.monitor_api_changed();
        wait
    }

    /// Sets whether a source is heard on the monitor (ADR-0097 decision 5): a live
    /// control, not on the undo stack, sent to the mixer without rebinding; the device
    /// opens or closes on the host's next round. `Ok(false)` when nothing changed.
    pub(super) fn set_source_monitor(
        &mut self,
        id: SourceId,
        monitor: SourceMonitor,
    ) -> Result<bool, Refusal> {
        let now = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .audio;
        if now.monitor == monitor {
            return Ok(false);
        }
        self.collection
            .set_source_monitor(id, monitor)
            .map_err(|e| Refusal::of(&e))?;
        self.studio.set_mix(&self.collection);
        Ok(true)
    }

    /// Chooses the monitor device (`None`: System default) and saves it. A monitor that
    /// plays is closed and opens on the new device on the host's next round.
    pub(super) fn set_monitor_device(&mut self, device: Option<String>) -> Result<(), Refusal> {
        if let Some(d) = &device
            && !self
                .adapters
                .sources
                .monitor_devices()
                .iter()
                .any(|m| m.target == *d)
        {
            return Err(Refusal::NotFound(format!(
                "There is no playback device {d}."
            )));
        }
        if self.monitor.settings.device == device {
            return Ok(());
        }
        self.monitor.settings.device = device;
        self.save_monitor()?;
        // A new device is a fresh start: no back-off from the old one.
        self.monitor.control = MonitorControl::default();
        self.monitor.problem = None;
        self.studio.stop_monitor();
        self.monitor_api_changed();
        Ok(())
    }

    /// Sets the monitor volume (linear, 0 to 1) and saves it; a playing monitor ramps.
    pub(super) fn set_monitor_volume(&mut self, volume: f32) -> Result<(), Refusal> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(Refusal::Invalid(
                "The monitor volume must be between 0 and 1.".to_owned(),
            ));
        }
        if self.monitor.settings.volume == volume {
            return Ok(());
        }
        self.monitor.settings.volume = volume;
        self.save_monitor()?;
        self.studio.set_monitor_volume(volume);
        self.monitor_api_changed();
        Ok(())
    }

    /// Solos sources on the monitor (session state, never saved): while any is soloed,
    /// the monitor plays only them; the mix is unchanged. Empty ends solo.
    pub(super) fn set_monitor_solo(&mut self, sources: &[u32]) -> Result<(), Refusal> {
        let mut solo = BTreeSet::new();
        for raw in sources {
            let id = SourceId::from_raw(*raw);
            if self.collection.source(id).is_none() {
                return Err(Refusal::NotFound(format!("There is no source {raw}.")));
            }
            solo.insert(id);
        }
        self.studio.set_solo(solo);
        self.monitor_api_changed();
        Ok(())
    }

    fn save_monitor(&self) -> Result<(), Refusal> {
        let Some(dir) = &self.recent_dir else {
            return Ok(());
        };
        monitor_settings::save(dir, &self.monitor.settings)
            .map_err(|e| Refusal::Failed(format!("The monitor settings were not saved: {e}")))
    }

    /// The monitor output as `monitor.status` and `monitor.changed` show it.
    pub(super) fn api_monitor(&self) -> api::MonitorStatus {
        let state = self.studio.monitor();
        let failure = state.as_ref().and_then(|m| m.failure.clone());
        api::MonitorStatus {
            available: self.adapters.sources.can_monitor(),
            device: self.monitor.settings.device.clone(),
            volume: self.monitor.settings.volume,
            wanted: self.studio.monitor_wanted(),
            playing: state.is_some() && failure.is_none(),
            latency_ms: state
                .as_ref()
                .filter(|_| failure.is_none())
                .map(|m| u32::try_from(m.latency.as_millis()).unwrap_or(u32::MAX)),
            solo: self.studio.solo().iter().map(|s| s.get()).collect(),
            error: failure.or_else(|| self.monitor.problem.clone()),
        }
    }

    /// The playback devices for `monitor.devices` (enumerates: only on request).
    pub(super) fn api_monitor_devices(&self) -> api::MonitorDeviceList {
        api::MonitorDeviceList {
            devices: self
                .adapters
                .sources
                .monitor_devices()
                .into_iter()
                .map(|d| api::MonitorDevice {
                    device: d.target,
                    name: d.name,
                    is_default: d.is_default,
                })
                .collect(),
        }
    }

    /// Publishes `monitor.changed` when the API's view of the monitor differs.
    pub(super) fn monitor_api_changed(&mut self) {
        let now = self.api_monitor();
        if self.monitor.api_published.as_ref() == Some(&now) {
            return;
        }
        self.monitor.api_published = Some(now.clone());
        self.events
            .changed(super::StateKey::topic(super::Topic::MonitorChanged), || {
                super::EventPayload::MonitorChanged(now)
            });
    }
}
