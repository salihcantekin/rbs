//! The monitor output in the UI (ADR-0097 F1): the headphones button of each Mixer
//! strip, the Mixer's monitor row and Settings › Audio › Monitor. All three read one
//! [`MonitorUi`], filled from the host's [`Update::Monitor`] and
//! [`Update::MonitorDevices`], and send the host's monitor commands.
//!
//! The device list is asked for only while something shows it: the Mixer's row (shown
//! only while the monitor is wanted) or the Audio settings page (decision 7).

use rbs_api::types as api;
use rbs_app::studio_host::{Command, Update};
use rbs_scene::model::{SourceId, SourceMonitor};
use rbs_ui::fader;
use slint::Model;

use super::{StudioUi, strings};
use crate::ui::Studio;

/// The Mixer button's code for a mode: 0 off, 1 monitor and output, 2 monitor only.
pub fn mode_code(m: SourceMonitor) -> i32 {
    match m {
        SourceMonitor::Off => 0,
        SourceMonitor::Also => 1,
        SourceMonitor::Only => 2,
    }
}

fn mode_of(code: i32) -> Option<SourceMonitor> {
    match code {
        0 => Some(SourceMonitor::Off),
        1 => Some(SourceMonitor::Also),
        2 => Some(SourceMonitor::Only),
        _ => None,
    }
}

/// What the UI knows of the monitor output.
#[derive(Debug, Default)]
pub struct MonitorUi {
    /// The host's last status; `None` before the studio runs.
    status: Option<api::MonitorStatus>,
    /// The playback devices, once listed.
    devices: Vec<api::MonitorDevice>,
    /// A list was asked for and has not come back.
    listing: bool,
    /// The Audio settings page is shown.
    page_shown: bool,
    /// The level in words while a fader drag has not yet been confirmed by the host.
    drag_text: Option<String>,
}

/// The monitor as the Mixer row and the Settings card show it.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorView {
    pub available: bool,
    pub wanted: bool,
    /// 0 neutral, 1 playing, 2 a problem.
    pub tone: i32,
    pub status: String,
    pub latency: String,
    pub volume: f32,
    pub volume_text: String,
    /// "System default (…)" first, then each device; a saved device that is not
    /// listed last.
    pub devices: Vec<String>,
    pub device: i32,
    pub device_name: String,
    /// The soloed sources' names, joined; empty while solo is off.
    pub solo: String,
}

impl MonitorUi {
    /// The sources soloed on the monitor.
    pub fn solo(&self) -> &[u32] {
        self.status.as_ref().map_or(&[], |s| s.solo.as_slice())
    }

    /// The view, with `name` naming a source by id.
    pub fn view(&self, name: impl Fn(u32) -> Option<String>) -> MonitorView {
        let Some(s) = &self.status else {
            return MonitorView {
                available: false,
                wanted: false,
                tone: 0,
                status: String::new(),
                latency: String::new(),
                volume: fader::position_of(1.0),
                volume_text: fader::volume_text(1.0),
                devices: vec!["System default".to_owned()],
                device: 0,
                device_name: "System default".to_owned(),
                solo: String::new(),
            };
        };
        let default_name = self
            .devices
            .iter()
            .find(|d| d.is_default)
            .map(|d| format!("System default ({})", d.name))
            .unwrap_or_else(|| "System default".to_owned());
        let mut devices = vec![default_name.clone()];
        devices.extend(self.devices.iter().map(|d| d.name.clone()));
        let (device, device_name) = match &s.device {
            None => (0, default_name),
            Some(id) => match self.devices.iter().position(|d| d.device == *id) {
                Some(i) => (i as i32 + 1, self.devices[i].name.clone()),
                // Not listed (yet, or unplugged): it stays chosen.
                None => {
                    let label = if self.devices.is_empty() {
                        "Saved device".to_owned()
                    } else {
                        "Saved device (not connected)".to_owned()
                    };
                    devices.push(label.clone());
                    (devices.len() as i32 - 1, label)
                }
            },
        };
        let latency = s
            .latency_ms
            .map(|ms| format!("{ms} ms"))
            .unwrap_or_default();
        let (tone, status) = if !s.available {
            (
                2,
                "No playback device can be used on this computer.".to_owned(),
            )
        } else if let Some(e) = &s.error {
            (2, format!("{e} Trying again."))
        } else if s.playing {
            (1, format!("Playing · {latency}"))
        } else if s.wanted {
            (0, "Opening the device…".to_owned())
        } else {
            (
                0,
                "Not in use. Turn on the monitor of a source in the Mixer to hear it here."
                    .to_owned(),
            )
        };
        let solo = s
            .solo
            .iter()
            .filter_map(|id| name(*id))
            .collect::<Vec<_>>()
            .join(", ");
        MonitorView {
            available: s.available,
            wanted: s.wanted,
            tone,
            status,
            latency,
            volume: fader::position_of(s.volume),
            volume_text: fader::volume_text(s.volume),
            devices,
            device,
            device_name,
            solo,
        }
    }
}

impl StudioUi {
    /// Takes the host's monitor updates. Returns true when they were monitor updates.
    pub(super) fn monitor_update(&mut self, u: &Update) -> bool {
        match u {
            Update::Monitor(s) => {
                let became_wanted =
                    s.wanted && !self.monitor.status.as_ref().is_some_and(|o| o.wanted);
                let solo_changed = self.monitor.status.as_ref().map(|o| &o.solo) != Some(&s.solo);
                let host_text = fader::volume_text(s.volume);
                if self.monitor.drag_text.as_deref() == Some(host_text.as_str()) {
                    self.monitor.drag_text = None;
                }
                self.monitor.status = Some(s.clone());
                if solo_changed {
                    self.mark_solo();
                }
                // The Mixer's row shows the device's name: list once when it appears.
                if became_wanted && self.monitor.devices.is_empty() {
                    self.list_monitor_devices();
                }
                true
            }
            Update::MonitorDevices(d) => {
                self.monitor.devices = d.clone();
                self.monitor.listing = false;
                true
            }
            _ => false,
        }
    }

    /// Pushes the monitor's state into a window's `Studio` global.
    pub(super) fn push_monitor(&self, g: &Studio<'_>) {
        let v = self.monitor.view(|id| {
            self.heard_names
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, n)| n.clone())
        });
        g.set_monitor_available(v.available);
        g.set_monitor_wanted(v.wanted);
        g.set_monitor_tone(v.tone);
        g.set_monitor_status(v.status.into());
        g.set_monitor_latency(v.latency.into());
        let text = self.monitor.drag_text.clone().unwrap_or(v.volume_text);
        g.set_monitor_volume(v.volume);
        g.set_monitor_volume_text(text.into());
        g.set_monitor_devices(strings(v.devices));
        g.set_monitor_device(v.device);
        g.set_monitor_device_name(v.device_name.into());
        g.set_monitor_solo(v.solo.into());
    }

    /// Sets the solo mark of each Mixer strip from the host's solo list.
    fn mark_solo(&mut self) {
        let solo = self.monitor.solo().to_vec();
        for i in 0..self.meters.row_count() {
            let Some(mut row) = self.meters.row_data(i) else {
                continue;
            };
            let on = solo.contains(&(row.id as u32));
            if row.solo != on {
                row.solo = on;
                self.meters.set_row_data(i, row);
            }
        }
    }

    /// Whether `id` is soloed on the monitor.
    pub fn is_soloed(&self, id: u32) -> bool {
        self.monitor.solo().contains(&id)
    }

    /// Sets a source's monitor mode (the Mixer's headphones button and menu).
    pub fn set_source_monitor(&mut self, id: i32, code: i32) {
        let Some(monitor) = mode_of(code) else {
            return;
        };
        self.send(Command::SetSourceMonitor {
            id: SourceId::from_raw(id as u32),
            monitor,
        });
    }

    /// Solos a source on the monitor or ends its solo; other soloed sources stay.
    pub fn set_source_solo(&mut self, id: i32, on: bool) {
        let id = id as u32;
        let mut solo: Vec<u32> = self.monitor.solo().to_vec();
        solo.retain(|s| *s != id);
        if on {
            solo.push(id);
        }
        self.send(Command::SetMonitorSolo(
            solo.into_iter().map(SourceId::from_raw).collect(),
        ));
    }

    /// Ends solo for every source.
    pub fn end_monitor_solo(&mut self) {
        self.send(Command::SetMonitorSolo(Vec::new()));
    }

    /// The words for a fader position while it is dragged, before the host confirms it.
    pub fn preview_monitor_volume(&mut self, position: f32) {
        self.monitor.drag_text = Some(fader::volume_text(fader::volume_at(position)));
    }

    /// The monitor volume from a fader position (sent when a drag ends).
    pub fn set_monitor_volume(&mut self, position: f32) {
        self.monitor.drag_text = Some(fader::volume_text(fader::volume_at(position)));
        self.send(Command::SetMonitorVolume(fader::volume_at(position)));
    }

    /// Chooses the device at `index` of the shown list (0: System default).
    pub fn set_monitor_device(&mut self, index: i32) {
        let device = match index {
            0 => None,
            i => match self.monitor.devices.get(i as usize - 1) {
                Some(d) => Some(d.device.clone()),
                // The saved, unlisted device: nothing changes.
                None => return,
            },
        };
        self.send(Command::SetMonitorDevice(device));
    }

    /// Asks the host for the playback devices (a picker is shown).
    pub fn list_monitor_devices(&mut self) {
        if !self.monitor.listing {
            self.monitor.listing = true;
            self.send(Command::ListMonitorDevices);
        }
    }

    /// The Audio settings page is shown (`true`) or left: it lists devices on entry.
    pub fn monitor_page_shown(&mut self, shown: bool) {
        if shown && !self.monitor.page_shown {
            self.list_monitor_devices();
        }
        self.monitor.page_shown = shown;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> api::MonitorStatus {
        api::MonitorStatus {
            available: true,
            device: None,
            volume: 1.0,
            wanted: false,
            playing: false,
            latency_ms: None,
            solo: Vec::new(),
            error: None,
        }
    }

    fn device(id: &str, name: &str, is_default: bool) -> api::MonitorDevice {
        api::MonitorDevice {
            device: id.to_owned(),
            name: name.to_owned(),
            is_default,
        }
    }

    #[test]
    fn the_view_names_the_device_and_the_state() {
        let mut m = MonitorUi {
            status: Some(status()),
            ..MonitorUi::default()
        };
        let v = m.view(|_| None);
        assert_eq!(v.tone, 0);
        assert!(v.status.starts_with("Not in use"));
        assert_eq!(v.devices, ["System default"]);
        m.devices = vec![
            device("a", "Speakers", true),
            device("b", "Headphones", false),
        ];
        m.status = Some(api::MonitorStatus {
            device: Some("b".into()),
            wanted: true,
            playing: true,
            latency_ms: Some(42),
            solo: vec![7],
            ..status()
        });
        let v = m.view(|id| (id == 7).then(|| "Mic".to_owned()));
        assert_eq!(
            v.devices,
            ["System default (Speakers)", "Speakers", "Headphones"]
        );
        assert_eq!((v.device, v.device_name.as_str()), (2, "Headphones"));
        assert_eq!((v.tone, v.status.as_str()), (1, "Playing · 42 ms"));
        assert_eq!(v.solo, "Mic");
        // A saved device that is not listed stays chosen, said in words.
        m.status = Some(api::MonitorStatus {
            device: Some("gone".into()),
            wanted: true,
            error: Some("The device is not available.".into()),
            ..status()
        });
        let v = m.view(|_| None);
        assert_eq!(v.device_name, "Saved device (not connected)");
        assert_eq!(v.device, 3);
        assert_eq!(v.tone, 2);
        assert!(v.status.ends_with("Trying again."));
    }

    #[test]
    fn mode_codes_round_trip() {
        for m in [SourceMonitor::Off, SourceMonitor::Also, SourceMonitor::Only] {
            assert_eq!(mode_of(mode_code(m)), Some(m));
        }
        assert_eq!(mode_of(3), None);
    }
}
