//! FCPXML 1.9 (ADR-0089 decisions 8 and 9).
//!
//! Layout: one `format` per distinct picture size and rate, one `asset` per file with
//! its URL in `media-rep`; the sequence's spine holds a `gap` when the master starts a
//! frame or more after `t0`, then the master clip. Separate picture files are connected
//! clips on lanes 1, 2, ...; separate sound files on lanes -1, -2, .... An item that
//! begins before the master's first frame hangs off the gap, every other item off the
//! master; their `offset` and `start` are in that parent's local time. Scene markers
//! are chapter markers, the others standard markers; colours are dropped.

use std::fmt::Write as _;

use rbs_core::timing::FrameRate;
use rbs_manifest::{FileTiming, ManifestFile, MarkerKind, Picture, RecordingManifest};

use crate::time::{
    ceil_frame, floor_frame, frames_text, nearest_frame, nearest_sample, samples_text,
    samples_to_frame,
};
use crate::{MediaFolder, ProjectError};

/// The version written.
pub(crate) const VERSION: &str = "1.9";

#[derive(Clone, Copy, PartialEq, Eq)]
struct FormatKey {
    width: u32,
    height: u32,
    rate: FrameRate,
}

impl From<Picture> for FormatKey {
    fn from(p: Picture) -> Self {
        Self {
            width: p.width,
            height: p.height,
            rate: p.frame_rate,
        }
    }
}

/// Something placed under a parent of the spine, in sequence frames.
enum Item<'a> {
    Clip {
        asset: usize,
        lane: i32,
        name: &'a str,
        at: u64,
        /// In-point in the asset's timebase, as text.
        start: String,
        frames: u64,
        format: Option<usize>,
    },
    Marker {
        at: u64,
        chapter: bool,
        label: &'a str,
    },
}

impl Item<'_> {
    fn at(&self) -> u64 {
        match self {
            Self::Clip { at, .. } | Self::Marker { at, .. } => *at,
        }
    }
}

fn timing(file: &ManifestFile) -> Result<&FileTiming, ProjectError> {
    file.timing
        .as_ref()
        .ok_or_else(|| ProjectError::MissingTiming(file.file_name.clone()))
}

fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

fn esc(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
}

fn attr(out: &mut String, key: &str, value: &str) {
    out.push(' ');
    out.push_str(key);
    out.push_str("=\"");
    esc(out, value);
    out.push('"');
}

fn audio_rate_name(hz: u32) -> &'static str {
    match hz {
        32_000 => "32k",
        44_100 => "44.1k",
        88_200 => "88.2k",
        96_000 => "96k",
        176_400 => "176.4k",
        192_000 => "192k",
        _ => "48k",
    }
}

fn audio_layout(channels: u16) -> &'static str {
    match channels {
        1 => "mono",
        2 => "stereo",
        _ => "surround",
    }
}

pub(crate) fn write(
    manifest: &RecordingManifest,
    folder: &MediaFolder,
    title: &str,
) -> Result<String, ProjectError> {
    let done = manifest.completion.ok_or(ProjectError::Incomplete)?;
    let seq_picture = done.picture.ok_or(ProjectError::NoPicture)?;
    let rate = seq_picture.frame_rate;
    let master_index = manifest
        .files
        .iter()
        .position(|f| f.role == "master")
        .ok_or(ProjectError::NoMaster)?;
    let master = &manifest.files[master_index];
    let master_timing = timing(master)?;

    // Formats: the sequence's first, then each other distinct picture.
    let mut formats: Vec<FormatKey> = vec![seq_picture.into()];
    let mut asset_format = Vec::with_capacity(manifest.files.len());
    for file in &manifest.files {
        let t = timing(file)?;
        match file.role.as_str() {
            "master" | "video" | "audio" => {}
            other => return Err(ProjectError::UnknownRole(other.to_owned())),
        }
        if file.role == "video" && t.picture.is_none() {
            return Err(ProjectError::MissingTiming(file.file_name.clone()));
        }
        if file.role == "audio" && t.sound.is_none() {
            return Err(ProjectError::MissingTiming(file.file_name.clone()));
        }
        asset_format.push(t.picture.map(|p| {
            let key = FormatKey::from(p);
            match formats.iter().position(|k| *k == key) {
                Some(i) => i,
                None => {
                    formats.push(key);
                    formats.len() - 1
                }
            }
        }));
    }
    let format_id = |i: usize| format!("r{}", i + 1);
    let asset_id = |i: usize| format!("r{}", formats.len() + i + 1);

    // The storyline.
    let gap = nearest_frame(master_timing.start_offset_ns, rate);
    let master_frames = nearest_frame(master_timing.duration_ns, rate).max(1);
    let end = gap + master_frames;

    let mut items = Vec::new();
    let (mut video_lane, mut audio_lane) = (0i32, 0i32);
    for (i, file) in manifest.files.iter().enumerate() {
        if i == master_index {
            continue;
        }
        let t = timing(file)?;
        let name = file.source_name.as_deref().unwrap_or(stem(&file.file_name));
        let (lane, at, start, frames) = if file.role == "video" {
            video_lane += 1;
            let at = nearest_frame(t.start_offset_ns, rate);
            let frames = nearest_frame(t.duration_ns, rate);
            (video_lane, at, "0s".to_owned(), frames)
        } else {
            audio_lane -= 1;
            let Some(sound) = t.sound else { continue };
            let at = ceil_frame(t.start_offset_ns, rate);
            let in_point = samples_to_frame(t.start_offset_ns, at, rate, sound.sample_rate);
            let last = floor_frame(t.start_offset_ns.saturating_add(t.duration_ns), rate);
            (
                audio_lane,
                at,
                samples_text(in_point, sound.sample_rate),
                last.saturating_sub(at),
            )
        };
        if at >= end {
            continue;
        }
        let frames = frames.min(end - at);
        if frames == 0 {
            continue;
        }
        items.push(Item::Clip {
            asset: i,
            lane,
            name,
            at,
            start,
            frames,
            format: asset_format[i],
        });
    }
    let mut markers: Vec<_> = manifest.markers.iter().collect();
    markers.sort_by_key(|m| (m.t_ns, m.index));
    for m in markers {
        items.push(Item::Marker {
            at: floor_frame(m.t_ns, rate).min(end - 1),
            chapter: m.kind == MarkerKind::Scene,
            label: &m.label,
        });
    }

    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE fcpxml>\n\n");
    let _ = writeln!(x, "<fcpxml version=\"{VERSION}\">");
    x.push_str("    <resources>\n");
    for (i, f) in formats.iter().enumerate() {
        x.push_str("        <format");
        attr(&mut x, "id", &format_id(i));
        attr(&mut x, "frameDuration", &frames_text(1, f.rate));
        attr(&mut x, "width", &f.width.to_string());
        attr(&mut x, "height", &f.height.to_string());
        x.push_str("/>\n");
    }
    for (i, file) in manifest.files.iter().enumerate() {
        let t = timing(file)?;
        x.push_str("        <asset");
        attr(&mut x, "id", &asset_id(i));
        attr(&mut x, "name", stem(&file.file_name));
        attr(&mut x, "start", "0s");
        let duration = match (t.picture, t.sound) {
            (Some(p), _) => frames_text(nearest_frame(t.duration_ns, p.frame_rate), p.frame_rate),
            (None, Some(s)) => {
                samples_text(nearest_sample(t.duration_ns, s.sample_rate), s.sample_rate)
            }
            (None, None) => return Err(ProjectError::MissingTiming(file.file_name.clone())),
        };
        attr(&mut x, "duration", &duration);
        if let Some(f) = asset_format[i] {
            attr(&mut x, "hasVideo", "1");
            attr(&mut x, "format", &format_id(f));
        }
        if let Some(s) = t.sound {
            attr(&mut x, "hasAudio", "1");
            attr(&mut x, "audioSources", "1");
            attr(&mut x, "audioChannels", &s.channels.to_string());
            attr(&mut x, "audioRate", &s.sample_rate.to_string());
        }
        x.push_str(">\n            <media-rep");
        attr(&mut x, "kind", "original-media");
        attr(&mut x, "src", &folder.file_url(&file.file_name));
        x.push_str("/>\n        </asset>\n");
    }
    x.push_str("    </resources>\n    <library>\n        <event");
    attr(&mut x, "name", title);
    x.push_str(">\n            <project");
    attr(&mut x, "name", title);
    x.push_str(">\n                <sequence");
    attr(&mut x, "format", &format_id(0));
    attr(&mut x, "duration", &frames_text(end, rate));
    attr(&mut x, "tcStart", "0s");
    attr(&mut x, "tcFormat", "NDF");
    let master_sound = master_timing.sound;
    attr(
        &mut x,
        "audioLayout",
        audio_layout(master_sound.map_or(2, |s| s.channels)),
    );
    attr(
        &mut x,
        "audioRate",
        audio_rate_name(master_sound.map_or(48_000, |s| s.sample_rate)),
    );
    x.push_str(">\n                    <spine>\n");

    const PAD: &str = "                            ";
    let children = |x: &mut String, from: u64, before: bool| {
        let mine = |item: &&Item<'_>| (item.at() < gap) == before;
        // Clips first, then markers (the DTD's order).
        for item in items.iter().filter(mine) {
            if let Item::Clip {
                asset,
                lane,
                name,
                at,
                start,
                frames,
                format,
            } = item
            {
                x.push_str(PAD);
                x.push_str("<asset-clip");
                attr(x, "ref", &asset_id(*asset));
                attr(x, "lane", &lane.to_string());
                attr(x, "offset", &frames_text(at - from, rate));
                attr(x, "name", name);
                attr(x, "start", start);
                attr(x, "duration", &frames_text(*frames, rate));
                if let Some(f) = format {
                    attr(x, "format", &format_id(*f));
                }
                attr(x, "tcFormat", "NDF");
                x.push_str("/>\n");
            }
        }
        for item in items.iter().filter(mine) {
            if let Item::Marker { at, chapter, label } = item {
                x.push_str(PAD);
                x.push_str(if *chapter {
                    "<chapter-marker"
                } else {
                    "<marker"
                });
                attr(x, "start", &frames_text(at - from, rate));
                attr(x, "duration", &frames_text(1, rate));
                attr(x, "value", label);
                if *chapter {
                    attr(x, "posterOffset", "0s");
                }
                x.push_str("/>\n");
            }
        }
    };
    if gap > 0 {
        x.push_str("                        <gap");
        attr(&mut x, "name", "Gap");
        attr(&mut x, "offset", "0s");
        attr(&mut x, "start", "0s");
        attr(&mut x, "duration", &frames_text(gap, rate));
        x.push_str(">\n");
        children(&mut x, 0, true);
        x.push_str("                        </gap>\n");
    }
    x.push_str("                        <asset-clip");
    attr(&mut x, "ref", &asset_id(master_index));
    attr(&mut x, "offset", &frames_text(gap, rate));
    attr(&mut x, "name", stem(&master.file_name));
    attr(&mut x, "start", "0s");
    attr(&mut x, "duration", &frames_text(master_frames, rate));
    if let Some(f) = asset_format[master_index] {
        attr(&mut x, "format", &format_id(f));
    }
    attr(&mut x, "tcFormat", "NDF");
    x.push_str(">\n");
    children(&mut x, gap, false);
    x.push_str("                        </asset-clip>\n");
    x.push_str(
        "                    </spine>\n                </sequence>\n            </project>\n        </event>\n    </library>\n</fcpxml>\n",
    );
    Ok(x)
}
