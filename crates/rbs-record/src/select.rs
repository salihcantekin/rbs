//! Adapter selection by capability (P3): the registry orders candidates (user choice,
//! preference, hardware first, registration), this module states what the recording
//! needs and tries them in that order.

use std::sync::Arc;

use rbs_codec::{AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, VideoEncoderConfig};
use rbs_codec::{VideoEncoderFactory, caps as codec_caps};
use rbs_core::capability::{Constraint, Requirement};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, Registry};
use rbs_mux::{MuxerFactory, caps as mux_caps};
use rbs_nodes::VideoEncoderNode;
use rbs_transport::{TransportFactory, caps as transport_caps};

use crate::recording::{Port, RecordError};

/// The adapter chosen for one port.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    /// Its id.
    pub adapter: AdapterId,
    /// Its display name.
    pub name: String,
}

impl Chosen {
    fn of<F: ?Sized + AdapterFactory>(factory: &F) -> Self {
        Self {
            adapter: factory.id(),
            name: factory.display_name().to_owned(),
        }
    }
}

/// The scheme of `url` (`file` for `file:///C:/x.mkv`), lower case as written.
pub fn scheme_of(url: &str) -> Option<&str> {
    let (scheme, _) = url.split_once("://")?;
    let valid = !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

fn includes(key: rbs_core::capability::CapabilityKey, value: &str) -> Requirement {
    Requirement::new().with(key, Constraint::Includes(value.to_owned()))
}

/// Collects the rejected adapters of a selection as `(id, reason)`.
fn rejected<F: ?Sized>(selection: &rbs_core::registry::Selection<F>) -> Vec<(AdapterId, String)> {
    selection
        .rejected
        .iter()
        .map(|(id, reason)| (id.clone(), reason.to_string()))
        .collect()
}

/// The first video encoder, best first, that lists the codec, the input memory kind and
/// the pixel format of `config` (and CBR padding when asked for) and opens a session
/// for it.
pub(crate) fn video_encoder(
    registry: &Registry<dyn VideoEncoderFactory>,
    config: &VideoEncoderConfig,
) -> Result<(VideoEncoderNode, Chosen), RecordError> {
    let requirement = Requirement::new()
        .with(
            codec_caps::VIDEO_ENCODE,
            Constraint::Includes(config.codec.as_str().to_owned()),
        )
        .with(
            codec_caps::INPUT_MEMORY,
            Constraint::Includes(config.memory.as_str().to_owned()),
        )
        .with(
            codec_caps::INPUT_PIXEL_FORMAT,
            Constraint::Includes(config.format.pixel_format.as_str().to_owned()),
        );
    let requirement = if config.cbr_padding {
        requirement.with(codec_caps::VIDEO_CBR_PADDING, Constraint::Flag(true))
    } else {
        requirement
    };
    let selection = registry.select(&requirement);
    let mut tried = rejected(&selection);
    for factory in &selection.candidates {
        match VideoEncoderNode::open(Arc::clone(factory), config.clone()) {
            Ok(node) => return Ok((node, Chosen::of(factory.as_ref()))),
            Err(e) => tried.push((factory.id(), e.to_string())),
        }
    }
    Err(RecordError::NoAdapter {
        port: Port::VideoEncoder,
        tried,
    })
}

/// The first audio encoder, best first, that lists the codec and creates a session
/// for `config`.
pub(crate) fn audio_encoder(
    registry: &Registry<dyn AudioEncoderFactory>,
    config: &AudioEncoderConfig,
) -> Result<(Box<dyn AudioEncoder>, Chosen), RecordError> {
    let requirement = includes(codec_caps::AUDIO_ENCODE, config.codec.as_str());
    let selection = registry.select(&requirement);
    let mut tried = rejected(&selection);
    for factory in &selection.candidates {
        match factory.create(config) {
            Ok(session) => return Ok((session, Chosen::of(factory.as_ref()))),
            Err(e) => tried.push((factory.id(), e.to_string())),
        }
    }
    Err(RecordError::NoAdapter {
        port: Port::AudioEncoder,
        tried,
    })
}

/// The best muxer that lists `container`. It is created by the output's `prepare`.
pub(crate) fn muxer(
    registry: &Registry<dyn MuxerFactory>,
    container: ContainerId,
) -> Result<(Arc<dyn MuxerFactory>, Chosen), RecordError> {
    let requirement = includes(mux_caps::CONTAINER, container.as_str());
    let selection = registry.select(&requirement);
    match selection.best() {
        Some(f) => Ok((Arc::clone(f), Chosen::of(f.as_ref()))),
        None => Err(RecordError::NoAdapter {
            port: Port::Muxer,
            tried: rejected(&selection),
        }),
    }
}

/// The best transport that lists the URL's scheme and takes `container` chunks. It
/// opens the destination in the output's `prepare`.
pub(crate) fn transport(
    registry: &Registry<dyn TransportFactory>,
    url: &str,
    container: ContainerId,
) -> Result<(Arc<dyn TransportFactory>, Chosen), RecordError> {
    let scheme = scheme_of(url).ok_or_else(|| RecordError::InvalidUrl(url.to_owned()))?;
    let requirement = Requirement::new()
        .with(
            transport_caps::SCHEMES,
            Constraint::Includes(scheme.to_ascii_lowercase()),
        )
        .with(
            transport_caps::INPUT,
            Constraint::Includes(container.transport_input().to_owned()),
        );
    let selection = registry.select(&requirement);
    match selection.best() {
        Some(f) => Ok((Arc::clone(f), Chosen::of(f.as_ref()))),
        None => Err(RecordError::NoAdapter {
            port: Port::Transport,
            tried: rejected(&selection),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes_are_read_from_urls() {
        assert_eq!(scheme_of("file:///C:/x.mkv"), Some("file"));
        assert_eq!(scheme_of("srt+x://h:1"), Some("srt+x"));
        assert_eq!(scheme_of("C:\\x.mkv"), None);
        assert_eq!(scheme_of("://x"), None);
        assert_eq!(scheme_of("a b://x"), None);
    }
}
