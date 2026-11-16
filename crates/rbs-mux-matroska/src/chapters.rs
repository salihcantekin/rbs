//! Chapters added to a finished Matroska file (ADR-0089 decision 14).
//!
//! The muxer reserves [`RESERVED_BYTES`] as a `Void` at the start of the Segment. After
//! the recording is closed, [`append_chapters`] appends a `Chapters` element at the end
//! of the Segment, patches the Segment size and turns the `Void` into a `SeekHead` that
//! points at it. Not on any hot path: it runs once, after the file is closed.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::ebml;

/// The `Void` the muxer writes as the Segment's first child: room for a `SeekHead`
/// indexing `Info`, `Tracks` and `Chapters` (117 bytes with 8-byte sizes) and the
/// `Void` that fills the rest.
pub(crate) const RESERVED_BYTES: usize = 128;

const EBML_HEADER: u32 = 0x1a45_dfa3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_a966;
const TIMESTAMP_SCALE: u32 = 0x2a_d7b1;
const TRACKS: u32 = 0x1654_ae6b;
const CLUSTER: u32 = 0x1f43_b675;
const CLUSTER_TIMESTAMP: u32 = 0xe7;
const VOID: u32 = 0xec;
const SEEK_HEAD: u32 = 0x114d_9b74;
const SEEK: u32 = 0x4dbb;
const SEEK_ID: u32 = 0x53ab;
const SEEK_POSITION: u32 = 0x53ac;
const CHAPTERS: u32 = 0x1043_a770;
const EDITION_ENTRY: u32 = 0x45b9;
const EDITION_UID: u32 = 0x45bc;
const EDITION_FLAG_DEFAULT: u32 = 0x45db;
const CHAPTER_ATOM: u32 = 0xb6;
const CHAPTER_UID: u32 = 0x73c4;
const CHAPTER_TIME_START: u32 = 0x91;
const CHAPTER_DISPLAY: u32 = 0x80;
const CHAP_STRING: u32 = 0x85;
const CHAP_LANGUAGE: u32 = 0x437c;

/// The default `TimestampScale` (RFC 9559): 1 ms.
const DEFAULT_SCALE_NS: u64 = 1_000_000;
/// Element payloads read whole (the EBML header, a Cluster timestamp) stay below this.
const MAX_READ: u64 = 1024;
/// How far after the first Cluster's timestamp a later Cluster can still start
/// earlier: the muxer's largest interleaving bound (20 s) plus its largest composition
/// delay (10 s). Clusters starting within it are read for the earliest time.
const LATE_WINDOW_NS: u64 = 30_000_000_000;
/// A chapter title is cut to this many bytes (at a character boundary).
pub const MAX_TITLE_BYTES: usize = 1024;

/// One chapter: where it starts in the file and its title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter<'a> {
    /// From the file's first presentation time (its first sample), in nanoseconds.
    pub start_ns: u64,
    /// The chapter's name; cut to [`MAX_TITLE_BYTES`].
    pub title: &'a str,
}

/// A file [`append_chapters`] can write: it also makes the bytes written so far
/// durable, so the three writes reach the disk in order.
pub trait ChapterFile: Read + Write + Seek {
    /// Makes the written bytes durable (`File::sync_data`).
    fn sync(&mut self) -> io::Result<()>;
}

impl ChapterFile for std::fs::File {
    fn sync(&mut self) -> io::Result<()> {
        self.sync_data()
    }
}

/// In memory, for tests: nothing to make durable.
impl ChapterFile for io::Cursor<Vec<u8>> {
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Why [`append_chapters`] wrote nothing, or failed part-way.
#[derive(Debug)]
pub enum ChapterError {
    /// Reading or writing the file failed. A failure after the first write leaves a
    /// valid file (see [`append_chapters`]).
    Io(io::Error),
    /// Not a Matroska file, or one this function cannot follow (an element of unknown
    /// size other than the Segment, a size field it does not write).
    NotMatroska,
    /// The Segment's size is unknown: the recording did not end cleanly, or its output
    /// could not seek.
    Unfinished,
    /// No reserved `Void` at the Segment's start: written by another tool, by an older
    /// build, or the chapters were added already.
    NoRoom,
    /// The file has no Cluster, so no first presentation time.
    NoMedia,
}

impl std::fmt::Display for ChapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::NotMatroska => f.write_str("the file is not a Matroska file RBS can follow"),
            Self::Unfinished => f.write_str("the file did not end cleanly"),
            Self::NoRoom => f.write_str("the file has no room for chapters"),
            Self::NoMedia => f.write_str("the file holds no media"),
        }
    }
}

impl std::error::Error for ChapterError {}

impl From<io::Error> for ChapterError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// An element header: its id, its size (`None` when unknown) and where its payload
/// starts.
struct Header {
    id: u32,
    size: Option<u64>,
    size_len: u8,
    data: u64,
}

fn byte<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

/// Reads an element header at the current position.
fn header<R: Read + Seek>(r: &mut R) -> Result<Header, ChapterError> {
    let first = byte(r)?;
    let id_len = first.leading_zeros() + 1;
    if id_len > 4 {
        return Err(ChapterError::NotMatroska);
    }
    let mut id = u32::from(first);
    for _ in 1..id_len {
        id = (id << 8) | u32::from(byte(r)?);
    }
    let first = byte(r)?;
    let size_len = first.leading_zeros() + 1;
    if size_len > 8 {
        return Err(ChapterError::NotMatroska);
    }
    // `size_len` is 1 to 8: an 8-byte size has no value bits in its first byte.
    let mask = (0xffu16 >> size_len) as u8;
    let mut value = u64::from(first & mask);
    let mut all_ones = first & mask == mask;
    for _ in 1..size_len {
        let b = byte(r)?;
        all_ones &= b == 0xff;
        value = (value << 8) | u64::from(b);
    }
    Ok(Header {
        id,
        size: (!all_ones).then_some(value),
        // `size_len` is at most 8 here.
        size_len: size_len as u8,
        data: r.stream_position()?,
    })
}

/// Reads an unsigned integer payload of `size` bytes (at most 8).
fn uint<R: Read>(r: &mut R, size: u64) -> Result<u64, ChapterError> {
    if size > 8 {
        return Err(ChapterError::NotMatroska);
    }
    let mut value = 0u64;
    for _ in 0..size {
        value = (value << 8) | u64::from(byte(r)?);
    }
    Ok(value)
}

/// Whether the EBML header whose payload is `size` bytes at the current position names
/// the `matroska` doc type.
fn is_matroska<R: Read + Seek>(r: &mut R, size: u64) -> Result<bool, ChapterError> {
    let end = r.stream_position()? + size;
    while r.stream_position()? < end {
        let h = header(r)?;
        let size = h.size.ok_or(ChapterError::NotMatroska)?;
        if h.id == DOC_TYPE {
            if size > MAX_READ {
                return Ok(false);
            }
            // `size` is at most `MAX_READ`.
            let mut doc = vec![0u8; size as usize];
            r.read_exact(&mut doc)?;
            let doc = doc.split(|b| *b == 0).next().unwrap_or_default();
            return Ok(doc == b"matroska");
        }
        r.seek(SeekFrom::Start(h.data + size))?;
    }
    Ok(false)
}

/// What the scan of the Segment finds: where `Info` and `Tracks` are (from the
/// Segment's data start) and the Segment time of the file's first sample.
struct Layout {
    info: Option<u64>,
    tracks: Option<u64>,
    first_ns: u64,
}

/// The Cluster timestamp at the current position (a Cluster's payload), in
/// `TimestampScale` units.
fn cluster_time<R: Read + Seek>(r: &mut R, cluster_end: u64) -> Result<u64, ChapterError> {
    while r.stream_position()? < cluster_end {
        let child = header(r)?;
        let size = child.size.ok_or(ChapterError::NotMatroska)?;
        if child.id == CLUSTER_TIMESTAMP {
            return uint(r, size);
        }
        r.seek(SeekFrom::Start(child.data + size))?;
    }
    Err(ChapterError::NoMedia)
}

/// Scans the Segment's top-level elements from `from` (after the reserved `Void`) to
/// `end`. The file's first sample is the earliest Cluster timestamp: the muxer makes a
/// Cluster's timestamp the earliest presentation time in it, and a stream arriving late
/// can put an earlier one in a later Cluster, never more than [`LATE_WINDOW_NS`] after
/// the first Cluster's.
fn layout<R: Read + Seek>(
    r: &mut R,
    data: u64,
    from: u64,
    end: u64,
) -> Result<Layout, ChapterError> {
    let mut scale = DEFAULT_SCALE_NS;
    let mut info = None;
    let mut tracks = None;
    let mut first: Option<u64> = None;
    let mut earliest = u64::MAX;
    let mut at = from;
    while at < end {
        r.seek(SeekFrom::Start(at))?;
        let h = header(r)?;
        let size = h.size.ok_or(ChapterError::NotMatroska)?;
        match h.id {
            CLUSTER => {
                let t = cluster_time(r, h.data + size)?;
                let window = LATE_WINDOW_NS / scale;
                match first {
                    None => first = Some(t),
                    Some(f) if t > f.saturating_add(window) => break,
                    Some(_) => {}
                }
                earliest = earliest.min(t);
            }
            INFO => {
                info = Some(at - data);
                let info_end = h.data + size;
                while r.stream_position()? < info_end {
                    let child = header(r)?;
                    let size = child.size.ok_or(ChapterError::NotMatroska)?;
                    if child.id == TIMESTAMP_SCALE {
                        scale = uint(r, size)?.max(1);
                    }
                    r.seek(SeekFrom::Start(child.data + size))?;
                }
            }
            TRACKS => tracks = Some(at - data),
            _ => {}
        }
        at = h.data + size;
    }
    if first.is_none() {
        return Err(ChapterError::NoMedia);
    }
    Ok(Layout {
        info,
        tracks,
        first_ns: earliest.saturating_mul(scale),
    })
}

/// `title` cut to at most [`MAX_TITLE_BYTES`] at a character boundary.
fn cut(title: &str) -> &str {
    if title.len() <= MAX_TITLE_BYTES {
        return title;
    }
    let mut end = MAX_TITLE_BYTES;
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    &title[..end]
}

/// The `Chapters` element: one default edition with one atom per chapter, in the order
/// given. `first_ns` is the Segment time of the file's first sample.
fn chapters_element(chapters: &[Chapter<'_>], first_ns: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let all = ebml::open(&mut out, CHAPTERS);
    let edition = ebml::open(&mut out, EDITION_ENTRY);
    ebml::uint(&mut out, EDITION_UID, 1);
    ebml::uint(&mut out, EDITION_FLAG_DEFAULT, 1);
    for (i, chapter) in chapters.iter().enumerate() {
        let atom = ebml::open(&mut out, CHAPTER_ATOM);
        ebml::uint(&mut out, CHAPTER_UID, i as u64 + 1);
        ebml::uint(
            &mut out,
            CHAPTER_TIME_START,
            first_ns.saturating_add(chapter.start_ns),
        );
        let display = ebml::open(&mut out, CHAPTER_DISPLAY);
        ebml::bytes(&mut out, CHAP_STRING, cut(chapter.title).as_bytes());
        ebml::bytes(&mut out, CHAP_LANGUAGE, b"und");
        ebml::close(&mut out, display);
        ebml::close(&mut out, atom);
    }
    ebml::close(&mut out, edition);
    ebml::close(&mut out, all);
    out
}

/// The `SeekHead` indexing `Info`, `Tracks` (when found) and `Chapters` at
/// `chapters` (positions from the Segment's data start), followed by a `Void` filling
/// the reserved room exactly.
fn seek_head(layout: &Layout, chapters: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(RESERVED_BYTES);
    let head = ebml::open(&mut out, SEEK_HEAD);
    let entries = [
        (INFO, layout.info),
        (TRACKS, layout.tracks),
        (CHAPTERS, Some(chapters)),
    ];
    for (id, position) in entries {
        let Some(position) = position else { continue };
        let seek = ebml::open(&mut out, SEEK);
        ebml::bytes(&mut out, SEEK_ID, &id.to_be_bytes());
        ebml::uint(&mut out, SEEK_POSITION, position);
        ebml::close(&mut out, seek);
    }
    ebml::close(&mut out, head);
    let rest = RESERVED_BYTES - out.len();
    ebml::void(&mut out, rest);
    out
}

/// Adds `chapters` to a finished Matroska file this crate wrote (ADR-0089 decision 14).
///
/// `chapters[i].start_ns` counts from the file's first sample; it is placed on the
/// Segment timeline from the first Cluster's timestamp. The file must end with its
/// Segment, which must have a known size (the muxer's `finish` patch) and start with
/// the reserved `Void`. Nothing is written when any check fails, or when `chapters`
/// is empty.
///
/// Three writes, each leaving a valid file: the `Chapters` element is appended past the
/// Segment's end (readers ignore it), the Segment size is patched to cover it (readers
/// that scan reach it), and the `Void` becomes a `SeekHead` pointing at it (readers that
/// stop at the first Cluster find it).
pub fn append_chapters<F: ChapterFile>(
    file: &mut F,
    chapters: &[Chapter<'_>],
) -> Result<(), ChapterError> {
    if chapters.is_empty() {
        return Ok(());
    }
    let len = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    let ebml_header = header(file)?;
    let size = ebml_header.size.ok_or(ChapterError::NotMatroska)?;
    if ebml_header.id != EBML_HEADER || size > MAX_READ || !is_matroska(file, size)? {
        return Err(ChapterError::NotMatroska);
    }
    file.seek(SeekFrom::Start(ebml_header.data + size))?;
    let segment_at = file.stream_position()?;
    let segment = header(file)?;
    if segment.id != SEGMENT {
        return Err(ChapterError::NotMatroska);
    }
    let Some(segment_size) = segment.size else {
        return Err(ChapterError::Unfinished);
    };
    // The muxer writes an 8-byte size; the patch keeps that width.
    if segment.size_len != 8 {
        return Err(ChapterError::NoRoom);
    }
    let segment_end = segment.data + segment_size;
    if segment_end != len {
        return Err(ChapterError::Unfinished);
    }
    let reserved = header(file)?;
    if reserved.id != VOID
        || reserved.size.map(|s| reserved.data + s) != Some(segment.data + RESERVED_BYTES as u64)
    {
        return Err(ChapterError::NoRoom);
    }
    let layout = layout(
        file,
        segment.data,
        segment.data + RESERVED_BYTES as u64,
        segment_end,
    )?;

    let element = chapters_element(chapters, layout.first_ns);
    file.seek(SeekFrom::Start(segment_end))?;
    file.write_all(&element)?;
    file.flush()?;
    file.sync()?;

    let mut size = Vec::with_capacity(8);
    ebml::size8(&mut size, segment_size + element.len() as u64);
    // The size field follows the Segment's 4-byte id.
    file.seek(SeekFrom::Start(segment_at + 4))?;
    file.write_all(&size)?;
    file.flush()?;
    file.sync()?;

    file.seek(SeekFrom::Start(segment.data))?;
    file.write_all(&seek_head(&layout, segment_end - segment.data))?;
    file.flush()?;
    file.sync()?;
    Ok(())
}

/// Writes the reserved `Void` (the muxer's header, at the Segment's first child).
pub(crate) fn reserve(out: &mut Vec<u8>) {
    ebml::void(out, RESERVED_BYTES);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn the_seek_head_fills_the_reserved_room() {
        let layout = Layout {
            info: Some(128),
            tracks: Some(200),
            first_ns: 0,
        };
        let head = seek_head(&layout, 1234);
        assert_eq!(head.len(), RESERVED_BYTES);
        assert_eq!(&head[..4], &SEEK_HEAD.to_be_bytes());
        // 12 + 3 * 35 bytes of SeekHead, then an 11-byte Void.
        assert_eq!(head[117], 0xec);
    }

    #[test]
    fn long_titles_are_cut_at_a_character_boundary() {
        let title = "é".repeat(MAX_TITLE_BYTES);
        let short = cut(&title);
        assert!(short.len() <= MAX_TITLE_BYTES);
        assert!(short.len() > MAX_TITLE_BYTES - 2);
        assert_eq!(cut("short"), "short");
    }
}
