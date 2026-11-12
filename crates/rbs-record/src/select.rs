//! Adapter selection by capability (P3): the registry orders candidates (user choice,
//! preference, hardware first, registration), this module states what the recording
//! needs and tries them in that order.

use std::sync::Arc;

use rbs_codec::{AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, VideoEncoderConfig};
use rbs_codec::{VideoEncoderFactory, caps as codec_caps};
use rbs_core::capability::{CapabilitySet, CapabilityValue, Constraint, Requirement};
use rbs_core::media::ContainerId;
use rbs_core::media::Size;
use rbs_core::registry::{AdapterFactory, AdapterId, Registry};
use rbs_mux::{MuxerFactory, caps as mux_caps};
use rbs_nodes::VideoEncoderNode;
use rbs_transport::{TransportFactory, TransportInput, caps as transport_caps};

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

/// The capabilities a video encoder needs for `config`.
fn video_requirement(config: &VideoEncoderConfig) -> Requirement {
    Requirement::new()
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
        )
}

/// True when `adapter` is a candidate of a video encoder selection for `config`: the
/// registry holds it and it meets the requirement (a running encoder of that adapter
/// may then serve another output, ADR-0047).
pub(crate) fn is_video_candidate(
    registry: &Registry<dyn VideoEncoderFactory>,
    config: &VideoEncoderConfig,
    adapter: &AdapterId,
) -> bool {
    registry
        .select(&video_requirement(config))
        .candidates
        .iter()
        .any(|f| f.id() == *adapter)
}

/// True when `adapter` is a candidate of an audio encoder selection for `config`.
pub(crate) fn is_audio_candidate(
    registry: &Registry<dyn AudioEncoderFactory>,
    config: &AudioEncoderConfig,
    adapter: &AdapterId,
) -> bool {
    registry
        .select(&includes(codec_caps::AUDIO_ENCODE, config.codec.as_str()))
        .candidates
        .iter()
        .any(|f| f.id() == *adapter)
}

/// Why `size` is outside a declared `codec.video.width` or `codec.video.height`.
///
/// An encoder that does not declare a range, or whose probe is missing, is not
/// refused for range (ADR-0102). A declared range that excludes a side is.
fn picture_outside(caps: Option<&CapabilitySet>, size: Size) -> Option<String> {
    let caps = caps?;
    let width = side_outside(
        caps,
        &codec_caps::VIDEO_WIDTH,
        size.width,
        "codec.video.width",
    );
    let height = side_outside(
        caps,
        &codec_caps::VIDEO_HEIGHT,
        size.height,
        "codec.video.height",
    );
    match (width, height) {
        (None, None) => None,
        (Some(w), None) => Some(w),
        (None, Some(h)) => Some(h),
        (Some(w), Some(h)) => Some(format!("{w}; {h}")),
    }
}

/// `name min..=max excludes value` when the declared range does not contain `value`.
fn side_outside(
    caps: &CapabilitySet,
    key: &rbs_core::capability::CapabilityKey,
    value: u32,
    name: &str,
) -> Option<String> {
    match caps.get(key) {
        Some(CapabilityValue::IntRange { min, max }) => {
            let v = i64::from(value);
            (v < *min || v > *max).then(|| format!("{name} {min}..={max} excludes {value}"))
        }
        _ => None,
    }
}

/// The first video encoder, best first, that lists the codec, the input memory kind and
/// the pixel format of `config` and opens a session for it (a candidate that refuses a
/// setting, such as a rate-control mode it lacks, is skipped with its reason). A
/// candidate whose declared width or height range excludes the picture is skipped the
/// same way; if none remain, the error is [`RecordError::NoAdapter`].
pub(crate) fn video_encoder(
    registry: &Registry<dyn VideoEncoderFactory>,
    config: &VideoEncoderConfig,
) -> Result<(VideoEncoderNode, Chosen), RecordError> {
    let selection = registry.select(&video_requirement(config));
    let mut tried = rejected(&selection);
    for factory in &selection.candidates {
        if let Some(why) = picture_outside(registry.capabilities(&factory.id()), config.format.size)
        {
            tried.push((factory.id(), why));
            continue;
        }
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

/// The best transport that lists the URL's scheme and takes `input` (packets, or the
/// chunks of a container). It opens the destination in the output's `prepare`.
pub(crate) fn transport(
    registry: &Registry<dyn TransportFactory>,
    url: &str,
    input: TransportInput,
) -> Result<(Arc<dyn TransportFactory>, Chosen), RecordError> {
    let scheme = scheme_of(url).ok_or_else(|| RecordError::InvalidUrl(url.to_owned()))?;
    let requirement = Requirement::new()
        .with(
            transport_caps::SCHEMES,
            Constraint::Includes(scheme.to_ascii_lowercase()),
        )
        .with(
            transport_caps::INPUT,
            Constraint::Includes(input.capability_value().to_owned()),
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
#[allow(clippy::unwrap_used)]
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

    /// A declared width or height range that excludes the picture skips that encoder
    /// before a session is opened. An encoder that declares no range is still tried.
    #[test]
    fn a_declared_range_that_excludes_the_picture_skips_the_encoder() {
        use std::sync::atomic::{AtomicU64, Ordering};

        use rbs_codec::{CreateError, RateControl, VideoEncoder};
        use rbs_core::media::{CodecId, ColorSpace, MemoryKind, PixelFormat, VideoFormat};
        use rbs_core::registry::{AdapterFactory, ProbeError};
        use rbs_core::timing::FrameRate;

        struct RangeFactory {
            id: &'static str,
            width: Option<(i64, i64)>,
            height: Option<(i64, i64)>,
            creates: Arc<AtomicU64>,
        }

        impl AdapterFactory for RangeFactory {
            fn id(&self) -> AdapterId {
                AdapterId::new(self.id)
            }
            fn display_name(&self) -> &str {
                self.id
            }
            fn probe(&self) -> Result<CapabilitySet, ProbeError> {
                let mut caps = CapabilitySet::new()
                    .with(codec_caps::VIDEO_ENCODE, set(&["h264"]))
                    .with(codec_caps::INPUT_MEMORY, set(&["system"]))
                    .with(codec_caps::INPUT_PIXEL_FORMAT, set(&["nv12"]));
                if let Some((min, max)) = self.width {
                    caps.insert(
                        codec_caps::VIDEO_WIDTH,
                        CapabilityValue::IntRange { min, max },
                    );
                }
                if let Some((min, max)) = self.height {
                    caps.insert(
                        codec_caps::VIDEO_HEIGHT,
                        CapabilityValue::IntRange { min, max },
                    );
                }
                Ok(caps)
            }
        }

        impl VideoEncoderFactory for RangeFactory {
            fn create(&self, _: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
                self.creates.fetch_add(1, Ordering::Relaxed);
                Err(CreateError::Unsupported {
                    what: "not opened (test)".into(),
                })
            }
        }

        fn set(values: &[&str]) -> CapabilityValue {
            CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
        }

        let narrow_creates = Arc::new(AtomicU64::new(0));
        let open_creates = Arc::new(AtomicU64::new(0));
        let mut registry: Registry<dyn VideoEncoderFactory> = Registry::new();
        registry
            .register(Arc::new(RangeFactory {
                id: "narrow",
                width: Some((2, 32)),
                height: Some((2, 18)),
                creates: Arc::clone(&narrow_creates),
            }))
            .unwrap();
        registry
            .register(Arc::new(RangeFactory {
                id: "open",
                width: None,
                height: None,
                creates: Arc::clone(&open_creates),
            }))
            .unwrap();
        registry.probe_all();

        let config = VideoEncoderConfig::new(
            CodecId::H264,
            VideoFormat::new(
                Size::new(64, 36),
                PixelFormat::Nv12,
                ColorSpace::BT709_LIMITED,
            ),
            FrameRate::FPS_30,
            MemoryKind::System,
            RateControl::Cbr { bitrate_bps: 1 },
        );
        let Err(err) = video_encoder(&registry, &config) else {
            panic!("expected NoAdapter");
        };
        let RecordError::NoAdapter { port, tried } = err else {
            panic!("expected NoAdapter, got {err}");
        };
        assert_eq!(port, Port::VideoEncoder);
        assert_eq!(
            narrow_creates.load(Ordering::Relaxed),
            0,
            "skipped before open"
        );
        assert_eq!(
            open_creates.load(Ordering::Relaxed),
            1,
            "no range: still tried"
        );
        assert!(
            tried.iter().any(|(id, why)| {
                id.as_str() == "narrow" && why.contains("excludes") && why.contains("64")
            }),
            "{tried:?}"
        );
    }
}
