//! A chapter writer for the recording tests (`record.rs`).

/// Markers as Matroska chapters, as `rbs-app`'s writer does (ADR-0159 decision 4).
#[derive(Debug)]
pub struct MatroskaChapters;

impl rbs_studio_host::studio_host::ChapterWriter for MatroskaChapters {
    fn append(
        &self,
        file: &mut std::fs::File,
        chapters: &[rbs_studio_host::studio_host::Chapter<'_>],
    ) -> Result<rbs_studio_host::studio_host::Applied, String> {
        use rbs_studio_host::studio_host::Applied;
        let chapters: Vec<rbs_mux_matroska::Chapter<'_>> = chapters
            .iter()
            .map(|c| rbs_mux_matroska::Chapter {
                start_ns: c.start_ns,
                title: c.title,
            })
            .collect();
        match rbs_mux_matroska::append_chapters(file, &chapters) {
            Ok(()) => Ok(Applied::Written),
            Err(rbs_mux_matroska::ChapterError::Io(e)) => Err(e.to_string()),
            Err(_) => Ok(Applied::NotSupported),
        }
    }
}
