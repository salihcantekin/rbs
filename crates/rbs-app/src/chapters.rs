//! Markers as Matroska chapters: the studio host's [`ChapterWriter`] in this
//! composition root (ADR-0159 decision 4).

use std::fs::File;

use crate::studio_host::{Applied, Chapter, ChapterWriter};

/// Appends chapters with `rbs-mux-matroska`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MatroskaChapters;

impl ChapterWriter for MatroskaChapters {
    fn append(&self, file: &mut File, chapters: &[Chapter<'_>]) -> Result<Applied, String> {
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
            // Written by another Matroska adapter, or by an output that could not seek.
            Err(_) => Ok(Applied::NotSupported),
        }
    }
}
