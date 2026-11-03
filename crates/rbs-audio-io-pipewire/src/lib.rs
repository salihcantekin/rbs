//! PipeWire adapter for the RBS audio input port (`docs/specs/audio-io-port.md`).
//!
//! # Endpoints
//!
//! | Kind | PipeWire object | Id |
//! | --- | --- | --- |
//! | `Input` | an `Audio/Source` node (microphone, line input, virtual source) | `source:<node.name>` |
//! | `OutputLoopback` | the monitor ports of an `Audio/Sink` node | `monitor:<node.name>` |
//! | `Application` | every `Stream/Output/Audio` node of one application | `app:<application.name>` |
//!
//! `enumerate` reads the registry and the `default` and `settings` metadata in one
//! short-lived connection; it keeps nothing.
//!
//! # Format
//!
//! Streams deliver `f32` planar audio at the graph rate (`clock.force-rate`, else
//! `clock.rate` of the `settings` metadata, else 48 kHz) with the node's own channels,
//! so libpipewire never resamples or remixes inside this process: the ports of every
//! PipeWire node already run at the graph rate (devices at another rate are converted
//! by their own node in the daemon). Application endpoints are captured in stereo:
//! each of the application's ports is linked to the stream's port of the same
//! position (`MONO` to both), and several streams of one application are summed by
//! PipeWire's port mixer.
//!
//! # How audio travels
//!
//! - `open_input` reads the endpoint's format and creates nothing that outlives the
//!   call (Standby, P13).
//! - `start` runs a PipeWire main loop on its own thread (`rbs-pw-audio`) with one
//!   capture stream. Input and monitor streams name their node in `target.object` and
//!   the session manager links them; application streams are linked by the adapter
//!   itself, port by port, including streams the application opens later.
//! - The stream processes on PipeWire's real-time data thread
//!   (`PW_STREAM_FLAG_RT_PROCESS`, scheduled by libpipewire's `module-rt`). Each cycle
//!   it stamps the buffer with the cycle time (`pw_stream_get_time`) through the
//!   port's clock edge and pushes the planes into the port's sample ring
//!   ([`rbs_audio_io::ring`]); `read` on the audio thread only reads that ring.
//! - The stream sets `node.always-process`, so it is scheduled every cycle even while
//!   nothing plays into a monitor or an application: loopback and application streams
//!   deliver timed silence as the port requires.
//! - The node disappearing (device unplugged), or every PipeWire client of the
//!   application disconnecting (the application exited), makes `read` return
//!   `EndpointLost`.
//!
//! On other platforms the backend compiles and probes as unavailable, so the engine can
//! register it unconditionally.

#[cfg(any(target_os = "linux", test))]
mod graph;
mod pacing;
mod packets;

#[doc(hidden)]
pub use packets::bench;

#[cfg(target_os = "linux")]
mod linux;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.audio-io.pipewire";

pub use platform::PipeWireAudioBackend;

/// Whether a PipeWire daemon socket exists for this process (`PIPEWIRE_REMOTE`,
/// `PIPEWIRE_RUNTIME_DIR` or `XDG_RUNTIME_DIR`). Does not connect. Always `false` on
/// other platforms.
pub fn daemon_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::socket_present()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub use crate::linux::PipeWireAudioBackend;
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use rbs_audio_io::{
        AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, EndpointKind, InputRequest,
    };
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Backend of the PipeWire audio adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct PipeWireAudioBackend;

    impl PipeWireAudioBackend {
        /// Creates the backend. Does nothing until used.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for PipeWireAudioBackend {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "PipeWire audio"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Err(ProbeError::new("PipeWire needs Linux"))
        }
    }

    impl AudioBackend for PipeWireAudioBackend {
        fn enumerate(&self, _kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
            Err(AudioIoError::Unsupported("PipeWire needs Linux"))
        }

        fn open_input(
            &self,
            _request: &InputRequest,
        ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
            Err(AudioIoError::Unsupported("PipeWire needs Linux"))
        }
    }
}
