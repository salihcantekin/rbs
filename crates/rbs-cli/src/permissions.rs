//! Asks the OS for permission to capture the camera and microphone before a recording
//! opens them, so a refused or unasked device is left out like an absent one instead
//! of failing the whole recording (a microphone opens at instantiation, which is
//! strict) or starting a source that cannot run (a camera fails only in `prepare`).
//! Requests may show the OS prompt and block until the user answers; `record` runs
//! them on the main thread before the engine starts.

use std::sync::Arc;

use rbs_audio_io::AudioBackend;
use rbs_capture::CaptureFactory;
use rbs_core::capability::Requirement;
use rbs_core::registry::AdapterId;
use rbs_sources::{SourceClass, SourceTypes, class_of};

/// Why a picked target may not be captured; `None` when it may.
pub type Check<'a> = dyn Fn(&'static str, &AdapterId) -> Option<String> + 'a;

/// The probed adapters of `types`, kept to ask each one for its own permission.
pub struct Permissions {
    capture: Vec<Arc<dyn CaptureFactory>>,
    audio: Vec<Arc<dyn AudioBackend>>,
}

impl Permissions {
    /// Every probed capture and audio adapter of `types`.
    pub fn new(types: &mut SourceTypes) -> Self {
        Self {
            capture: types
                .capture_adapters()
                .select(&Requirement::new())
                .candidates,
            audio: types
                .audio_adapters()
                .select(&Requirement::new())
                .candidates,
        }
    }

    /// Asks the adapter that listed a target of `type_id` for permission to capture
    /// it (a prompt when the user has not been asked). Displays are not checked: a
    /// recording needs one, so their failure stays an error.
    pub fn check(&self, type_id: &'static str, adapter: &AdapterId) -> Option<String> {
        match class_of(&rbs_graph::node::NodeTypeId::new_static(type_id))? {
            SourceClass::Capture(rbs_capture::TargetKind::Display) => None,
            SourceClass::Capture(kind) => {
                let f = self.capture.iter().find(|f| f.id() == *adapter)?;
                match f.request_permission(kind) {
                    Ok(rbs_capture::Permission::Granted) => None,
                    Ok(_) => Some(format!(
                        "{} capture permission not granted (allow it in the system's privacy settings)",
                        kind.as_str()
                    )),
                    Err(e) => Some(e.to_string()),
                }
            }
            SourceClass::Audio(kind) => {
                let b = self.audio.iter().find(|b| b.id() == *adapter)?;
                match b.request_permission(kind) {
                    Ok(rbs_audio_io::Permission::Granted) => None,
                    Ok(_) => Some(
                        "audio capture permission not granted (allow it in the system's privacy settings)"
                            .to_owned(),
                    ),
                    Err(e) => Some(e.to_string()),
                }
            }
            _ => None,
        }
    }
}
