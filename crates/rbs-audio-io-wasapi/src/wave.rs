//! What a `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` describes, as plain numbers.
//!
//! Kept free of Windows types so the rules are tested on every platform. The Windows
//! glue copies the fields out of the OS structure and calls [`WaveFormat::parse`].

use rbs_core::media::{AudioFormat, ChannelLayout, MAX_CHANNELS};
use rbs_core::timing::SampleRate;

/// `WAVE_FORMAT_PCM`.
pub(crate) const TAG_PCM: u16 = 0x0001;
/// `WAVE_FORMAT_IEEE_FLOAT`.
pub(crate) const TAG_FLOAT: u16 = 0x0003;
/// `WAVE_FORMAT_EXTENSIBLE`.
pub(crate) const TAG_EXTENSIBLE: u16 = 0xFFFE;

/// The `KSDATAFORMAT_SUBTYPE_*` GUIDs are `xxxxxxxx-0000-0010-8000-00aa00389b71`, the
/// first field being the format tag. This is the GUID with that field zeroed, as a
/// `u128` in `GUID::to_u128` order.
const SUBTYPE_BASE: u128 = 0x0000_0000_0000_0010_8000_00aa_0038_9b71;

/// `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT`.
const MASK_STEREO: u32 = 0x3;
/// Front left, right, centre and LFE.
const MASK_3_1: u32 = 0xF;
/// Front left, right, centre, LFE, back left and right (`KSAUDIO_SPEAKER_5POINT1`).
const MASK_5_1_BACK: u32 = 0x3F;
/// Front left, right, centre, LFE, side left and right (`KSAUDIO_SPEAKER_5POINT1_SURROUND`).
const MASK_5_1_SIDE: u32 = 0x60F;
/// Front left, right, centre, LFE, side left and right, back left and right is
/// `0x63F`, but WASAPI orders the back pair before the side pair while RBS's
/// `Surround7_1` puts the side pair first, so 7.1 devices are delivered as
/// `Discrete(8)` rather than reordered.
const _MASK_7_1: u32 = 0x63F;

/// A sample type the ring converts (`rbs_audio_io::ring::Sample`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleKind {
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
    /// Unsigned 8-bit.
    U8,
    /// Signed 16-bit.
    I16,
    /// Packed signed 24-bit.
    I24,
    /// Signed 32-bit (also 24 valid bits in a 32-bit container).
    I32,
}

impl SampleKind {
    /// Bytes per sample.
    pub(crate) const fn bytes(self) -> usize {
        match self {
            SampleKind::U8 => 1,
            SampleKind::I16 => 2,
            SampleKind::I24 => 3,
            SampleKind::F32 | SampleKind::I32 => 4,
            SampleKind::F64 => 8,
        }
    }
}

/// The fields of a wave format the adapter reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WaveFields {
    /// `wFormatTag`.
    pub tag: u16,
    /// `nChannels`.
    pub channels: u16,
    /// `nSamplesPerSec`.
    pub rate: u32,
    /// `nBlockAlign`.
    pub block_align: u16,
    /// `wBitsPerSample` (container bits).
    pub bits: u16,
    /// For `WAVE_FORMAT_EXTENSIBLE`: `Samples.wValidBitsPerSample`, `dwChannelMask` and
    /// `SubFormat` (as `GUID::to_u128`).
    pub extensible: Option<(u16, u32, u128)>,
}

/// A device format the adapter can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WaveFormat {
    /// Sample type of the OS buffers.
    pub kind: SampleKind,
    /// Channels per frame in the OS buffers (may exceed `MAX_CHANNELS`).
    pub source_channels: usize,
    /// What the stream delivers: the rate and the first `MAX_CHANNELS` channels.
    pub format: AudioFormat,
}

impl WaveFormat {
    /// Reads `fields`. `None` when the sample type cannot be converted (compressed
    /// formats, odd containers) or the format is inconsistent.
    pub(crate) fn parse(fields: WaveFields) -> Option<Self> {
        let (tag, mask) = match fields.extensible {
            Some((_, mask, subformat)) => {
                if subformat & !(0xFFFF_FFFF_u128 << 96) != SUBTYPE_BASE {
                    return None;
                }
                let tag = u16::try_from(subformat >> 96).ok()?;
                (tag, Some(mask))
            }
            None => (fields.tag, None),
        };
        let valid = fields.extensible.map_or(fields.bits, |(valid, _, _)| valid);
        let kind = match (tag, fields.bits) {
            (TAG_FLOAT, 32) => SampleKind::F32,
            (TAG_FLOAT, 64) => SampleKind::F64,
            (TAG_PCM, 8) => SampleKind::U8,
            (TAG_PCM, 16) => SampleKind::I16,
            (TAG_PCM, 24) => SampleKind::I24,
            // 24 or 20 valid bits left-aligned in 32 read correctly as i32.
            (TAG_PCM, 32) if valid <= 32 => SampleKind::I32,
            _ => return None,
        };
        let source_channels = usize::from(fields.channels);
        if source_channels == 0 || usize::from(fields.block_align) != source_channels * kind.bytes()
        {
            return None;
        }
        let rate = SampleRate::new(fields.rate)?;
        let format = AudioFormat::new(rate, layout(source_channels, mask)).ok()?;
        Some(Self {
            kind,
            source_channels,
            format,
        })
    }

    /// The delivered channel count.
    pub(crate) fn channels(&self) -> usize {
        self.format.channels.channels()
    }
}

/// The layout of the first `MAX_CHANNELS` of `channels` channels with speaker `mask`
/// (`None` for a plain `WAVEFORMATEX`). Only masks whose WASAPI channel order matches
/// RBS's layouts are named; everything else is discrete.
pub(crate) fn layout(channels: usize, mask: Option<u32>) -> ChannelLayout {
    let named = match (channels, mask) {
        (1, _) => Some(ChannelLayout::Mono),
        (2, None | Some(MASK_STEREO) | Some(0)) => Some(ChannelLayout::Stereo),
        (4, Some(MASK_3_1)) => Some(ChannelLayout::Surround3_1),
        (6, Some(MASK_5_1_BACK) | Some(MASK_5_1_SIDE)) => Some(ChannelLayout::Surround5_1),
        _ => None,
    };
    named.unwrap_or_else(|| {
        let n = channels.min(MAX_CHANNELS);
        ChannelLayout::Discrete(u8::try_from(n).unwrap_or(u8::MAX))
    })
}

/// The `SubFormat` GUID of `tag` as a `u128`, for building extensible formats.
#[cfg(test)]
pub(crate) const fn subformat(tag: u16) -> u128 {
    SUBTYPE_BASE | ((tag as u128) << 96)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn extensible(channels: u16, bits: u16, valid: u16, mask: u32, tag: u16) -> WaveFields {
        WaveFields {
            tag: TAG_EXTENSIBLE,
            channels,
            rate: 48_000,
            block_align: channels * bits / 8,
            bits,
            extensible: Some((valid, mask, subformat(tag))),
        }
    }

    #[test]
    fn reads_the_usual_mix_formats() {
        let mix = WaveFormat::parse(extensible(2, 32, 32, 0x3, TAG_FLOAT)).unwrap();
        assert_eq!(mix.kind, SampleKind::F32);
        assert_eq!(mix.format, AudioFormat::STEREO_48K);

        let usb = WaveFormat::parse(extensible(1, 24, 24, 0x4, TAG_PCM)).unwrap();
        assert_eq!(usb.kind, SampleKind::I24);
        assert_eq!(usb.format.channels, ChannelLayout::Mono);

        let padded = WaveFormat::parse(extensible(2, 32, 24, 0x3, TAG_PCM)).unwrap();
        assert_eq!(padded.kind, SampleKind::I32);

        let plain = WaveFormat::parse(WaveFields {
            tag: TAG_PCM,
            channels: 2,
            rate: 44_100,
            block_align: 4,
            bits: 16,
            extensible: None,
        })
        .unwrap();
        assert_eq!(plain.kind, SampleKind::I16);
        assert_eq!(plain.format.sample_rate, SampleRate::HZ_44100);
    }

    #[test]
    fn surround_layouts_follow_the_channel_order() {
        let six = WaveFormat::parse(extensible(6, 32, 32, 0x60F, TAG_FLOAT)).unwrap();
        assert_eq!(six.format.channels, ChannelLayout::Surround5_1);
        let eight = WaveFormat::parse(extensible(8, 32, 32, 0x63F, TAG_FLOAT)).unwrap();
        assert_eq!(eight.format.channels, ChannelLayout::Discrete(8));
        let quad = WaveFormat::parse(extensible(4, 32, 32, 0x33, TAG_FLOAT)).unwrap();
        assert_eq!(quad.format.channels, ChannelLayout::Discrete(4));
    }

    #[test]
    fn wide_interfaces_deliver_their_first_eight_channels() {
        let wide = WaveFormat::parse(extensible(18, 32, 32, 0, TAG_FLOAT)).unwrap();
        assert_eq!(wide.source_channels, 18);
        assert_eq!(wide.channels(), 8);
        assert_eq!(wide.format.channels, ChannelLayout::Discrete(8));
    }

    #[test]
    fn refuses_what_the_ring_cannot_convert() {
        // A compressed subtype (AC-3 over S/PDIF).
        assert!(WaveFormat::parse(extensible(2, 16, 16, 0x3, 0x0092)).is_none());
        // A GUID outside the KSDATAFORMAT family.
        let mut odd = extensible(2, 32, 32, 0x3, TAG_FLOAT);
        odd.extensible = Some((32, 0x3, 0x1234));
        assert!(WaveFormat::parse(odd).is_none());
        // 20-bit samples in a 24-bit container are fine; 12-bit containers are not.
        assert!(WaveFormat::parse(extensible(2, 12, 12, 0x3, TAG_PCM)).is_none());
        // Inconsistent block alignment.
        let mut bad = extensible(2, 32, 32, 0x3, TAG_FLOAT);
        bad.block_align = 6;
        assert!(WaveFormat::parse(bad).is_none());
        // No channels, no rate.
        assert!(WaveFormat::parse(extensible(0, 32, 32, 0, TAG_FLOAT)).is_none());
        let mut silent = extensible(2, 32, 32, 0x3, TAG_FLOAT);
        silent.rate = 0;
        assert!(WaveFormat::parse(silent).is_none());
    }
}
