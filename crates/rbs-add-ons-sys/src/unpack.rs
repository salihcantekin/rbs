//! tar.gz and tar.bz2 decoding. Nothing is written to disk and nothing is checked
//! here: `rbs-add-ons` checks every entry it is handed.

use std::io::Read;
use std::ops::ControlFlow;

use bzip2::read::MultiBzDecoder;
use flate2::read::MultiGzDecoder;
use rbs_add_ons::{ArchiveEntry, EntryKind, Format, Unpack, UnpackError};
use tar::EntryType;

/// The production [`Unpack`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SysUnpack;

impl Unpack for SysUnpack {
    fn unpack(
        &self,
        format: Format,
        input: &mut dyn Read,
        visit: &mut dyn FnMut(ArchiveEntry<'_>) -> ControlFlow<()>,
    ) -> Result<(), UnpackError> {
        let decoded: Box<dyn Read + '_> = match format {
            Format::TarGz => Box::new(MultiGzDecoder::new(input)),
            Format::TarBz2 => Box::new(MultiBzDecoder::new(input)),
        };
        let mut archive = tar::Archive::new(decoded);
        let entries = archive.entries().map_err(|e| UnpackError(e.to_string()))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| UnpackError(e.to_string()))?;
            // PAX headers and GNU long names are folded into the entry by `tar`.
            let kind = match entry.header().entry_type() {
                EntryType::Regular | EntryType::Continuous => EntryKind::File,
                EntryType::Directory => EntryKind::Dir,
                EntryType::Symlink => EntryKind::Symlink,
                EntryType::Link => EntryKind::HardLink,
                _ => EntryKind::Other,
            };
            let name = entry.path_bytes().into_owned();
            let link = entry.link_name_bytes().map(|l| l.into_owned());
            let mode = entry.header().mode().unwrap_or(0o644);
            let flow = visit(ArchiveEntry {
                name,
                kind,
                mode,
                link,
                data: &mut entry,
            });
            if flow.is_break() {
                return Ok(());
            }
        }
        Ok(())
    }
}
