//! AAC: `AudioSpecificConfig` (ISO/IEC 14496-3) and ADTS headers.

use crate::FormatError;

/// Sampling frequencies indexed by `samplingFrequencyIndex`.
pub const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The fields of an `AudioSpecificConfig` that containers need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    /// Audio object type (2 = AAC-LC).
    pub object_type: u8,
    /// Index into [`SAMPLE_RATES`].
    pub sample_rate_index: u8,
    /// Channel configuration (1 = mono, 2 = stereo, ... 7 = 7.1).
    pub channels: u8,
}

impl AudioSpecificConfig {
    /// AAC-LC at `sample_rate` with `channels` (channel configuration 1..=7).
    pub fn lc(sample_rate: u32, channels: u8) -> Result<Self, FormatError> {
        let index = SAMPLE_RATES
            .iter()
            .position(|r| *r == sample_rate)
            .ok_or(FormatError::Unsupported("AAC sample rate"))?;
        if !(1..=7).contains(&channels) {
            return Err(FormatError::Unsupported("AAC channel configuration"));
        }
        Ok(Self {
            object_type: 2,
            sample_rate_index: index as u8,
            channels,
        })
    }

    /// Parses the first two bytes (explicit sample rates and extensions are not
    /// supported).
    pub fn parse(data: &[u8]) -> Result<Self, FormatError> {
        let b = data
            .get(..2)
            .ok_or(FormatError::Truncated("AudioSpecificConfig"))?;
        let object_type = b[0] >> 3;
        let sample_rate_index = ((b[0] & 0x7) << 1) | (b[1] >> 7);
        let channels = (b[1] >> 3) & 0xf;
        if object_type == 0 || object_type == 31 {
            return Err(FormatError::Unsupported("audio object type"));
        }
        if usize::from(sample_rate_index) >= SAMPLE_RATES.len() {
            return Err(FormatError::Unsupported("explicit AAC sample rate"));
        }
        Ok(Self {
            object_type,
            sample_rate_index,
            channels,
        })
    }

    /// The two-byte encoding.
    pub const fn to_bytes(self) -> [u8; 2] {
        [
            (self.object_type << 3) | (self.sample_rate_index >> 1),
            ((self.sample_rate_index & 1) << 7) | (self.channels << 3),
        ]
    }

    /// The sample rate in Hz.
    pub fn sample_rate(self) -> u32 {
        SAMPLE_RATES[usize::from(self.sample_rate_index) % SAMPLE_RATES.len()]
    }

    /// The 7-byte ADTS header (no CRC) for a raw frame of `payload_len` bytes.
    ///
    /// ADTS can only signal object types 1 to 4 (Main, LC, SSR, LTP) and channel
    /// configurations 1 to 7; others, including explicitly signalled HE-AAC (5, 29) and
    /// configuration 0 (PCE), are `Unsupported`.
    pub fn adts_header(self, payload_len: usize) -> Result<[u8; 7], FormatError> {
        if !(1..=4).contains(&self.object_type) {
            return Err(FormatError::Unsupported("audio object type in ADTS"));
        }
        if !(1..=7).contains(&self.channels) {
            return Err(FormatError::Unsupported("channel configuration in ADTS"));
        }
        let len = payload_len + 7;
        if len > 0x1fff {
            return Err(FormatError::Invalid("ADTS frame length"));
        }
        let profile = self.object_type.saturating_sub(1) & 0x3;
        Ok([
            0xff,
            0xf1, // MPEG-4, layer 0, no CRC
            (profile << 6) | (self.sample_rate_index << 2) | (self.channels >> 2),
            ((self.channels & 0x3) << 6) | ((len >> 11) as u8 & 0x3),
            (len >> 3) as u8,
            (((len & 0x7) as u8) << 5) | 0x1f,
            0xfc,
        ])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn lc_48k_stereo_matches_known_bytes() {
        let asc = AudioSpecificConfig::lc(48_000, 2).unwrap();
        assert_eq!(asc.to_bytes(), [0x11, 0x90]);
        assert_eq!(AudioSpecificConfig::parse(&[0x11, 0x90]), Ok(asc));
        assert_eq!(asc.sample_rate(), 48_000);
        assert_eq!(
            AudioSpecificConfig::lc(44_100, 2).unwrap().to_bytes(),
            [0x12, 0x10]
        );
    }

    #[test]
    fn adts_header_fields() {
        let asc = AudioSpecificConfig::lc(48_000, 2).unwrap();
        let h = asc.adts_header(100).unwrap();
        assert_eq!(&h[..2], &[0xff, 0xf1]);
        let len =
            (usize::from(h[3] & 0x3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        assert_eq!(len, 107);
        assert_eq!((h[2] >> 2) & 0xf, 3); // 48 kHz
        assert!(asc.adts_header(9000).is_err());
        // Explicit HE-AAC (object type 5) and channel configurations 0 and 8.
        let he = AudioSpecificConfig::parse(&[0x2b, 0x12]).unwrap();
        assert_eq!(he.object_type, 5);
        assert!(he.adts_header(10).is_err());
        let mut bad = asc;
        bad.channels = 8;
        assert!(bad.adts_header(10).is_err());
        bad.channels = 0;
        assert!(bad.adts_header(10).is_err());
    }

    #[test]
    fn rejects_unsupported_configs() {
        assert!(AudioSpecificConfig::lc(12_345, 2).is_err());
        assert!(AudioSpecificConfig::lc(48_000, 0).is_err());
        assert!(AudioSpecificConfig::parse(&[0x00, 0x00]).is_err());
        assert!(AudioSpecificConfig::parse(&[0x17, 0x80]).is_err());
    }
}
