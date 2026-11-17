//! The `keep_alpha` contract of the decode port (ADR-0140 decision 2).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::Path;

use rbs_core::capability::CapabilitySet;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_media_decode::{DecodeError, DecodeTarget, MediaDecoder, MediaDecoderFactory};
use rbs_media_decode_test::{ScriptClip, ScriptFactory};

/// An adapter that knows nothing of alpha: it only implements `open`, so the port's
/// default `open_for` decides.
struct Unaware(ScriptFactory);

impl AdapterFactory for Unaware {
    fn id(&self) -> AdapterId {
        AdapterId::new("org.rbs.test.unaware")
    }

    fn display_name(&self) -> &str {
        "Unaware"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        self.0.probe()
    }
}

impl MediaDecoderFactory for Unaware {
    fn open(&self, path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        self.0.open(path)
    }
}

fn open(
    factory: &dyn MediaDecoderFactory,
    target: &DecodeTarget,
) -> Result<Box<dyn MediaDecoder>, DecodeError> {
    factory.open_for(Path::new("/clip.mov"), target)
}

#[test]
fn keep_alpha_has_no_effect_without_alpha() {
    let keep = DecodeTarget::none().with_keep_alpha();
    assert!(keep.keep_alpha && !DecodeTarget::none().keep_alpha);
    let plain = ScriptFactory::new(ScriptClip::silent_frames());
    assert!(!open(&plain, &keep).unwrap().has_alpha());
    let unaware = Unaware(ScriptFactory::new(ScriptClip::silent_frames()));
    assert!(open(&unaware, &keep).is_ok());
}

#[test]
fn the_default_open_refuses_to_drop_alpha_it_was_asked_to_keep() {
    let unaware = Unaware(ScriptFactory::new(ScriptClip::silent_frames()).with_alpha(true));
    assert!(matches!(
        open(&unaware, &DecodeTarget::none().with_keep_alpha()),
        Err(DecodeError::AlphaUnsupported)
    ));
    // Without the option an alpha file opens as it does today.
    assert!(open(&unaware, &DecodeTarget::none()).unwrap().has_alpha());
}

#[test]
fn an_adapter_keeps_alpha_or_refuses_with_alpha_unsupported() {
    let keep = DecodeTarget::adapter(7).with_keep_alpha();
    let keeps = ScriptFactory::new(ScriptClip::silent_frames()).with_alpha(true);
    assert!(open(&keeps, &keep).unwrap().has_alpha());
    let drops = ScriptFactory::new(ScriptClip::silent_frames()).with_alpha(false);
    assert!(matches!(
        open(&drops, &keep),
        Err(DecodeError::AlphaUnsupported)
    ));
    assert!(open(&drops, &DecodeTarget::adapter(7)).is_ok());
}
