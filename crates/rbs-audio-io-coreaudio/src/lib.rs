//! CoreAudio adapter for the RBS audio input port (`docs/specs/audio-io-port.md`).
//!
//! # Endpoints
//!
//! | Kind | CoreAudio object | Id |
//! | --- | --- | --- |
//! | `Input` | a device with input channels (microphone, line input, interface) | `input:<device UID>` |
//! | `OutputLoopback` | a device with output channels, read through a process tap of its output (macOS 14.4+) | `output:<device UID>` |
//! | `Application` | a process playing audio, read through a process tap (macOS 14.4+) | `app:<process id>` |
//!
//! Device UIDs are stable across restarts. `enumerate` only reads properties of the
//! HAL's objects; it creates nothing and never prompts.
//!
//! # Format
//!
//! Input endpoints deliver the device's nominal rate and its input channels (the first
//! eight, as `Discrete(8)`, for wider interfaces) as the HAL delivers them to an IO
//! proc, so nothing is converted and endpoints list only the nominal rate. Loopback
//! and application endpoints are stereo mixdowns made by the tap at the output
//! device's nominal rate (the default output device for applications); the HAL mixes
//! and converts, the accepted exception in the port's format rule.
//!
//! # How audio travels
//!
//! - `open_input` reads the endpoint's format and keeps nothing (Standby, P13). For
//!   `Input` it checks the microphone permission and never prompts.
//! - `start` creates, for loopback and applications, a private process tap and a
//!   private aggregate device that reads it (clocked by the output device, with drift
//!   compensation), then registers an IO proc on the input device or the aggregate. The
//!   IO proc runs on the HAL's real-time IO thread: it stamps each cycle through the
//!   port's clock edge from its host time and pushes the samples into the port's sample
//!   ring ([`rbs_audio_io::ring`]). `read` on the audio thread only reads that ring.
//!   The adapter starts no threads of its own.
//! - Aggregate devices run continuously, so loopback and application streams deliver
//!   timed silence while nothing plays, as the port requires.
//! - A device that disappears (`kAudioDevicePropertyDeviceIsAlive`) or a captured
//!   process that exits (it leaves the HAL's process list) makes `read` return
//!   `EndpointLost`; a nominal rate change makes it return `FormatChanged`.
//!
//! # Permissions
//!
//! Microphones need the microphone permission (`NSMicrophoneUsageDescription` in the
//! host application's bundle); [`AudioBackend::request_permission`] shows the prompt.
//! Process taps need the "System Audio Recording" permission, which macOS has no
//! public query for: the first tap shows the OS prompt itself, and a tap without
//! permission delivers silence. Those kinds therefore report `Granted`.
//!
//! On other platforms the backend compiles and probes as unavailable, so the engine can
//! register it unconditionally.
//!
//! [`AudioBackend::request_permission`]: rbs_audio_io::AudioBackend::request_permission

use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::media::{ChannelLayout, MAX_CHANNELS};

// Used by the macOS backend; elsewhere only by tests and the benchmark.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod packets;

#[cfg(target_os = "macos")]
mod mac;

#[doc(hidden)]
pub use packets::bench;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.audio-io.coreaudio";

/// First macOS version (major, minor) whose process taps the adapter uses.
pub const PROCESS_TAP_VERSION: (isize, isize) = (14, 4);

pub use platform::CoreAudioBackend;

#[cfg(target_os = "macos")]
#[doc(hidden)]
pub use mac::DIAG;

/// IO procs, process taps and aggregate devices the adapter owns right now, for the
/// conformance suite's resource checks.
#[doc(hidden)]
pub fn owned_resources() -> u64 {
    OWNED.load(Ordering::SeqCst)
}

static OWNED: AtomicU64 = AtomicU64::new(0);

/// A resource counted by [`owned_resources`] while this guard lives.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct Owned(());

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Owned {
    pub(crate) fn new() -> Self {
        OWNED.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        OWNED.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The layout of the first `MAX_CHANNELS` of a device's `channels` input channels.
/// CoreAudio input channels carry no reliable speaker positions, so only mono and
/// stereo are named.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn layout(channels: usize) -> Option<ChannelLayout> {
    match channels {
        0 => None,
        1 => Some(ChannelLayout::Mono),
        2 => Some(ChannelLayout::Stereo),
        n => Some(ChannelLayout::Discrete(
            u8::try_from(n.min(MAX_CHANNELS)).unwrap_or(u8::MAX),
        )),
    }
}

/// Endpoint id prefixes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod ids {
    use rbs_audio_io::{EndpointId, EndpointKind};

    pub(crate) const INPUT: &str = "input:";
    pub(crate) const OUTPUT: &str = "output:";
    pub(crate) const APP: &str = "app:";

    /// The id of a device (by UID) or a process (by pid).
    pub(crate) fn endpoint(kind: EndpointKind, native: &str) -> EndpointId {
        let prefix = match kind {
            EndpointKind::OutputLoopback => OUTPUT,
            EndpointKind::Application => APP,
            _ => INPUT,
        };
        EndpointId::new(format!("{prefix}{native}"))
    }

    /// What an id names: its kind and the device UID or pid string.
    pub(crate) fn parse(id: &EndpointId) -> Option<(EndpointKind, &str)> {
        let id = id.as_str();
        [
            (INPUT, EndpointKind::Input),
            (OUTPUT, EndpointKind::OutputLoopback),
            (APP, EndpointKind::Application),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| {
            id.strip_prefix(prefix)
                .filter(|rest| !rest.is_empty())
                .map(|rest| (kind, rest))
        })
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub use crate::mac::CoreAudioBackend;
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use rbs_audio_io::{
        AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, EndpointKind, InputRequest,
    };
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Backend of the CoreAudio adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct CoreAudioBackend;

    impl CoreAudioBackend {
        /// Creates the backend. Does nothing until used.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for CoreAudioBackend {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "CoreAudio"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Err(ProbeError::new("CoreAudio needs macOS"))
        }
    }

    impl AudioBackend for CoreAudioBackend {
        fn enumerate(&self, _kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
            Err(AudioIoError::Unsupported("CoreAudio needs macOS"))
        }

        fn open_input(
            &self,
            _request: &InputRequest,
        ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
            Err(AudioIoError::Unsupported("CoreAudio needs macOS"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_audio_io::{EndpointId, EndpointKind};

    #[test]
    fn ids_round_trip() {
        let uid = "BuiltInMicrophoneDevice";
        for kind in EndpointKind::ALL {
            assert_eq!(ids::parse(&ids::endpoint(kind, uid)), Some((kind, uid)));
        }
        assert_eq!(ids::parse(&EndpointId::new("input:")), None);
        assert_eq!(ids::parse(&EndpointId::new("capture:x")), None);
    }

    #[test]
    fn wide_devices_deliver_eight_discrete_channels() {
        assert_eq!(layout(0), None);
        assert_eq!(layout(1), Some(ChannelLayout::Mono));
        assert_eq!(layout(2), Some(ChannelLayout::Stereo));
        assert_eq!(layout(6), Some(ChannelLayout::Discrete(6)));
        assert_eq!(layout(18), Some(ChannelLayout::Discrete(8)));
    }
}
