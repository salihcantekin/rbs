//! `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15, "hvcC"): the HEVC extradata
//! carried by MP4 and Matroska.
//!
//! Parsing reads the NAL length size and the VPS, SPS and PPS. It does not interpret
//! the profile fields; the encoder wrote those.

use crate::FormatError;

/// A parsed hvcC record. Parameter sets borrow from the parsed buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hvcc<'a> {
    /// Bytes per NAL length prefix (1, 2 or 4).
    pub length_size: u8,
    /// VPS, SPS and PPS, without start codes or lengths.
    pub parameter_sets: Vec<&'a [u8]>,
}

impl<'a> Hvcc<'a> {
    /// Parses an hvcC record. Not for hot paths (allocates the parameter-set list).
    pub fn parse(data: &'a [u8]) -> Result<Self, FormatError> {
        if data.first() != Some(&1) {
            return Err(FormatError::Unsupported("hvcC version"));
        }
        let marker = *data.get(21).ok_or(FormatError::Truncated("hvcC"))?;
        let length_size = (marker & 0x3) + 1;
        if length_size == 3 {
            return Err(FormatError::Invalid("hvcC NAL length size"));
        }
        let arrays = usize::from(*data.get(22).ok_or(FormatError::Truncated("hvcC arrays"))?);
        let mut pos = 23;
        let mut parameter_sets = Vec::new();
        for _ in 0..arrays {
            let kind = data.get(pos).ok_or(FormatError::Truncated("hvcC array"))? & 0x3f;
            pos += 1;
            let count_bytes = data
                .get(pos..pos + 2)
                .ok_or(FormatError::Truncated("hvcC NAL count"))?;
            let count = usize::from(u16::from_be_bytes([count_bytes[0], count_bytes[1]]));
            pos += 2;
            for _ in 0..count {
                let len_bytes = data
                    .get(pos..pos + 2)
                    .ok_or(FormatError::Truncated("hvcC NAL length"))?;
                let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
                pos += 2;
                let nal = data
                    .get(pos..pos + len)
                    .ok_or(FormatError::Truncated("hvcC NAL"))?;
                pos += len;
                // 32 VPS, 33 SPS, 34 PPS.
                if matches!(kind, 32..=34) {
                    parameter_sets.push(nal);
                }
            }
        }
        if parameter_sets.is_empty() {
            return Err(FormatError::Invalid("hvcC without parameter sets"));
        }
        Ok(Self {
            length_size,
            parameter_sets,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_length_size_and_the_parameter_sets() {
        // version, 20 profile bytes, lengthSizeMinusOne = 3, one VPS array.
        let mut data = vec![1];
        data.extend(std::iter::repeat_n(0u8, 20));
        data.push(0x03);
        data.push(1);
        data.push(0x80 | 32); // array completeness, VPS
        data.extend_from_slice(&1u16.to_be_bytes());
        data.extend_from_slice(&3u16.to_be_bytes());
        data.extend_from_slice(&[0x40, 0x01, 0x0c]);
        let parsed = Hvcc::parse(&data).unwrap();
        assert_eq!(parsed.length_size, 4);
        assert_eq!(parsed.parameter_sets, vec![&[0x40, 0x01, 0x0c][..]]);
    }
}
