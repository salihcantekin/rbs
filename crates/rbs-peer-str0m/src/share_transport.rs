//! The guest share's transport factory (peer-port.md, "Share transport"): the studio
//! starts a guest share as an output to `rbs-guest://<session id>`, and this factory
//! opens the media path of that sharing session of this process. The URL names a
//! session only; nothing about the peer is in it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::CodecId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_peer::SessionId;
use rbs_transport::caps;
use rbs_transport::{CreateError, Transport, TransportConfig, TransportFactory};

use crate::link::WebRtcMedia;

/// The registry id of [`GuestShareTransports`].
pub const SHARE_TRANSPORT_ID: &str = "org.rbs.transport.guest-share";
/// The URL scheme of a guest share output.
pub const SHARE_SCHEME: &str = "rbs-guest";

/// The sharing sessions of one [`WebRtcPeerFactory`](crate::WebRtcPeerFactory), by
/// session id. Weak: a closed session's media goes away with it.
#[derive(Debug, Default)]
pub(crate) struct Shares {
    sessions: Mutex<Vec<(SessionId, Weak<WebRtcMedia>)>>,
}

impl Shares {
    fn lock(&self) -> MutexGuard<'_, Vec<(SessionId, Weak<WebRtcMedia>)>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a sharing session; drops entries whose session is gone.
    pub(crate) fn add(&self, id: SessionId, media: &Arc<WebRtcMedia>) {
        let mut sessions = self.lock();
        sessions.retain(|(_, m)| m.strong_count() > 0);
        sessions.push((id, Arc::downgrade(media)));
    }

    fn find(&self, id: &str) -> Option<Arc<WebRtcMedia>> {
        self.lock()
            .iter()
            .find(|(s, _)| s.to_string() == id)
            .and_then(|(_, m)| m.upgrade())
    }
}

/// The URL of the output that sends sharing session `id`.
pub fn share_url(id: SessionId) -> String {
    format!("{SHARE_SCHEME}://{id}")
}

/// Opens a sharing session's media path as an output transport. Made by
/// [`WebRtcPeerFactory::share_transports`](crate::WebRtcPeerFactory::share_transports)
/// and registered in the transport registry by the composition root.
#[derive(Debug, Clone)]
pub struct GuestShareTransports {
    shares: Arc<Shares>,
}

impl GuestShareTransports {
    pub(crate) fn new(shares: Arc<Shares>) -> Self {
        Self { shares }
    }
}

impl AdapterFactory for GuestShareTransports {
    fn id(&self) -> AdapterId {
        AdapterId::new(SHARE_TRANSPORT_ID)
    }

    fn display_name(&self) -> &str {
        "Guest share"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(
                caps::SCHEMES,
                CapabilityValue::Set(vec![SHARE_SCHEME.to_owned()]),
            )
            .with(
                caps::INPUT,
                CapabilityValue::Set(vec![caps::INPUT_PACKETS.to_owned()]),
            )
            .with(
                caps::CODECS,
                CapabilityValue::Set(vec![
                    CodecId::H264.as_str().to_owned(),
                    CodecId::Opus.as_str().to_owned(),
                ]),
            )
            .with(caps::RECONNECT, CapabilityValue::Flag(false))
            .with(caps::BANDWIDTH_ESTIMATE, CapabilityValue::Flag(true)))
    }
}

impl TransportFactory for GuestShareTransports {
    fn create(&self, config: &TransportConfig) -> Result<Box<dyn Transport>, CreateError> {
        let id = config
            .url
            .strip_prefix(SHARE_SCHEME)
            .and_then(|rest| rest.strip_prefix("://"))
            .ok_or_else(|| CreateError::Unsupported {
                what: "URL scheme (this adapter sends to rbs-guest:// URLs)".into(),
            })?;
        let media = self
            .shares
            .find(id)
            .ok_or_else(|| CreateError::InvalidConfig("no such guest share".into()))?;
        let transport = WebRtcMedia::transport(&media.handle())
            .ok_or_else(|| CreateError::InvalidConfig("guest share already sent".into()))?;
        Ok(Box::new(transport))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rbs_transport::TransportInput;

    fn id(seed: u8) -> SessionId {
        SessionId::new([seed; 16]).unwrap()
    }

    #[test]
    fn a_share_is_opened_once_and_only_while_its_session_lives() {
        let shares = Arc::new(Shares::default());
        let factory = GuestShareTransports::new(Arc::clone(&shares));
        let media = WebRtcMedia::new();
        shares.add(id(1), &media);
        let config = TransportConfig::new(share_url(id(1)), TransportInput::Packets);
        assert!(factory.create(&config).is_ok());
        assert!(factory.create(&config).is_err(), "handed out once");
        assert!(
            factory
                .create(&TransportConfig::new(
                    share_url(id(2)),
                    TransportInput::Packets
                ))
                .is_err()
        );
        let other = WebRtcMedia::new();
        shares.add(id(3), &other);
        drop(other);
        assert!(
            factory
                .create(&TransportConfig::new(
                    share_url(id(3)),
                    TransportInput::Packets
                ))
                .is_err(),
            "gone with its session"
        );
        assert!(
            factory
                .create(&TransportConfig::new("rtmp://x/y", TransportInput::Packets))
                .is_err()
        );
    }
}
