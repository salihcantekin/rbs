//! The sound track's edit list of an MP4 or QuickTime file (ISO/IEC 14496-12
//! `elst`), read without decoding anything.
//!
//! Media Foundation's MPEG-4 source stamps the first decoded AAC sample at 0, including
//! the encoder priming the edit list skips (1024 samples for a usual AAC encoder, about
//! 21 ms at 48 kHz), and keeps the end padding after the edit's length. AVFoundation
//! applies the edit list itself. So this adapter reads the list and trims the sound to
//! it, and the sound of a file lines up with its pictures on both platforms.
//!
//! Only the sound track's first edit is applied: a leading empty edit is a delay, and
//! the first edit with media is where the sound starts and how long it plays. A list
//! with more media edits (rare outside editing tools) keeps only its first.

use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

/// The largest `moov` box read. A two-hour file's sample tables are a few MiB.
const MAX_MOOV: u64 = 64 << 20;

/// What the sound track's edit list asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SoundEdit {
    /// Media time (the decoder's stamps) of the first sample to play.
    pub(crate) skip: Duration,
    /// Presentation time that sample plays at: an empty edit before it.
    pub(crate) delay: Duration,
    /// How long the edit plays; `None` when the list says to the end of the media.
    pub(crate) length: Option<Duration>,
}

impl SoundEdit {
    /// Presentation time of media time `media`, in nanoseconds (negative before the
    /// edit starts).
    pub(crate) fn presentation(&self, media: Duration) -> i128 {
        media.as_nanos() as i128 - self.skip.as_nanos() as i128 + self.delay.as_nanos() as i128
    }

    /// Presentation time the sound ends at, when the edit states its length.
    pub(crate) fn end(&self) -> Option<Duration> {
        self.length.map(|length| self.delay.saturating_add(length))
    }

    /// Whether the edit changes nothing.
    pub(crate) fn is_identity(&self) -> bool {
        self.skip.is_zero() && self.delay.is_zero() && self.length.is_none()
    }
}

/// What to do with one decoded sound chunk after the edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trimmed {
    /// Play the chunk (now trimmed) from this presentation time.
    Play(Duration),
    /// Every sample is before the edit starts: read the next chunk.
    Skip,
    /// The chunk starts at or after the edit's end: the sound is over.
    End,
}

/// Trims one interleaved chunk stamped at media time `pts` to the edit: drops the
/// frames before its start (the priming) and after its end (the padding), rounding
/// to whole frames, and gives the chunk's presentation time.
pub(crate) fn trim(
    edit: &SoundEdit,
    pts: Duration,
    rate: u32,
    channels: u16,
    samples: &mut Vec<f32>,
) -> Trimmed {
    let channels = usize::from(channels.max(1));
    let rate = i128::from(rate.max(1));
    let mut start = edit.presentation(pts);
    let frames = (samples.len() / channels) as i128;
    if let Some(end) = edit.end() {
        let end = end.as_nanos() as i128;
        if start >= end {
            return Trimmed::End;
        }
        let keep = ((end - start) * rate / 1_000_000_000).min(frames);
        samples.truncate(usize::try_from(keep).unwrap_or(0) * channels);
    }
    if start < 0 {
        let drop = (-start * rate + 999_999_999) / 1_000_000_000;
        let have = (samples.len() / channels) as i128;
        if drop >= have {
            return Trimmed::Skip;
        }
        samples.drain(..usize::try_from(drop).unwrap_or(0) * channels);
        start += drop * 1_000_000_000 / rate;
    }
    if samples.is_empty() {
        return Trimmed::End;
    }
    Trimmed::Play(Duration::from_nanos(
        u64::try_from(start.max(0)).unwrap_or(0),
    ))
}

/// The sound track's edit, or `None` for a file that is no MP4 or QuickTime file,
/// has no sound track, or has no edit list. Reads only the box headers before `moov`
/// and `moov` itself.
pub(crate) fn sound_edit(file: &mut (impl Read + Seek)) -> Option<SoundEdit> {
    let moov = find_moov(file)?;
    let mut movie_scale = None;
    let mut found = None;
    for (kind, body) in Boxes(&moov) {
        match &kind {
            b"mvhd" => movie_scale = timescale(body),
            b"trak" if found.is_none() => found = sound_track(body),
            _ => {}
        }
    }
    let (track_scale, entries) = found?;
    let movie_scale = movie_scale?;
    let seconds = |value: u64, scale: u32| {
        let nanos = u128::from(value) * 1_000_000_000 / u128::from(scale.max(1));
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    };
    let mut delay = Duration::ZERO;
    for (segment, media_time) in entries {
        if media_time < 0 {
            // An empty edit: nothing plays for its length.
            delay = delay.saturating_add(seconds(segment, movie_scale));
            continue;
        }
        let skip = seconds(media_time.unsigned_abs(), track_scale);
        // A length of 0 means "the rest of the media" in a fragmented file.
        let length = (segment != 0).then(|| seconds(segment, movie_scale));
        return Some(SoundEdit {
            skip,
            delay,
            length,
        });
    }
    None
}

/// Reads the `moov` box body: walks the top-level boxes by their headers.
fn find_moov(file: &mut (impl Read + Seek)) -> Option<Vec<u8>> {
    let mut at = 0u64;
    file.seek(SeekFrom::Start(0)).ok()?;
    // The first box of an MP4 or QuickTime file is `ftyp` (or, in old QuickTime
    // files, `moov`, `mdat`, `wide` or `free`); anything else is no such file.
    let mut first = true;
    loop {
        let mut header = [0u8; 8];
        file.seek(SeekFrom::Start(at)).ok()?;
        file.read_exact(&mut header).ok()?;
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let kind = [header[4], header[5], header[6], header[7]];
        if first && !matches!(&kind, b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free") {
            return None;
        }
        first = false;
        let (size, header_len) = match size32 {
            1 => {
                let mut large = [0u8; 8];
                file.read_exact(&mut large).ok()?;
                (u64::from_be_bytes(large), 16)
            }
            // To the end of the file: the last box.
            0 => {
                let end = file.seek(SeekFrom::End(0)).ok()?;
                (end.checked_sub(at)?, 8)
            }
            n => (u64::from(n), 8),
        };
        if size < header_len {
            return None;
        }
        if &kind == b"moov" {
            let body = size - header_len;
            if body > MAX_MOOV {
                return None;
            }
            file.seek(SeekFrom::Start(at + header_len)).ok()?;
            let mut moov = vec![0u8; usize::try_from(body).ok()?];
            file.read_exact(&mut moov).ok()?;
            return Some(moov);
        }
        at = at.checked_add(size)?;
    }
}

/// The boxes inside a body: `(type, body)`. Stops at the first malformed header.
struct Boxes<'a>(&'a [u8]);

impl<'a> Iterator for Boxes<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let data = self.0;
        let size32 = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?);
        let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
        let (size, header) = match size32 {
            1 => (
                usize::try_from(u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)).ok()?,
                16,
            ),
            0 => (data.len(), 8),
            n => (usize::try_from(n).ok()?, 8),
        };
        if size < header || size > data.len() {
            self.0 = &[];
            return None;
        }
        self.0 = &data[size..];
        Some((kind, &data[header..size]))
    }
}

/// The track's timescale and edit list entries `(segment duration, media time)` when
/// it is a sound track with an edit list.
fn sound_track(trak: &[u8]) -> Option<(u32, Vec<(u64, i64)>)> {
    let mut entries = None;
    let mut mdia = None;
    for (kind, body) in Boxes(trak) {
        match &kind {
            b"edts" => {
                entries = Boxes(body)
                    .find(|(kind, _)| kind == b"elst")
                    .and_then(|(_, elst)| edit_list(elst));
            }
            b"mdia" => mdia = Some(body),
            _ => {}
        }
    }
    let mdia = mdia?;
    let mut scale = None;
    let mut sound = false;
    for (kind, body) in Boxes(mdia) {
        match &kind {
            b"mdhd" => scale = timescale(body),
            // Full box header (4), pre_defined (4), then the handler type.
            b"hdlr" => sound = body.get(8..12) == Some(b"soun"),
            _ => {}
        }
    }
    if !sound {
        return None;
    }
    Some((scale?, entries?))
}

/// The timescale of an `mvhd` or `mdhd` body (version 0 or 1).
fn timescale(body: &[u8]) -> Option<u32> {
    let at = match body.first()? {
        0 => 12,
        1 => 20,
        _ => return None,
    };
    let scale = u32::from_be_bytes(body.get(at..at + 4)?.try_into().ok()?);
    (scale != 0).then_some(scale)
}

/// The entries of an `elst` body.
fn edit_list(body: &[u8]) -> Option<Vec<(u64, i64)>> {
    let version = *body.first()?;
    let count = u32::from_be_bytes(body.get(4..8)?.try_into().ok()?);
    let entry = match version {
        0 => 12,
        1 => 20,
        _ => return None,
    };
    let mut entries = Vec::new();
    let mut at = 8usize;
    for _ in 0..count.min(1024) {
        let raw = body.get(at..at + entry)?;
        let (segment, media_time) = if version == 1 {
            (
                u64::from_be_bytes(raw[0..8].try_into().ok()?),
                i64::from_be_bytes(raw[8..16].try_into().ok()?),
            )
        } else {
            (
                u64::from(u32::from_be_bytes(raw[0..4].try_into().ok()?)),
                i64::from(i32::from_be_bytes(raw[4..8].try_into().ok()?)),
            )
        };
        entries.push((segment, media_time));
        at += entry;
    }
    Some(entries)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn header_v0(scale: u32) -> Vec<u8> {
        let mut body = vec![0u8; 12];
        body.extend_from_slice(&scale.to_be_bytes());
        body.extend_from_slice(&[0u8; 8]);
        body
    }

    fn track(handler: &[u8; 4], scale: u32, edits: &[(u32, i32)]) -> Vec<u8> {
        let mut elst = vec![0u8, 0, 0, 0];
        elst.extend_from_slice(&u32::try_from(edits.len()).unwrap().to_be_bytes());
        for (segment, media) in edits {
            elst.extend_from_slice(&segment.to_be_bytes());
            elst.extend_from_slice(&media.to_be_bytes());
            elst.extend_from_slice(&[0, 1, 0, 0]);
        }
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0u8; 13]);
        let mdia = [boxed(b"mdhd", &header_v0(scale)), boxed(b"hdlr", &hdlr)].concat();
        let edts = boxed(b"elst", &elst);
        let body = [boxed(b"edts", &edts), boxed(b"mdia", &mdia)].concat();
        boxed(b"trak", &body)
    }

    fn file(tracks: &[Vec<u8>], moov_last: bool) -> Vec<u8> {
        let moov = boxed(
            b"moov",
            &[boxed(b"mvhd", &header_v0(1000)), tracks.concat()].concat(),
        );
        let mdat = boxed(b"mdat", &[0u8; 32]);
        let ftyp = boxed(b"ftyp", b"isom\0\0\0\0");
        if moov_last {
            [ftyp, mdat, moov].concat()
        } else {
            [ftyp, moov, mdat].concat()
        }
    }

    #[test]
    fn aac_priming_and_length_are_read_from_the_sound_track() {
        for moov_last in [false, true] {
            let bytes = file(
                &[
                    track(b"vide", 10_240, &[(500, 0)]),
                    track(b"soun", 48_000, &[(500, 1024)]),
                ],
                moov_last,
            );
            let edit = sound_edit(&mut Cursor::new(bytes)).unwrap();
            assert_eq!(edit.skip, Duration::from_nanos(21_333_333));
            assert_eq!(edit.delay, Duration::ZERO);
            assert_eq!(edit.end(), Some(Duration::from_millis(500)));
            assert_eq!(edit.presentation(Duration::ZERO), -21_333_333);
        }
    }

    #[test]
    fn a_leading_empty_edit_delays_the_sound() {
        let bytes = file(&[track(b"soun", 44_100, &[(100, -1), (1_000, 0)])], false);
        let edit = sound_edit(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(edit.delay, Duration::from_millis(100));
        assert_eq!(edit.skip, Duration::ZERO);
        assert_eq!(edit.end(), Some(Duration::from_millis(1_100)));
    }

    #[test]
    fn no_sound_track_no_edit_list_or_no_mp4_gives_none() {
        let video_only = file(&[track(b"vide", 90_000, &[(500, 0)])], false);
        assert_eq!(sound_edit(&mut Cursor::new(video_only)), None);
        assert_eq!(
            sound_edit(&mut Cursor::new(b"RIFF\0\0\0\0AVI ".to_vec())),
            None
        );
        assert_eq!(sound_edit(&mut Cursor::new(Vec::new())), None);
        // A truncated box never reads past the data.
        let mut cut = file(&[track(b"soun", 48_000, &[(500, 1024)])], false);
        cut.truncate(60);
        assert_eq!(sound_edit(&mut Cursor::new(cut)), None);
    }

    #[test]
    fn priming_and_padding_are_cut_to_whole_frames() {
        let edit = SoundEdit {
            skip: Duration::from_nanos(21_333_333),
            delay: Duration::ZERO,
            length: Some(Duration::from_millis(500)),
        };
        // 2048 stereo frames at 48 kHz from media 0: the first 1024 are priming.
        let mut samples: Vec<f32> = (0..4096).map(|i| (i / 2) as f32).collect();
        assert_eq!(
            trim(&edit, Duration::ZERO, 48_000, 2, &mut samples),
            Trimmed::Play(Duration::ZERO)
        );
        assert_eq!(samples.len(), 2048);
        assert_eq!(samples[0], 1024.0);
        // All priming: skipped.
        let mut early = vec![0.0f32; 1000 * 2];
        assert_eq!(
            trim(&edit, Duration::ZERO, 48_000, 2, &mut early),
            Trimmed::Skip
        );
        // The chunk that holds the end keeps the frames before it (500 ms = 24000).
        let at = Duration::from_nanos(21_333_333 + 23_000 * 20_833);
        let mut last = vec![0.0f32; 2048 * 2];
        let Trimmed::Play(start) = trim(&edit, at, 48_000, 2, &mut last) else {
            panic!("plays");
        };
        let kept = last.len() as u128 / 2;
        let end = start.as_nanos() + kept * 1_000_000_000 / 48_000;
        assert!(end <= 500_000_000 && end > 500_000_000 - 20_834, "{end}");
        // After the end: over.
        let mut after = vec![0.0f32; 8];
        let past = Duration::from_millis(530);
        assert_eq!(trim(&edit, past, 48_000, 2, &mut after), Trimmed::End);
    }

    #[test]
    fn the_fixture_skips_1024_samples_and_plays_half_a_second() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/clip.mp4");
        let mut fixture = std::fs::File::open(path).unwrap();
        let edit = sound_edit(&mut fixture).unwrap();
        assert_eq!(edit.skip, Duration::from_nanos(21_333_333));
        assert_eq!(edit.end(), Some(Duration::from_millis(500)));
    }
}
