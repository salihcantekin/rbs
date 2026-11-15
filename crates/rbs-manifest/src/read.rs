//! Reading a manifest's TOML text.

use std::fmt;

use rbs_core::timing::FrameRate;
use toml::{Table, Value};

use crate::{
    Completion, FORMAT, FileTiming, ImageCounts, ImageEntry, KIND, ManifestFile, Marker,
    MarkerColor, MarkerKind, Pause, Picture, RecordingManifest, SkippedEntry, Sound, SourceRef,
    checked_label,
};

/// Why a manifest could not be read. Messages name keys, never paths.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadError {
    /// The text is not TOML.
    Syntax,
    /// `kind` is not `recording-manifest`.
    Kind,
    /// `format` is higher than this build reads (or below 1).
    Format(i64),
    /// A required key is missing or has the wrong type or value.
    Field(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax => f.write_str("the recording manifest is not valid TOML"),
            Self::Kind => f.write_str("the file is not a recording manifest"),
            Self::Format(n) => write!(f, "recording manifest format {n} cannot be read"),
            Self::Field(key) => write!(f, "the recording manifest has a bad `{key}`"),
        }
    }
}

impl std::error::Error for ReadError {}

fn bad(key: &str) -> ReadError {
    ReadError::Field(key.to_owned())
}

fn int(t: &Table, key: &str) -> Result<Option<i64>, ReadError> {
    match t.get(key) {
        None => Ok(None),
        Some(Value::Integer(n)) => Ok(Some(*n)),
        Some(_) => Err(bad(key)),
    }
}

fn u64_of(t: &Table, key: &str) -> Result<Option<u64>, ReadError> {
    int(t, key)?
        .map(|n| u64::try_from(n).map_err(|_| bad(key)))
        .transpose()
}

fn u32_of(t: &Table, key: &str) -> Result<Option<u32>, ReadError> {
    int(t, key)?
        .map(|n| u32::try_from(n).map_err(|_| bad(key)))
        .transpose()
}

fn need<T>(value: Option<T>, key: &str) -> Result<T, ReadError> {
    value.ok_or_else(|| bad(key))
}

fn string(t: &Table, key: &str) -> Result<Option<String>, ReadError> {
    match t.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(bad(key)),
    }
}

fn tables<'a>(t: &'a Table, key: &str) -> Result<Vec<&'a Table>, ReadError> {
    match t.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_table().ok_or_else(|| bad(key)))
            .collect(),
        Some(_) => Err(bad(key)),
    }
}

/// `frame_duration = "den/num"` seconds per frame, back to a frame rate.
fn picture(t: &Table) -> Result<Option<Picture>, ReadError> {
    let Some(duration) = string(t, "frame_duration")? else {
        if t.contains_key("width") || t.contains_key("height") {
            return Err(bad("frame_duration"));
        }
        return Ok(None);
    };
    let (num, den) = duration
        .split_once('/')
        .ok_or_else(|| bad("frame_duration"))?;
    let num: u32 = num.trim().parse().map_err(|_| bad("frame_duration"))?;
    let den: u32 = den.trim().parse().map_err(|_| bad("frame_duration"))?;
    // A frame of num/den seconds is den/num frames per second.
    let frame_rate = FrameRate::new(den, num).ok_or_else(|| bad("frame_duration"))?;
    let width = need(u32_of(t, "width")?, "width")?;
    let height = need(u32_of(t, "height")?, "height")?;
    if width == 0 || height == 0 {
        return Err(bad(if width == 0 { "width" } else { "height" }));
    }
    Ok(Some(Picture {
        width,
        height,
        frame_rate,
    }))
}

fn sound(t: &Table) -> Result<Option<Sound>, ReadError> {
    let Some(sample_rate) = u32_of(t, "sample_rate")? else {
        return Ok(None);
    };
    let channels = need(u32_of(t, "channels")?, "channels")?;
    let channels = u16::try_from(channels).map_err(|_| bad("channels"))?;
    if sample_rate == 0 || channels == 0 {
        return Err(bad("sample_rate"));
    }
    Ok(Some(Sound {
        sample_rate,
        channels,
    }))
}

fn file(t: &Table) -> Result<ManifestFile, ReadError> {
    let role = need(string(t, "role")?, "role")?;
    let file_name = need(string(t, "name")?, "name")?;
    if !crate::single_file_name(&file_name) {
        return Err(bad("name"));
    }
    let timing = match u64_of(t, "start_offset_ns")? {
        None => None,
        Some(start_offset_ns) => Some(FileTiming {
            start_offset_ns,
            duration_ns: need(u64_of(t, "duration_ns")?, "duration_ns")?,
            picture: picture(t)?,
            sound: sound(t)?,
        }),
    };
    let image_counts = match u64_of(t, "coalesced")? {
        None => None,
        Some(coalesced) => Some(ImageCounts {
            coalesced,
            over_cap: need(u64_of(t, "over_cap")?, "over_cap")?,
        }),
    };
    Ok(ManifestFile {
        role,
        file_name,
        source: u32_of(t, "source")?.map(SourceRef::new),
        source_name: string(t, "source_name")?,
        timing,
        images: tables(t, "images")?
            .into_iter()
            .map(image)
            .collect::<Result<_, _>>()?,
        image_counts,
    })
}

fn image(t: &Table) -> Result<ImageEntry, ReadError> {
    let file_name = need(string(t, "name")?, "name")?;
    if !crate::single_file_name(&file_name) {
        return Err(bad("name"));
    }
    let width = need(u32_of(t, "width")?, "width")?;
    let height = need(u32_of(t, "height")?, "height")?;
    if width == 0 || height == 0 {
        return Err(bad(if width == 0 { "width" } else { "height" }));
    }
    Ok(ImageEntry {
        file_name,
        width,
        height,
        at_ns: need(u64_of(t, "at_ns")?, "at_ns")?,
    })
}

fn marker(t: &Table) -> Result<Marker, ReadError> {
    let kind = need(string(t, "kind")?, "kind")?;
    let color = string(t, "color")?;
    let label = need(string(t, "label")?, "label")?;
    Ok(Marker {
        index: need(u32_of(t, "index")?, "index")?,
        t_ns: need(u64_of(t, "t_ns")?, "t_ns")?,
        pressed_ns: u64_of(t, "pressed_ns")?,
        kind: MarkerKind::parse(&kind).ok_or_else(|| bad("kind"))?,
        label: checked_label(Some(&label))
            .map_err(|_| bad("label"))?
            .unwrap_or_default(),
        color: match color {
            None => MarkerColor::default(),
            Some(c) => MarkerColor::parse(&c).ok_or_else(|| bad("color"))?,
        },
        while_paused: match t.get("while_paused") {
            None | Some(Value::Boolean(false)) => false,
            Some(Value::Boolean(true)) => true,
            Some(_) => return Err(bad("while_paused")),
        },
    })
}

fn pause(t: &Table) -> Result<Pause, ReadError> {
    let from_ns = need(u64_of(t, "from_ns")?, "from_ns")?;
    let to_ns = need(u64_of(t, "to_ns")?, "to_ns")?;
    if to_ns < from_ns {
        return Err(bad("to_ns"));
    }
    Ok(Pause {
        at_file_ns: need(u64_of(t, "at_file_ns")?, "at_file_ns")?,
        from_ns,
        to_ns,
        join_error_ns: u64_of(t, "join_error_ns")?,
    })
}

pub(crate) fn parse(text: &str) -> Result<RecordingManifest, ReadError> {
    let t: Table = text.parse().map_err(|_| ReadError::Syntax)?;
    if string(&t, "kind")?.as_deref() != Some(KIND) {
        return Err(ReadError::Kind);
    }
    let format = need(int(&t, "format")?, "format")?;
    if format < 1 || format > i64::from(FORMAT) {
        return Err(ReadError::Format(format));
    }
    let completion = match t.get("complete") {
        None | Some(Value::Boolean(false)) => None,
        Some(Value::Boolean(true)) => Some(Completion {
            duration_ns: need(u64_of(&t, "duration_ns")?, "duration_ns")?,
            picture: picture(&t)?,
        }),
        Some(_) => return Err(bad("complete")),
    };
    Ok(RecordingManifest {
        t0_utc: need(string(&t, "t0_utc")?, "t0_utc")?,
        t0_ns: need(u64_of(&t, "t0_ns")?, "t0_ns")?,
        files: tables(&t, "files")?
            .into_iter()
            .map(file)
            .collect::<Result<_, _>>()?,
        markers: tables(&t, "markers")?
            .into_iter()
            .map(marker)
            .collect::<Result<_, _>>()?,
        completion,
        skipped: tables(&t, "skipped")?
            .into_iter()
            .map(skipped)
            .collect::<Result<_, _>>()?,
        pauses: tables(&t, "pauses")?
            .into_iter()
            .map(pause)
            .collect::<Result<_, _>>()?,
    })
}

fn skipped(t: &Table) -> Result<SkippedEntry, ReadError> {
    Ok(SkippedEntry {
        source: SourceRef::new(need(u32_of(t, "source")?, "source")?),
        source_name: string(t, "source_name")?,
        role: need(string(t, "role")?, "role")?,
        reason: need(string(t, "reason")?, "reason")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const HEAD: &str = "format = 1\nkind = \"recording-manifest\"\nt0_utc = \"t\"\nt0_ns = 1\n";

    #[test]
    fn other_kinds_formats_and_bad_fields_are_refused() {
        assert_eq!(parse("not toml ="), Err(ReadError::Syntax));
        assert_eq!(
            parse("format = 1\nkind = \"theme\"\n"),
            Err(ReadError::Kind)
        );
        assert_eq!(
            parse("format = 2\nkind = \"recording-manifest\"\nt0_utc = \"t\"\nt0_ns = 1\n"),
            Err(ReadError::Format(2))
        );
        assert_eq!(
            parse(&format!("{HEAD}complete = true\n")),
            Err(bad("duration_ns"))
        );
        assert_eq!(
            parse(&format!(
                "{HEAD}\n[[files]]\nrole = \"master\"\nname = \"../x.mkv\"\n"
            )),
            Err(bad("name"))
        );
        assert_eq!(
            parse(&format!(
                "{HEAD}\n[[markers]]\nindex = 1\nt_ns = 0\nkind = \"manual\"\nlabel = \"a\"\ncolor = \"pink\"\n"
            )),
            Err(bad("color"))
        );
        assert_eq!(
            parse(&format!(
                "{HEAD}complete = true\nduration_ns = 1\nframe_duration = \"0/25\"\nwidth = 1\nheight = 1\n"
            )),
            Err(bad("frame_duration"))
        );
    }

    /// ADR-0124 decision 10: `paused_ns`, `[[pauses]]` and `while_paused` round-trip;
    /// a pause ending before it begins is refused.
    #[test]
    fn pauses_round_trip() {
        let mut m = RecordingManifest::new("t", 1);
        m.pauses = vec![
            Pause {
                at_file_ns: 10,
                from_ns: 10,
                to_ns: 15,
                join_error_ns: Some(3),
            },
            Pause {
                at_file_ns: 15,
                from_ns: 20,
                to_ns: 30,
                join_error_ns: None,
            },
        ];
        m.markers.push(Marker {
            index: 1,
            t_ns: 15,
            pressed_ns: Some(15),
            kind: MarkerKind::Manual,
            label: "Marker 1".into(),
            color: MarkerColor::Blue,
            while_paused: true,
        });
        let text = m.to_toml();
        assert!(text.contains("paused_ns = 15\n"), "{text}");
        assert_eq!(parse(&text).unwrap(), m);
        assert_eq!(
            parse(&format!(
                "{HEAD}\n[[pauses]]\nat_file_ns = 1\nfrom_ns = 5\nto_ns = 4\n"
            )),
            Err(bad("to_ns"))
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let m = parse(&format!(
            "{HEAD}later = \"field\"\n\n[later_table]\nx = 1\n"
        ))
        .unwrap();
        assert_eq!(m.t0_ns, 1);
        assert!(m.files.is_empty());
    }
}
