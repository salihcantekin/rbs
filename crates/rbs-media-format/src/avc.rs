//! `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15, "avcC"): the H.264 extradata
//! carried by MP4, Matroska and FLV.

use crate::FormatError;
use crate::nal::{self, h264};

/// A parsed avcC record. Parameter sets borrow from the parsed buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig<'a> {
    /// `AVCProfileIndication`.
    pub profile: u8,
    /// `profile_compatibility`.
    pub compatibility: u8,
    /// `AVCLevelIndication`.
    pub level: u8,
    /// Bytes per NAL length prefix (1, 2 or 4).
    pub length_size: u8,
    /// Sequence parameter sets, without start codes or lengths.
    pub sps: Vec<&'a [u8]>,
    /// Picture parameter sets.
    pub pps: Vec<&'a [u8]>,
}

impl<'a> AvcConfig<'a> {
    /// Parses an avcC record. Not for hot paths (allocates the parameter-set lists).
    pub fn parse(data: &'a [u8]) -> Result<Self, FormatError> {
        let header = data.get(..6).ok_or(FormatError::Truncated("avcC header"))?;
        if header[0] != 1 {
            return Err(FormatError::Unsupported("avcC version"));
        }
        let length_size = (header[4] & 0x3) + 1;
        if length_size == 3 {
            return Err(FormatError::Invalid("avcC NAL length size"));
        }
        let mut pos = 6;
        let sps_count = usize::from(header[5] & 0x1f);
        let sps = read_sets(data, &mut pos, sps_count)?;
        let pps_count = usize::from(
            *data
                .get(pos)
                .ok_or(FormatError::Truncated("avcC PPS count"))?,
        );
        pos += 1;
        let pps = read_sets(data, &mut pos, pps_count)?;
        if sps.is_empty() {
            return Err(FormatError::Invalid("avcC without SPS"));
        }
        Ok(Self {
            profile: header[1],
            compatibility: header[2],
            level: header[3],
            length_size,
            sps,
            pps,
        })
    }

    /// Writes the record (4-byte lengths). The profile and level come from the first
    /// SPS.
    pub fn build(sps: &[&[u8]], pps: &[&[u8]], out: &mut Vec<u8>) -> Result<(), FormatError> {
        let first = sps.first().ok_or(FormatError::Invalid("no SPS"))?;
        let ids = first.get(1..4).ok_or(FormatError::Truncated("SPS"))?;
        if sps.len() > 31 || pps.len() > 255 {
            return Err(FormatError::Invalid("too many parameter sets"));
        }
        out.extend_from_slice(&[1, ids[0], ids[1], ids[2], 0xfc | 3, 0xe0 | sps.len() as u8]);
        write_sets(sps, out)?;
        out.push(pps.len() as u8);
        write_sets(pps, out)?;
        Ok(())
    }

    /// Builds an avcC record from the SPS and PPS found in an Annex B access unit
    /// (typically the encoder's first keyframe). `None` if either is missing.
    pub fn from_annexb(access_unit: &[u8], out: &mut Vec<u8>) -> Option<Result<(), FormatError>> {
        let sps: Vec<&[u8]> = nal::annexb_units(access_unit)
            .filter(|u| nal::h264_type(u) == Some(h264::SPS))
            .collect();
        let pps: Vec<&[u8]> = nal::annexb_units(access_unit)
            .filter(|u| nal::h264_type(u) == Some(h264::PPS))
            .collect();
        if sps.is_empty() || pps.is_empty() {
            return None;
        }
        Some(Self::build(&sps, &pps, out))
    }
}

fn read_sets<'a>(
    data: &'a [u8],
    pos: &mut usize,
    count: usize,
) -> Result<Vec<&'a [u8]>, FormatError> {
    let mut sets = Vec::with_capacity(count);
    for _ in 0..count {
        let len = data
            .get(*pos..*pos + 2)
            .ok_or(FormatError::Truncated("avcC parameter set length"))?;
        let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
        let set = data
            .get(*pos + 2..*pos + 2 + len)
            .ok_or(FormatError::Truncated("avcC parameter set"))?;
        sets.push(set);
        *pos += 2 + len;
    }
    Ok(sets)
}

fn write_sets(sets: &[&[u8]], out: &mut Vec<u8>) -> Result<(), FormatError> {
    for set in sets {
        let len =
            u16::try_from(set.len()).map_err(|_| FormatError::Invalid("parameter set size"))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(set);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];

    #[test]
    fn build_and_parse_round_trip() {
        let mut record = Vec::new();
        AvcConfig::build(&[SPS], &[PPS], &mut record).unwrap();
        let parsed = AvcConfig::parse(&record).unwrap();
        assert_eq!(parsed.profile, 0x64);
        assert_eq!(parsed.level, 0x1f);
        assert_eq!(parsed.length_size, 4);
        assert_eq!(parsed.sps, [SPS]);
        assert_eq!(parsed.pps, [PPS]);
    }

    #[test]
    fn extracts_parameter_sets_from_annexb() {
        let mut au = vec![0, 0, 0, 1];
        au.extend_from_slice(SPS);
        au.extend_from_slice(&[0, 0, 1]);
        au.extend_from_slice(PPS);
        au.extend_from_slice(&[0, 0, 1, 0x65, 0x88]);
        let mut record = Vec::new();
        AvcConfig::from_annexb(&au, &mut record).unwrap().unwrap();
        assert_eq!(AvcConfig::parse(&record).unwrap().pps, [PPS]);
        assert!(AvcConfig::from_annexb(&[0, 0, 1, 0x65], &mut Vec::new()).is_none());
    }

    #[test]
    fn rejects_bad_records() {
        assert!(AvcConfig::parse(&[1, 2]).is_err());
        assert!(AvcConfig::parse(&[2, 0, 0, 0, 0xff, 0xe0, 0]).is_err());
        assert!(AvcConfig::parse(&[1, 0, 0, 0, 0xff, 0xe1, 0, 9, 1]).is_err());
        assert!(AvcConfig::parse(&[1, 0, 0, 0, 0xff, 0xe0, 0]).is_err());
    }
}
