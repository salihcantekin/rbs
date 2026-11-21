//! The output fields of `settings.set` (protocol 1.100, ADR-0164 decision 5): Swap
//! after transition, Separate files, the replay buffer and the vertical recording and
//! stream sections. Checked whole with the rest of the patch before anything is
//! applied; each change goes back to the window, which saves the file it owns.

use std::path::{Path, PathBuf};

use super::*;
use crate::record_profile::{HARDWARE_LIMIT_ASK, HARDWARE_LIMIT_SKIP, LimitPolicy, SeparateFiles};
use crate::replay_settings::ReplaySetup;

/// The output fields of a patch, checked.
#[derive(Default)]
pub(super) struct OutputPatch {
    swap: Option<bool>,
    files: Option<SeparateFiles>,
    replay: Option<ReplaySetup>,
    vertical_profile: Option<Option<RecordProfile>>,
    vertical_stream: Option<StreamProfile>,
}

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidParams, why)
}

/// `invalid_params` for a relative folder (ADR-0164 decision 7); empty is allowed
/// where the field says so.
fn absolute(folder: &str, what: &str) -> Result<(), ApiError> {
    if !folder.is_empty() && Path::new(folder).is_relative() {
        return Err(invalid(format!("{what} must be an absolute path")));
    }
    Ok(())
}

impl Host {
    /// Checks the output fields of `patch` against the studio's state; nothing changes.
    pub(super) fn check_output_settings(
        &self,
        patch: &rbs_api::request::SettingsPatch,
    ) -> Result<OutputPatch, ApiError> {
        let mut out = OutputPatch {
            swap: patch.swap_after_transition,
            ..OutputPatch::default()
        };
        if let Some(f) = &patch.separate_files {
            let on_hardware_limit = match f.on_hardware_limit {
                rbs_api::request::HardwareLimitPolicy::Ask => HARDWARE_LIMIT_ASK,
                rbs_api::request::HardwareLimitPolicy::Skip => HARDWARE_LIMIT_SKIP,
                rbs_api::request::HardwareLimitPolicy::Cpu => {
                    return Err(invalid(
                        "on_hardware_limit cpu is not available until the CPU encoder for separate files exists",
                    ));
                }
            };
            let files = SeparateFiles {
                enabled: f.enabled,
                on_hardware_limit: on_hardware_limit.to_owned(),
                ..SeparateFiles::default()
            };
            let same = files.enabled == self.recording.separate_files
                && files.limit_policy() == self.recording.limit_policy;
            if !same && self.any_recording() {
                return Err(ApiError::new(
                    ErrorKind::Conflict,
                    "stop the recording before changing Separate files",
                ));
            }
            out.files = Some(files);
        }
        if let Some(r) = &patch.replay {
            if let Some(folder) = &r.folder {
                absolute(folder, "the replay folder")?;
            }
            let setup = ReplaySetup {
                arm_on_start: r.arm_on_start,
                length_secs: r.length_secs,
                memory_mib: r.memory_mib,
                audio: r.audio,
                folder: r
                    .folder
                    .as_deref()
                    .filter(|f| !f.is_empty())
                    .map(PathBuf::from),
                ..ReplaySetup::default()
            };
            if let Some(why) = setup.problem() {
                return Err(invalid(why));
            }
            out.replay = Some(setup);
        }
        if let Some(asked) = &patch.vertical_profile {
            let profile = match asked {
                Some(p) => {
                    check_profile(&p.id)?;
                    absolute(&p.folder, "the folder")?;
                    let profile = RecordProfile::from_api(p);
                    if let Some(why) = profile.problem() {
                        return Err(invalid(why));
                    }
                    if let Some(v) = &self.verticals.vertical {
                        output_size_for_canvas(profile.output_size, (v.size.width, v.size.height))?;
                    }
                    Some(profile)
                }
                None => None,
            };
            let running = self.outs[slot(OutputCanvas::Vertical)].recording.is_some();
            if running && profile != self.recording.vertical_profile {
                return Err(ApiError::new(
                    ErrorKind::Conflict,
                    "stop the vertical recording before changing its settings",
                ));
            }
            out.vertical_profile = Some(profile);
        }
        match &patch.vertical_stream {
            None => {}
            Some(None) => {
                return Err(invalid(
                    "vertical_stream cannot be null: its server is the local user's",
                ));
            }
            Some(Some(p)) => {
                let Some(current) = &self.streaming.vertical_stream_profile else {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the vertical stream is not set up; set its server in the window first",
                    ));
                };
                let stream = current
                    .with_api(p)
                    .map_err(|why| invalid(why.into_text()))?;
                if let Some(v) = &self.verticals.vertical {
                    output_size_for_canvas(stream.output_size, (v.size.width, v.size.height))?;
                }
                let running = self.outs[slot(OutputCanvas::Vertical)].streaming.is_some();
                if running && stream != *current {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "end the vertical stream before changing its settings",
                    ));
                }
                out.vertical_stream = Some(stream);
            }
        }
        Ok(out)
    }

    /// Applies checked output fields; each change goes to the window. True when one
    /// changed.
    pub(super) fn apply_output_settings(
        &mut self,
        o: OutputPatch,
        notify: &dyn Fn(Update),
    ) -> bool {
        let mut changed = false;
        if let Some(on) = o.swap
            && on != self.swap_after_transition
        {
            self.swap_after_transition = on;
            notify(Update::SwapAfterTransition(on));
            changed = true;
        }
        if let Some(files) = o.files {
            let policy = files.limit_policy();
            if files.enabled != self.recording.separate_files
                || policy != self.recording.limit_policy
            {
                self.recording.separate_files = files.enabled;
                self.recording.limit_policy = policy;
                notify(Update::SeparateFiles(files));
                changed = true;
            }
        }
        if let Some(setup) = o.replay
            && setup != self.replay.setup
        {
            self.replay.setup = setup.clone();
            self.replay_changed(notify);
            notify(Update::ReplaySetup(setup));
            changed = true;
        }
        if let Some(profile) = o.vertical_profile
            && profile != self.recording.vertical_profile
        {
            self.recording.vertical_profile = profile.clone();
            notify(Update::VerticalProfile(profile));
            changed = true;
        }
        if let Some(stream) = o.vertical_stream
            && self.streaming.vertical_stream_profile.as_ref() != Some(&stream)
        {
            self.streaming.vertical_stream_profile = Some(stream.clone());
            self.availability_changed();
            notify(Update::VerticalStreamProfile(stream));
            changed = true;
        }
        changed
    }

    /// The output fields of `settings.get` (protocol 1.100).
    pub(in crate::studio_host) fn api_output_settings(&self, s: &mut api::Settings) {
        s.swap_after_transition = Some(self.swap_after_transition);
        s.separate_files = Some(api::SeparateFilesSettings {
            enabled: self.recording.separate_files,
            on_hardware_limit: match self.recording.limit_policy.0 {
                LimitPolicy::Ask => rbs_api::request::HardwareLimitPolicy::Ask,
                LimitPolicy::Skip => rbs_api::request::HardwareLimitPolicy::Skip,
            },
        });
        let r = &self.replay.setup;
        s.replay = Some(api::ReplaySettings {
            arm_on_start: r.arm_on_start,
            length_secs: r.length_secs,
            memory_mib: r.memory_mib,
            audio: r.audio,
            folder: r.folder.as_ref().map(|f| f.display().to_string()),
        });
        s.vertical_profile = Some(
            self.recording
                .vertical_profile
                .as_ref()
                .map(|p| Box::new(p.to_api())),
        );
        s.vertical_stream = Some(
            self.streaming
                .vertical_stream_profile
                .as_ref()
                .map(|p| Box::new(p.to_api())),
        );
    }
}
