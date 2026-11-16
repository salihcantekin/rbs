//! A small reader for OBS's `basic.ini` (UTF-8, optional BOM, `[Section]` and
//! `key=value` lines), and the profile facts read from it.

use std::collections::BTreeMap;

/// Sections by name, each its keys and values. Later duplicates win, as in OBS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ini {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl Ini {
    /// Parses `text`. Lines that are neither a section, a key nor a comment are ignored.
    pub fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut current = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                current = name.trim().to_owned();
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                sections
                    .entry(current.clone())
                    .or_default()
                    .insert(k.trim().to_owned(), v.trim().to_owned());
            }
        }
        Self { sections }
    }

    /// The value of `key` in `section`.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(section)?.get(key).map(String::as_str)
    }

    /// The value parsed as `T`.
    pub fn parse_as<T: std::str::FromStr>(&self, section: &str, key: &str) -> Option<T> {
        self.get(section, key)?.trim().parse().ok()
    }
}

/// The video facts of an OBS profile (`[Video]`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFacts {
    /// `BaseCX`, `BaseCY`: the canvas.
    pub base: Option<(u32, u32)>,
    /// `OutputCX`, `OutputCY`: the scaled output.
    pub output: Option<(u32, u32)>,
    /// Frame rate as `num/den`.
    pub fps: Option<(u32, u32)>,
}

impl VideoFacts {
    /// Reads `[Video]`.
    pub fn read(ini: &Ini) -> Self {
        let pair = |a: &str, b: &str| {
            Some((
                ini.parse_as::<u32>("Video", a)?,
                ini.parse_as::<u32>("Video", b)?,
            ))
        };
        let fps = match ini.parse_as::<u32>("Video", "FPSType").unwrap_or(0) {
            1 => ini.parse_as::<u32>("Video", "FPSInt").map(|n| (n, 1)),
            2 => pair("FPSNum", "FPSDen"),
            _ => common_fps(ini.get("Video", "FPSCommon").unwrap_or("30")),
        };
        Self {
            base: pair("BaseCX", "BaseCY"),
            output: pair("OutputCX", "OutputCY"),
            fps,
        }
    }
}

/// OBS's `FPSCommon` choices.
fn common_fps(text: &str) -> Option<(u32, u32)> {
    match text.trim() {
        "23.976" | "24 NTSC" => Some((24_000, 1_001)),
        "29.97" => Some((30_000, 1_001)),
        "47.95" | "48 NTSC" => Some((48_000, 1_001)),
        "59.94" => Some((60_000, 1_001)),
        t => t.parse::<u32>().ok().map(|n| (n, 1)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn video_facts_follow_the_fps_type() {
        let ini = Ini::parse(
            "\u{feff}[General]\nName=Main\n\n[Video]\nBaseCX=2560\nBaseCY=1440\nOutputCX=1920\nOutputCY=1080\nFPSType=0\nFPSCommon=59.94\n",
        );
        let v = VideoFacts::read(&ini);
        assert_eq!(v.base, Some((2560, 1440)));
        assert_eq!(v.output, Some((1920, 1080)));
        assert_eq!(v.fps, Some((60_000, 1_001)));
        assert_eq!(ini.get("General", "Name"), Some("Main"));

        let v = VideoFacts::read(&Ini::parse("[Video]\nFPSType=2\nFPSNum=50\nFPSDen=1\n"));
        assert_eq!(v.fps, Some((50, 1)));
        assert_eq!(v.base, None);
        let v = VideoFacts::read(&Ini::parse("[Video]\nFPSType=1\nFPSInt=48\n"));
        assert_eq!(v.fps, Some((48, 1)));
    }
}
