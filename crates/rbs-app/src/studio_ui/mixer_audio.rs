use crate::studio_host::SourceKind;

pub(super) fn has_mixer_audio(kind: Option<SourceKind>, has_meter: bool) -> bool {
    has_meter || kind.is_some_and(SourceKind::is_audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_metered_picture_source_has_mixer_audio() {
        assert!(has_mixer_audio(Some(SourceKind::Media), true));
        assert!(!has_mixer_audio(Some(SourceKind::Media), false));
        assert!(has_mixer_audio(Some(SourceKind::AudioInput), false));
    }
}
