//! What a finished file holds, for the manifest's stop rewrite (ADR-0089 decision 6,
//! recording spec "Recording manifest").

use rbs_core::media::{StreamDescriptor, StreamParams};
use rbs_manifest::{FileTiming, Picture, Sound};
use rbs_nodes::output::OutputNodeStats;

/// The media an output delivered and its formats. Times are on the master clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaSpan {
    /// The earliest sample in the file (the muxer's media time 0), in nanoseconds.
    pub first_ns: u64,
    /// Where the file's media ends, in nanoseconds.
    pub end_ns: u64,
    /// Its picture, when it has a video stream.
    pub picture: Option<Picture>,
    /// Its sound, when it has an audio stream.
    pub sound: Option<Sound>,
}

/// The picture and sound of `streams` (the first video and the first audio stream).
pub(crate) fn formats(streams: &[StreamDescriptor]) -> (Option<Picture>, Option<Sound>) {
    let mut picture = None;
    let mut sound = None;
    for s in streams {
        match s.params {
            StreamParams::Video {
                format, frame_rate, ..
            } if picture.is_none() => {
                picture = Some(Picture {
                    width: format.size.width,
                    height: format.size.height,
                    frame_rate,
                });
            }
            StreamParams::Audio { format, .. } if sound.is_none() => {
                sound = Some(Sound {
                    sample_rate: format.sample_rate.hz(),
                    channels: u16::try_from(format.channels.channels()).unwrap_or(u16::MAX),
                });
            }
            _ => {}
        }
    }
    (picture, sound)
}

impl MediaSpan {
    /// What `stats` saw delivered, with the formats of its streams; `None` when no
    /// packet went out.
    pub(crate) fn of(
        stats: &OutputNodeStats,
        (picture, sound): (Option<Picture>, Option<Sound>),
    ) -> Option<Self> {
        let (first_ns, end_ns) = stats.span()?;
        Some(Self {
            first_ns,
            end_ns,
            picture,
            sound,
        })
    }

    /// The manifest's stop fields for a recording whose origin is `t0_ns`. A file that
    /// begins before `t0` (never for a recording folder, whose tracks start at or after
    /// it) is placed at 0; move `t0` first with [`rbs_manifest::RecordingManifest::move_origin`].
    pub fn timing(&self, t0_ns: u64) -> FileTiming {
        FileTiming {
            start_offset_ns: self.first_ns.saturating_sub(t0_ns),
            duration_ns: self.end_ns.saturating_sub(self.first_ns),
            picture: self.picture,
            sound: self.sound,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rbs_core::media::{AudioFormat, BitstreamFormat, Bytes, CodecId};
    use std::sync::atomic::Ordering;

    #[test]
    fn timing_is_relative_to_t0_and_formats_come_from_the_streams() {
        let aac = StreamDescriptor::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            Bytes::new(),
            StreamParams::audio(AudioFormat::STEREO_48K, 1024),
        );
        let stats = OutputNodeStats::default();
        assert_eq!(
            MediaSpan::of(&stats, formats(std::slice::from_ref(&aac))),
            None
        );
        stats.first_pts_ns.store(5_250, Ordering::Relaxed);
        stats.end_ns.store(9_000, Ordering::Relaxed);
        let span = MediaSpan::of(&stats, formats(&[aac])).unwrap();
        let t = span.timing(5_000);
        assert_eq!((t.start_offset_ns, t.duration_ns), (250, 3_750));
        assert_eq!(t.picture, None);
        assert_eq!(
            t.sound,
            Some(Sound {
                sample_rate: 48_000,
                channels: 2
            })
        );
        assert_eq!(span.timing(6_000).start_offset_ns, 0);
    }
}
