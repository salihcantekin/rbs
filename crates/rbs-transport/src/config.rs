//! Transport configuration.

use std::fmt;
use std::time::Duration;

use rbs_core::media::ContainerId;

/// A stream key, passphrase or token. Never shown in `Debug`, logs, events or errors.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value, for the adapter to put on the wire. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// What a transport session receives.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportInput {
    /// Encoded packets; the transport does its own framing (RTMP, WHIP).
    Packets,
    /// The chunks of a muxer writing this container (files, SRT with MPEG-TS).
    Container(ContainerId),
}

impl TransportInput {
    /// The `transport.input` capability value for this input.
    pub const fn capability_value(self) -> &'static str {
        match self {
            TransportInput::Packets => crate::caps::INPUT_PACKETS,
            TransportInput::Container(c) => c.transport_input(),
        }
    }
}

/// Bounds of the send queue. When either is reached the drop policy applies.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    /// Most bytes queued.
    pub max_bytes: usize,
    /// Most media duration queued (newest minus oldest queued timestamp).
    pub max_duration: Duration,
}

impl QueueLimits {
    /// Limits of `max_bytes` and `max_duration`.
    pub const fn new(max_bytes: usize, max_duration: Duration) -> Self {
        Self {
            max_bytes,
            max_duration,
        }
    }
}

impl Default for QueueLimits {
    /// 8 MiB or 3 seconds, enough to ride out short network stalls at streaming
    /// bitrates.
    fn default() -> Self {
        Self {
            max_bytes: 8 * 1024 * 1024,
            max_duration: Duration::from_secs(3),
        }
    }
}

/// What to do when the connection drops.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReconnectPolicy {
    /// Report `Disconnected` and stop.
    #[default]
    Off,
    /// Reconnect with exponential back-off.
    Backoff {
        /// First delay.
        initial: Duration,
        /// Largest delay.
        max: Duration,
        /// Give up after this many attempts; `None` retries until `finish`.
        attempts: Option<u32>,
    },
}

/// Everything needed to create a transport session. Build with
/// [`TransportConfig::new`] and the `with_*` methods.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportConfig {
    /// Destination, for example `rtmp://host/app`, `srt://host:port`, `file:///path`.
    /// Must not contain the secret.
    pub url: String,
    /// Stream key or passphrase, kept out of the URL so it is never logged.
    pub secret: Option<Secret>,
    /// What this session receives.
    pub input: TransportInput,
    /// Send queue bounds.
    pub queue: QueueLimits,
    /// Reconnect behaviour.
    pub reconnect: ReconnectPolicy,
    /// Opaque adapter-specific options. The engine never reads them (principle P3).
    pub adapter_options: Vec<(String, String)>,
}

impl TransportConfig {
    /// A configuration with default queue limits, no secret, no reconnect and no
    /// adapter options.
    pub fn new(url: impl Into<String>, input: TransportInput) -> Self {
        Self {
            url: url.into(),
            secret: None,
            input,
            queue: QueueLimits::default(),
            reconnect: ReconnectPolicy::Off,
            adapter_options: Vec::new(),
        }
    }

    /// Sets the stream key or passphrase.
    pub fn with_secret(mut self, secret: Secret) -> Self {
        self.secret = Some(secret);
        self
    }

    /// Sets the queue limits.
    pub fn with_queue(mut self, queue: QueueLimits) -> Self {
        self.queue = queue;
        self
    }

    /// Sets the reconnect policy.
    pub fn with_reconnect(mut self, reconnect: ReconnectPolicy) -> Self {
        self.reconnect = reconnect;
        self
    }

    /// Adds an opaque adapter-specific option.
    pub fn with_adapter_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.adapter_options.push((key.into(), value.into()));
        self
    }

    /// The URL scheme (text before `://`), lowercase.
    pub fn scheme(&self) -> Option<String> {
        self.url
            .split_once("://")
            .map(|(scheme, _)| scheme.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_redacted() {
        let config = TransportConfig::new("rtmp://example/live", TransportInput::Packets)
            .with_secret(Secret::new("live_123_abc"));
        let shown = format!("{config:?}");
        assert!(!shown.contains("live_123_abc"), "{shown}");
        assert_eq!(
            config.secret.as_ref().map(Secret::expose),
            Some("live_123_abc")
        );
    }

    #[test]
    fn scheme_and_input_values() {
        let config = TransportConfig::new(
            "SRT://host:9000",
            TransportInput::Container(ContainerId::MpegTs),
        );
        assert_eq!(config.scheme().as_deref(), Some("srt"));
        assert_eq!(config.input.capability_value(), "container.mpegts");
        assert_eq!(TransportInput::Packets.capability_value(), "packets");
    }
}
