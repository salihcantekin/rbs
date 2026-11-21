//! The settings a control API client changed through `settings.set`: the window
//! shows them and saves the files it owns (ADR-0163 Mirror rows, ADR-0164 decision
//! 5). Nothing is sent back; the control thread already uses the new value.

use rbs_app::studio_host::Update;

use super::{RECORDING_VERTICAL, SEPARATE_FILES_SECTION, STREAM_VERTICAL, StudioUi};

impl StudioUi {
    /// Applies a Mirror update; false for any other update.
    pub(super) fn mirror_update(&mut self, u: &Update) -> bool {
        match u {
            Update::Profile(p) => {
                self.profile = p.clone();
                self.folder_serial += 1;
                self.save_profile();
            }
            Update::StreamProfile(p) => {
                // It cannot change the server or the key, so a server being typed
                // here, or a key saved meanwhile, stays.
                let mut p = p.clone();
                p.server = std::mem::take(&mut self.stream.server);
                p.key_saved = self.stream.key_saved;
                self.stream = p;
                self.save_stream();
            }
            Update::SeparateFiles(f) => {
                self.separate_files.enabled = f.enabled;
                // A value a later build wrote stays while it behaves the same.
                if self.separate_files.limit_policy().0 != f.limit_policy().0 {
                    self.separate_files.on_hardware_limit = f.on_hardware_limit.clone();
                }
                if let Err(why) = self
                    .user
                    .set_section(SEPARATE_FILES_SECTION, &self.separate_files)
                {
                    self.profile_note = format!("The Separate files choice was not saved: {why}");
                }
            }
            Update::VerticalProfile(p) => {
                self.vertical_profile = p.clone();
                self.folder_serial += 1;
                let saved = match p {
                    Some(v) => self.user.set_section(RECORDING_VERTICAL, v),
                    None => self.user.remove_section(RECORDING_VERTICAL),
                };
                self.profile_note = match saved {
                    Ok(()) => String::new(),
                    Err(e) => format!("The vertical recording settings could not be saved: {e}"),
                };
            }
            Update::VerticalStreamProfile(p) => {
                let Some(v) = &mut self.vertical_stream else {
                    return true;
                };
                let mut p = p.clone();
                p.server = std::mem::take(&mut v.server);
                p.key_saved = v.key_saved;
                *v = p;
                let v = v.clone();
                self.stream_note = match self.user.set_section(STREAM_VERTICAL, &v) {
                    Ok(()) => String::new(),
                    Err(e) => format!("The vertical stream settings could not be saved: {e}"),
                };
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use rbs_app::record_profile::{RecordProfile, SeparateFiles};
    use rbs_app::stream_profile::StreamProfile;
    use rbs_app::studio_host::Update;

    use super::super::tests::ui;
    use super::{RECORDING_VERTICAL, SEPARATE_FILES_SECTION, STREAM_VERTICAL};

    #[test]
    fn settings_a_client_changed_are_saved_and_keep_the_local_fields() {
        let (mut ui, _d) = ui("mirror");
        let mut vertical = RecordProfile::new();
        vertical.folder = "/vertical".into();
        ui.apply(Update::VerticalProfile(Some(vertical.clone())));
        let saved = ui.user.section::<RecordProfile>(RECORDING_VERTICAL);
        assert_eq!(saved.unwrap(), Some(vertical));
        ui.apply(Update::VerticalProfile(None));
        assert_eq!(
            ui.user
                .section::<RecordProfile>(RECORDING_VERTICAL)
                .unwrap(),
            None
        );

        // A value a later build wrote stays while it behaves the same.
        ui.separate_files.on_hardware_limit = "later".into();
        ui.apply(Update::SeparateFiles(SeparateFiles {
            enabled: false,
            ..SeparateFiles::default()
        }));
        let files = ui
            .user
            .section::<SeparateFiles>(SEPARATE_FILES_SECTION)
            .unwrap()
            .unwrap();
        assert!(!files.enabled);
        assert_eq!(files.on_hardware_limit, "later");

        // The servers and keys are the local user's.
        let mut local = StreamProfile::new();
        local.server = "rtmp://typed.example/app".into();
        local.key_saved = true;
        ui.vertical_stream = Some(local.clone());
        ui.stream = local.clone();
        let mut client = StreamProfile::new();
        client.reconnect = false;
        client.hold_scene = Some(4);
        ui.apply(Update::VerticalStreamProfile(client.clone()));
        ui.apply(Update::StreamProfile(client));
        let saved = ui
            .user
            .section::<StreamProfile>(STREAM_VERTICAL)
            .unwrap()
            .unwrap();
        for p in [&saved, &ui.stream] {
            assert_eq!(p.server, local.server);
            assert!(p.key_saved);
            assert!(!p.reconnect);
            assert_eq!(p.hold_scene, Some(4));
        }
    }
}
