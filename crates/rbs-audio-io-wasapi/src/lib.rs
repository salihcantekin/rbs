//! WASAPI adapter for the RBS audio input port (`docs/specs/audio-io-port.md`).
//!
//! # Endpoints
//!
//! | Kind | WASAPI object | Id |
//! | --- | --- | --- |
//! | `Input` | an active capture endpoint (microphone, line input) | `capture:<endpoint id>` |
//! | `OutputLoopback` | an active render endpoint, read with `AUDCLNT_STREAMFLAGS_LOOPBACK` | `loopback:<endpoint id>` |
//! | `Application` | a process with an audio session on a render endpoint, read with process loopback (its process tree) | `app:<process id>` |
//!
//! Endpoint ids are the `IMMDevice` ids, stable across restarts. `enumerate` creates
//! the device enumerator and, per endpoint, an audio client to read the mix format,
//! and releases them before it returns; it never starts a stream, so microphone
//! indicators stay off.
//!
//! # Format
//!
//! Device endpoints deliver the shared-mode mix format (rate and channels) without
//! `AUTOCONVERTPCM`: WASAPI converts nothing, so endpoints list only the mix rate.
//! Devices with more than eight channels deliver their first eight as
//! `Discrete(8)`. Application endpoints have no mix format: the adapter asks process
//! loopback for 32-bit float stereo at the requested rate (44.1, 48 or 96 kHz, else the
//! default render endpoint's rate) and the OS mixer converts, the accepted exception
//! in the port's format rule.
//!
//! # How audio travels
//!
//! - `open_input` reads the endpoint's format and keeps nothing (Standby, P13).
//! - `start` runs one capture thread (`rbs-wasapi-audio`) registered with MMCSS
//!   ("Pro Audio"). It opens an event-driven shared-mode stream, waits for the event
//!   with a timeout of one period, and pushes every packet into the port's sample ring
//!   ([`rbs_audio_io::ring`]) stamped through the port's clock edge from its QPC
//!   position. `read` on the audio thread only reads that ring.
//! - Loopback and process loopback deliver no packets while nothing plays; the thread
//!   then fills timed silence, so `read` delivers continuous audio.
//! - A device that disappears (`AUDCLNT_E_DEVICE_INVALIDATED` and the endpoint no longer
//!   active) or a captured process that exits makes `read` return `EndpointLost`; a
//!   device that is still active but invalidated the stream (its format changed) makes
//!   it return `FormatChanged`.
//!
//! On other platforms the backend compiles and probes as unavailable, so the engine can
//! register it unconditionally.

use std::sync::atomic::{AtomicU64, Ordering};

// Used by the Windows backend; elsewhere only by tests and the benchmark.
#[cfg_attr(not(windows), allow(dead_code))]
mod packets;
#[cfg_attr(not(windows), allow(dead_code))]
mod wave;

#[cfg(windows)]
mod win;

#[doc(hidden)]
pub use packets::bench;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.audio-io.wasapi";

/// First Windows build with process loopback (Windows 10 version 2004).
pub const PROCESS_LOOPBACK_BUILD: u32 = 19_041;

pub use platform::WasapiAudioBackend;

/// Threads and OS audio streams the adapter owns right now, for the conformance
/// suite's resource checks. Counts only what this adapter created: the capture thread
/// and its audio client of every Active stream.
#[doc(hidden)]
pub fn owned_resources() -> u64 {
    OWNED.load(Ordering::SeqCst)
}

static OWNED: AtomicU64 = AtomicU64::new(0);

/// A resource counted by [`owned_resources`] while this guard lives.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Owned(());

#[cfg_attr(not(windows), allow(dead_code))]
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

/// Endpoint id prefixes.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod ids {
    use rbs_audio_io::{EndpointId, EndpointKind};

    pub(crate) const CAPTURE: &str = "capture:";
    pub(crate) const LOOPBACK: &str = "loopback:";
    pub(crate) const APP: &str = "app:";

    /// The id of a device endpoint or a process.
    pub(crate) fn endpoint(kind: EndpointKind, native: &str) -> EndpointId {
        let prefix = match kind {
            EndpointKind::OutputLoopback => LOOPBACK,
            EndpointKind::Application => APP,
            _ => CAPTURE,
        };
        EndpointId::new(format!("{prefix}{native}"))
    }

    /// What an id names: its kind and the device id or process id string.
    pub(crate) fn parse(id: &EndpointId) -> Option<(EndpointKind, &str)> {
        let id = id.as_str();
        [
            (CAPTURE, EndpointKind::Input),
            (LOOPBACK, EndpointKind::OutputLoopback),
            (APP, EndpointKind::Application),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| {
            id.strip_prefix(prefix)
                .filter(|rest| !rest.is_empty())
                .map(|rest| (kind, rest))
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ids_round_trip() {
            let device = "{0.0.1.00000000}.{6c4b6d0e-7d52-4b5c-9d38-1f0b3c3b9f11}";
            for kind in EndpointKind::ALL {
                let id = endpoint(kind, device);
                assert_eq!(parse(&id), Some((kind, device)));
            }
            assert_eq!(parse(&EndpointId::new("app:")), None);
            assert_eq!(parse(&EndpointId::new("source:mic")), None);
        }
    }
}

#[cfg(windows)]
mod platform {
    pub use crate::win::WasapiAudioBackend;
}

#[cfg(not(windows))]
mod platform {
    use rbs_audio_io::{
        AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, EndpointKind, InputRequest,
    };
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Backend of the WASAPI audio adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct WasapiAudioBackend;

    impl WasapiAudioBackend {
        /// Creates the backend. Does nothing until used.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for WasapiAudioBackend {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "WASAPI audio"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Err(ProbeError::new("WASAPI needs Windows"))
        }
    }

    impl AudioBackend for WasapiAudioBackend {
        fn enumerate(&self, _kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
            Err(AudioIoError::Unsupported("WASAPI needs Windows"))
        }

        fn open_input(
            &self,
            _request: &InputRequest,
        ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
            Err(AudioIoError::Unsupported("WASAPI needs Windows"))
        }
    }
}
