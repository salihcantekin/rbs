//! FCP7 XML, `xmeml` version 5 (ADR-0089 decision 9, format 2).
//!
//! DaVinci Resolve reads this format's markers; it drops FCPXML's (Resolve 21.1.1,
//! tested 2026-11-15). Premiere Pro reads it too.
//!
//! Layout: one sequence at the master's rate. Video track 1 holds the master's
//! picture from the frame nearest its start offset. Every file with sound gets one
//! audio track per channel (`sourcetrack` 1, 2, ...): Resolve makes a mono track of
//! each. The master's sound tracks come first and are linked to its picture, so they
//! move together; a separate file's channels are linked to each other. Each separate
//! picture file is its own video track (2, 3, ...). Every clip sits at the frame
//! nearest its start offset, so a separate sound file can be up to half a frame from
//! its real place (FCPXML keeps the exact sample). Each `file` carries a timecode,
//! without which Resolve does not find the file on disk. Markers are sequence markers
//! in sequence frames: Resolve 21 drops clip markers of a clip it imports from disk.
//! Colours and kinds are dropped: the format has none.

use std::fmt::Write as _;

use rbs_core::timing::FrameRate;
use rbs_manifest::{FileTiming, ManifestFile, RecordingManifest};

use crate::time::{floor_frame, nearest_frame};
use crate::{MediaFolder, ProjectError};

/// The version written.
pub(crate) const VERSION: &str = "5";

fn timing(file: &ManifestFile) -> Result<&FileTiming, ProjectError> {
    file.timing
        .as_ref()
        .ok_or_else(|| ProjectError::MissingTiming(file.file_name.clone()))
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

/// `<tag>text</tag>`, escaped.
fn el(out: &mut String, tag: &str, text: &str) {
    let _ = write!(out, "<{tag}>");
    esc(out, text);
    let _ = write!(out, "</{tag}>");
}

/// xmeml's `timebase` and `ntsc` for a rate: whole rates as they are, the NTSC rates
/// (`N*1000/1001`) as `N` with `ntsc` true. Any other rate has no xmeml form.
fn timebase(rate: FrameRate) -> Result<(u64, bool), ProjectError> {
    let r = rate.per_second();
    let (num, den) = (u64::from(r.num()), u64::from(r.den()));
    if den != 0 && num % den == 0 {
        return Ok((num / den, false));
    }
    if den == 1001 && num % 1000 == 0 {
        return Ok((num / 1000, true));
    }
    Err(ProjectError::UnsupportedRate(format!("{num}/{den}")))
}

fn rate_el(out: &mut String, tb: (u64, bool)) {
    let _ = write!(
        out,
        "<rate><timebase>{}</timebase><ntsc>{}</ntsc></rate>",
        tb.0,
        if tb.1 { "TRUE" } else { "FALSE" }
    );
}

/// FCP7's file URL form: `file://localhost/...` for a local path.
fn path_url(url: String) -> String {
    match url.strip_prefix("file:///") {
        Some(rest) => format!("file://localhost/{rest}"),
        None => url,
    }
}

/// A clip on a track, in sequence frames.
struct Clip<'a> {
    file: usize,
    name: &'a str,
    at: u64,
    frames: u64,
}

pub(crate) fn write(
    manifest: &RecordingManifest,
    folder: &MediaFolder,
    title: &str,
    options: crate::Options,
) -> Result<String, ProjectError> {
    let done = manifest.completion.ok_or(ProjectError::Incomplete)?;
    let seq_picture = done.picture.ok_or(ProjectError::NoPicture)?;
    let rate = seq_picture.frame_rate;
    let tb = timebase(rate)?;
    let master_index = manifest
        .files
        .iter()
        .position(|f| f.role == "master")
        .ok_or(ProjectError::NoMaster)?;
    let master = &manifest.files[master_index];
    let master_timing = timing(master)?;
    for file in &manifest.files {
        let t = timing(file)?;
        match file.role.as_str() {
            "master" => {}
            "video" if t.picture.is_some() => {}
            "audio" if t.sound.is_some() => {}
            "video" | "audio" => return Err(ProjectError::MissingTiming(file.file_name.clone())),
            other => return Err(ProjectError::UnknownRole(other.to_owned())),
        }
    }

    let gap = nearest_frame(master_timing.start_offset_ns, rate);
    let master_frames = nearest_frame(master_timing.duration_ns, rate).max(1);
    let end = gap + master_frames;
    let master_clip = Clip {
        file: master_index,
        name: stem(&master.file_name),
        at: gap,
        frames: master_frames,
    };

    let (mut video, mut audio) = (Vec::new(), Vec::new());
    for (i, file) in manifest.files.iter().enumerate() {
        if i == master_index {
            continue;
        }
        let t = timing(file)?;
        let at = nearest_frame(t.start_offset_ns, rate);
        if at >= end {
            continue;
        }
        let frames = nearest_frame(t.duration_ns, rate).min(end - at);
        if frames == 0 {
            continue;
        }
        let clip = Clip {
            file: i,
            name: file.source_name.as_deref().unwrap_or(stem(&file.file_name)),
            at,
            frames,
        };
        if file.role == "video" {
            video.push(clip);
        } else {
            audio.push(clip);
        }
    }

    let markers = crate::project_markers(manifest, options);

    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE xmeml>\n");
    let _ = writeln!(x, "<xmeml version=\"{VERSION}\">");
    x.push_str("  <sequence id=\"sequence-1\">\n    ");
    el(&mut x, "name", title);
    let _ = write!(x, "\n    <duration>{end}</duration>\n    ");
    rate_el(&mut x, tb);
    x.push_str("\n    ");
    timecode_el(&mut x, tb);
    x.push('\n');
    // Sequence markers: Resolve 21 keeps these and drops a clip's.
    for m in &markers {
        x.push_str("    ");
        marker(&mut x, &m.label, floor_frame(m.t_ns, rate).min(end - 1));
        x.push('\n');
    }
    x.push_str("    <media>\n      <video>\n        <format><samplecharacteristics>");
    rate_el(&mut x, tb);
    let _ = writeln!(
        x,
        "<width>{}</width><height>{}</height><pixelaspectratio>square</pixelaspectratio><fielddominance>none</fielddominance></samplecharacteristics></format>",
        seq_picture.width, seq_picture.height
    );

    // Tracks and ids. The master's picture is video track 1; every file with sound
    // has one audio track per channel (Resolve makes a mono track of each), its
    // clips linked to each other and, for the master, to its picture.
    let master_sound = master_timing.sound;
    let mut next_id = 1u32;
    let mut take_id = || {
        let id = next_id;
        next_id += 1;
        id
    };
    let master_video_id = take_id();
    let mut video_ids = Vec::with_capacity(video.len());
    for _ in &video {
        video_ids.push(take_id());
    }
    let mut next_audio_track = 1u32;
    // (clip, ids of its channel clips, first audio track)
    let mut sounds: Vec<(&Clip<'_>, Vec<u32>, u32)> = Vec::new();
    for clip in std::iter::once(&master_clip)
        .filter(|_| master_sound.is_some())
        .chain(&audio)
    {
        let channels = timing(&manifest.files[clip.file])?
            .sound
            .map_or(1, |s| s.channels.max(1));
        let ids = (0..channels).map(|_| take_id()).collect();
        sounds.push((clip, ids, next_audio_track));
        next_audio_track += u32::from(channels);
    }

    let mut written = vec![false; manifest.files.len()];
    let file_el = |x: &mut String, i: usize, written: &mut Vec<bool>| {
        let file = &manifest.files[i];
        if written[i] {
            let _ = write!(x, "<file id=\"file-{}\"/>", i + 1);
            return;
        }
        written[i] = true;
        let Some(t) = file.timing.as_ref() else {
            return;
        };
        let _ = write!(x, "<file id=\"file-{}\">", i + 1);
        el(x, "name", &file.file_name);
        el(x, "pathurl", &path_url(folder.file_url(&file.file_name)));
        rate_el(x, tb);
        let _ = write!(
            x,
            "<duration>{}</duration>",
            nearest_frame(t.duration_ns, rate)
        );
        // Resolve finds the file on disk only with a timecode.
        timecode_el(x, tb);
        x.push_str("<media>");
        if let Some(p) = t.picture {
            let _ = write!(
                x,
                "<video><samplecharacteristics><width>{}</width><height>{}</height></samplecharacteristics></video>",
                p.width, p.height
            );
        }
        if let Some(s) = t.sound {
            let _ = write!(
                x,
                "<audio><samplecharacteristics><depth>16</depth><samplerate>{}</samplerate></samplecharacteristics><channelcount>{}</channelcount></audio>",
                s.sample_rate, s.channels
            );
        }
        x.push_str("</media></file>");
    };
    let clip_open = |x: &mut String, id: u32, clip: &Clip<'_>| {
        let _ = write!(x, "          <clipitem id=\"clipitem-{id}\">\n            ");
        el(x, "name", clip.name);
        x.push_str("<enabled>TRUE</enabled>");
        let _ = write!(x, "<duration>{}</duration>", clip.frames);
        rate_el(x, tb);
        let _ = write!(
            x,
            "<start>{}</start><end>{}</end><in>0</in><out>{}</out>\n            ",
            clip.at,
            clip.at + clip.frames,
            clip.frames
        );
    };
    // The links of one group: an optional picture, then each channel's clip.
    let links = |x: &mut String, video_id: Option<u32>, ids: &[u32], first_track: u32| {
        if let Some(v) = video_id {
            let _ = writeln!(
                x,
                "            <link><linkclipref>clipitem-{v}</linkclipref><mediatype>video</mediatype><trackindex>1</trackindex><clipindex>1</clipindex></link>"
            );
        }
        for (n, id) in (0u32..).zip(ids) {
            let _ = writeln!(
                x,
                "            <link><linkclipref>clipitem-{id}</linkclipref><mediatype>audio</mediatype><trackindex>{}</trackindex><clipindex>1</clipindex><groupindex>1</groupindex></link>",
                first_track + n
            );
        }
    };

    // Video track 1: the master's picture.
    x.push_str("        <track>\n");
    clip_open(&mut x, master_video_id, &master_clip);
    file_el(&mut x, master_index, &mut written);
    x.push('\n');
    if let Some((_, ids, first)) = sounds.first().filter(|_| master_sound.is_some()) {
        links(&mut x, Some(master_video_id), ids, *first);
    }
    x.push_str("          </clipitem>\n        </track>\n");
    for (clip, id) in video.iter().zip(&video_ids) {
        x.push_str("        <track>\n");
        clip_open(&mut x, *id, clip);
        file_el(&mut x, clip.file, &mut written);
        x.push_str("\n          </clipitem>\n        </track>\n");
    }
    x.push_str("      </video>\n      <audio>\n");
    let (channels, sample_rate) = master_sound.map_or((2, 48_000), |s| (s.channels, s.sample_rate));
    let _ = write!(
        x,
        "        <numOutputChannels>{channels}</numOutputChannels>\n        <format><samplecharacteristics><depth>16</depth><samplerate>{sample_rate}</samplerate></samplecharacteristics></format>\n"
    );
    for (group, (clip, ids, first)) in sounds.iter().enumerate() {
        let video_id = (group == 0 && master_sound.is_some()).then_some(master_video_id);
        for (n, id) in (1u32..).zip(ids) {
            x.push_str("        <track>\n");
            clip_open(&mut x, *id, clip);
            file_el(&mut x, clip.file, &mut written);
            let _ = writeln!(
                x,
                "<sourcetrack><mediatype>audio</mediatype><trackindex>{n}</trackindex></sourcetrack>"
            );
            if ids.len() > 1 || video_id.is_some() {
                links(&mut x, video_id, ids, *first);
            }
            x.push_str("          </clipitem>\n        </track>\n");
        }
    }
    x.push_str("      </audio>\n    </media>\n  </sequence>\n</xmeml>\n");
    Ok(x)
}

fn timecode_el(out: &mut String, tb: (u64, bool)) {
    out.push_str("<timecode>");
    rate_el(out, tb);
    out.push_str(
        "<string>00:00:00:00</string><frame>0</frame><displayformat>NDF</displayformat></timecode>",
    );
}

fn marker(out: &mut String, label: &str, frame: u64) {
    out.push_str("<marker>");
    el(out, "name", label);
    let _ = write!(
        out,
        "<comment></comment><in>{frame}</in><out>-1</out></marker>"
    );
}

fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rates_map_to_a_timebase_and_ntsc() {
        assert_eq!(timebase(FrameRate::FPS_30).unwrap(), (30, false));
        assert_eq!(timebase(FrameRate::FPS_59_94).unwrap(), (60, true));
        assert_eq!(timebase(FrameRate::FPS_29_97).unwrap(), (30, true));
        assert_eq!(
            timebase(FrameRate::new(48, 2).unwrap()).unwrap(),
            (24, false)
        );
        assert!(matches!(
            timebase(FrameRate::new(100, 3).unwrap()),
            Err(ProjectError::UnsupportedRate(_))
        ));
    }

    #[test]
    fn local_urls_take_localhost() {
        assert_eq!(
            path_url("file:///V:/RBS/a.mp4".into()),
            "file://localhost/V:/RBS/a.mp4"
        );
        assert_eq!(
            path_url("file://server/share/a.mp4".into()),
            "file://server/share/a.mp4"
        );
    }
}
