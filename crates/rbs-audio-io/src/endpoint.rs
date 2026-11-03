//! Audio endpoints: the devices and applications audio can be read from.

use std::fmt;

use rbs_core::media::AudioFormat;
use rbs_core::timing::SampleRate;

/// What an endpoint captures.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointKind {
    /// A capture device: microphone, line input, audio interface input.
    Input,
    /// What an output device plays (system audio loopback).
    OutputLoopback,
    /// The audio one application (process tree) plays.
    Application,
}

impl EndpointKind {
    /// Every kind, in declaration order.
    pub const ALL: [EndpointKind; 3] = [
        EndpointKind::Input,
        EndpointKind::OutputLoopback,
        EndpointKind::Application,
    ];

    /// The name used in capability sets (`input`, `output_loopback`, `application`).
    pub const fn as_str(self) -> &'static str {
        match self {
            EndpointKind::Input => "input",
            EndpointKind::OutputLoopback => "output_loopback",
            EndpointKind::Application => "application",
        }
    }
}

impl fmt::Display for EndpointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Opaque, adapter-defined endpoint id.
///
/// Stable for the lifetime of the endpoint, and for devices also across restarts where
/// the OS allows it (WASAPI endpoint ids, CoreAudio device UIDs, PipeWire
/// `node.name`), because scenes store it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(String);

impl EndpointId {
    /// Creates an id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One endpoint as listed by [`AudioBackend::enumerate`](crate::AudioBackend::enumerate).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEndpoint {
    /// Id to pass to `open` and to store in scenes.
    pub id: EndpointId,
    /// What the endpoint captures.
    pub kind: EndpointKind,
    /// Human readable name from the OS, for UIs.
    pub name: String,
    /// The format the endpoint delivers when the request names no rate (the OS mix
    /// format or the device's current format).
    pub format: AudioFormat,
    /// Every rate the adapter can deliver for this endpoint without resampling in
    /// software, native rate included. Requests pick from this list.
    pub sample_rates: Vec<SampleRate>,
    /// True for the OS default endpoint of its kind.
    pub is_default: bool,
}

impl AudioEndpoint {
    /// An endpoint that delivers only its native format.
    pub fn new(
        id: EndpointId,
        kind: EndpointKind,
        name: impl Into<String>,
        format: AudioFormat,
    ) -> Self {
        Self {
            id,
            kind,
            name: name.into(),
            format,
            sample_rates: vec![format.sample_rate],
            is_default: false,
        }
    }

    /// Sets the rates the endpoint can deliver. The native rate is added if missing.
    pub fn with_sample_rates(mut self, rates: &[SampleRate]) -> Self {
        self.sample_rates = rates.to_vec();
        if !self.sample_rates.contains(&self.format.sample_rate) {
            self.sample_rates.insert(0, self.format.sample_rate);
        }
        self
    }

    /// Marks the endpoint as the OS default of its kind.
    pub fn with_default(mut self, is_default: bool) -> Self {
        self.is_default = is_default;
        self
    }
}

/// Whether the OS lets this process capture an endpoint kind.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Capturing is allowed.
    Granted,
    /// The user or a policy refused.
    Denied,
    /// Not asked yet; `request_permission` shows the OS prompt.
    NotDetermined,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_rate_is_always_listed() {
        let endpoint = AudioEndpoint::new(
            EndpointId::new("a"),
            EndpointKind::Input,
            "A",
            AudioFormat::STEREO_48K,
        )
        .with_sample_rates(&[SampleRate::HZ_44100]);
        assert_eq!(
            endpoint.sample_rates,
            [SampleRate::HZ_48000, SampleRate::HZ_44100]
        );
        assert_eq!(EndpointKind::OutputLoopback.as_str(), "output_loopback");
    }
}
