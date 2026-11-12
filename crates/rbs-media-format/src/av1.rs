//! `AV1CodecConfigurationRecord` (AV1 ISO BMFF, "av1C"): the AV1 extradata carried
//! by MP4 and Matroska.
//!
//! An encoder's extradata is the sequence-header OBU (and any metadata OBUs the
//! sequence parameters include). [`Av1Config::from_extradata`] also accepts a record
//! that is already an av1C. Building the record is session set-up, not a hot path.

use crate::FormatError;

/// OBU type of a sequence header.
const OBU_SEQUENCE_HEADER: u8 = 1;
/// OBU type of a metadata OBU. Kept in `configOBUs` with the sequence header.
const OBU_METADATA: u8 = 5;

/// A parsed AV1 codec configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1Config {
    /// `seq_profile` (0 is Main).
    pub profile: u8,
    /// `seq_level_idx` of operating point 0.
    pub level: u8,
    /// `seq_tier` of operating point 0. False is the Main tier.
    pub tier: bool,
    /// 10-bit or 12-bit pictures.
    pub high_bitdepth: bool,
    /// 12-bit pictures (profile 2 only).
    pub twelve_bit: bool,
    /// Luma only.
    pub monochrome: bool,
    /// Chroma is subsampled horizontally.
    pub subsampling_x: bool,
    /// Chroma is subsampled vertically.
    pub subsampling_y: bool,
    /// `chroma_sample_position` (0 when the chroma is not 4:2:0).
    pub chroma_sample_position: u8,
    /// Sequence header OBU and metadata OBUs, in the order they were given.
    pub config_obus: Vec<u8>,
}

impl Av1Config {
    /// Parses an av1C record. Not for hot paths.
    pub fn parse(data: &[u8]) -> Result<Self, FormatError> {
        let header = data.get(..4).ok_or(FormatError::Truncated("av1C header"))?;
        if header[0] != 0x81 {
            return Err(FormatError::Unsupported("av1C version"));
        }
        // The initial-presentation-delay bit sits in the high half of the fourth byte.
        if header[3] & 0x10 != 0 {
            return Err(FormatError::Unsupported("av1C initial presentation delay"));
        }
        Ok(Self {
            profile: header[1] >> 5,
            level: header[1] & 0x1f,
            tier: header[2] & 0x80 != 0,
            high_bitdepth: header[2] & 0x40 != 0,
            twelve_bit: header[2] & 0x20 != 0,
            monochrome: header[2] & 0x10 != 0,
            subsampling_x: header[2] & 0x08 != 0,
            subsampling_y: header[2] & 0x04 != 0,
            chroma_sample_position: header[2] & 0x03,
            config_obus: data[4..].to_vec(),
        })
    }

    /// Builds an av1C record (version 1, no presentation delay) into `out`.
    pub fn build(&self, out: &mut Vec<u8>) {
        out.push(0x81);
        out.push((self.profile & 0x7) << 5 | (self.level & 0x1f));
        let mut flags = self.chroma_sample_position & 0x03;
        if self.tier {
            flags |= 0x80;
        }
        if self.high_bitdepth {
            flags |= 0x40;
        }
        if self.twelve_bit {
            flags |= 0x20;
        }
        if self.monochrome {
            flags |= 0x10;
        }
        if self.subsampling_x {
            flags |= 0x08;
        }
        if self.subsampling_y {
            flags |= 0x04;
        }
        out.push(flags);
        out.push(0);
        out.extend_from_slice(&self.config_obus);
    }

    /// Reads either an av1C record or a sequence of OBUs whose first sequence header
    /// describes the stream.
    pub fn from_extradata(data: &[u8]) -> Result<Self, FormatError> {
        // A sequence-header OBU cannot start with 0x81: that byte has the forbidden bit set.
        if data.first() == Some(&0x81) {
            Self::parse(data)
        } else {
            Self::from_obus(data)
        }
    }

    /// Reads the first sequence-header OBU in `data` and keeps sequence-header and
    /// metadata OBUs as `configOBUs`.
    pub fn from_obus(data: &[u8]) -> Result<Self, FormatError> {
        let obus = split_obus(data)?;
        let mut config_obus = Vec::new();
        let mut sequence = None;
        for obu in &obus {
            if obu.kind == OBU_SEQUENCE_HEADER || obu.kind == OBU_METADATA {
                config_obus.extend_from_slice(&data[obu.full.clone()]);
            }
            if obu.kind == OBU_SEQUENCE_HEADER && sequence.is_none() {
                sequence = Some(obu.payload.clone());
            }
        }
        let Some(payload) = sequence else {
            return Err(FormatError::Invalid("AV1 sequence header"));
        };
        let mut fields = parse_sequence_header(&data[payload])?;
        fields.config_obus = config_obus;
        Ok(fields)
    }
}

struct Obu {
    kind: u8,
    /// The whole OBU, header included.
    full: std::ops::Range<usize>,
    /// Payload after the header and the size field.
    payload: std::ops::Range<usize>,
}

fn split_obus(data: &[u8]) -> Result<Vec<Obu>, FormatError> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < data.len() {
        let header = *data
            .get(at)
            .ok_or(FormatError::Truncated("AV1 OBU header"))?;
        if header & 0x80 != 0 {
            return Err(FormatError::Invalid("AV1 OBU forbidden bit"));
        }
        let kind = (header >> 3) & 0x0f;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut pos = at + 1;
        if extension {
            pos += 1;
            if pos > data.len() {
                return Err(FormatError::Truncated("AV1 OBU extension"));
            }
        }
        let (payload_at, end) = if has_size {
            let (size, used) = leb128(&data[pos..])?;
            let payload_at = pos + used;
            let end = payload_at + size;
            if end > data.len() {
                return Err(FormatError::Truncated("AV1 OBU"));
            }
            (payload_at, end)
        } else {
            (pos, data.len())
        };
        out.push(Obu {
            kind,
            full: at..end,
            payload: payload_at..end,
        });
        at = end;
        if !has_size {
            break;
        }
    }
    if out.is_empty() {
        return Err(FormatError::Truncated("AV1 OBU"));
    }
    Ok(out)
}

/// Unsigned LEB128, at most four bytes (an OBU size fits).
fn leb128(data: &[u8]) -> Result<(usize, usize), FormatError> {
    let mut value = 0usize;
    for i in 0..4 {
        let byte = *data.get(i).ok_or(FormatError::Truncated("AV1 leb128"))?;
        value |= usize::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err(FormatError::Invalid("AV1 leb128"))
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Result<u32, FormatError> {
        if n > 32 || self.pos + n as usize > self.data.len() * 8 {
            return Err(FormatError::Truncated("AV1 sequence header"));
        }
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self.data[self.pos / 8];
            let shift = 7 - (self.pos % 8);
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.pos += 1;
        }
        Ok(value)
    }

    fn flag(&mut self) -> Result<bool, FormatError> {
        Ok(self.bits(1)? == 1)
    }

    fn uvlc(&mut self) -> Result<u32, FormatError> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err(FormatError::Invalid("AV1 uvlc"));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.bits(zeros)?;
        Ok(((1u32 << zeros) - 1).saturating_add(rest))
    }
}

/// The sequence-header fields an av1C record stores. `config_obus` is filled by the caller.
fn parse_sequence_header(payload: &[u8]) -> Result<Av1Config, FormatError> {
    let mut bits = Bits {
        data: payload,
        pos: 0,
    };
    let profile = bits.bits(3)? as u8;
    if profile > 2 {
        return Err(FormatError::Unsupported("AV1 profile"));
    }
    let _still = bits.flag()?;
    let reduced = bits.flag()?;
    let (level, tier) = if reduced {
        (bits.bits(5)? as u8, false)
    } else {
        let timing = bits.flag()?;
        let decoder_model = if timing {
            skip_timing_info(&mut bits)?;
            let present = bits.flag()?;
            if present {
                Some(decoder_model_info(&mut bits)?)
            } else {
                None
            }
        } else {
            None
        };
        let initial_display_delay = bits.flag()?;
        let points = bits.bits(5)?;
        let mut level = 0u8;
        let mut tier = false;
        for i in 0..=points {
            let _idc = bits.bits(12)?;
            let seq_level = bits.bits(5)? as u8;
            let seq_tier = if seq_level > 7 { bits.flag()? } else { false };
            if i == 0 {
                level = seq_level;
                tier = seq_tier;
            }
            if let Some(n) = decoder_model {
                if bits.flag()? {
                    let _decoder = bits.bits(n)?;
                    let _encoder = bits.bits(n)?;
                    let _low_delay = bits.flag()?;
                }
            }
            if initial_display_delay && bits.flag()? {
                let _delay = bits.bits(4)?;
            }
        }
        (level, tier)
    };
    let width_bits = bits.bits(4)? + 1;
    let height_bits = bits.bits(4)? + 1;
    let _max_width = bits.bits(width_bits)?;
    let _max_height = bits.bits(height_bits)?;
    if !reduced && bits.flag()? {
        let _delta = bits.bits(4)?;
        let _additional = bits.bits(3)?;
    }
    let _superblock = bits.flag()?;
    let _filter_intra = bits.flag()?;
    let _intra_edge = bits.flag()?;
    if !reduced {
        let _interintra = bits.flag()?;
        let _masked = bits.flag()?;
        let _warped = bits.flag()?;
        let _dual = bits.flag()?;
        let order_hint = bits.flag()?;
        if order_hint {
            let _jnt = bits.flag()?;
            let _mvs = bits.flag()?;
        }
        // When the choose bit is clear the force value is one bit (0 or 1). A set
        // choose bit means adaptive (2) and is not followed by a value. Same shape
        // for integer motion vectors. This is how decoders read the fields.
        let choose_screen = bits.flag()?;
        let force_screen = if choose_screen { 2 } else { bits.bits(1)? };
        if force_screen > 0 {
            let choose_integer = bits.flag()?;
            if !choose_integer {
                let _force_integer = bits.bits(1)?;
            }
        }
        if order_hint {
            let _hint_bits = bits.bits(3)?;
        }
    }
    let _superres = bits.flag()?;
    let _cdef = bits.flag()?;
    let _restoration = bits.flag()?;
    let color = color_config(&mut bits, profile)?;
    let _film_grain = bits.flag()?;
    Ok(Av1Config {
        profile,
        level,
        tier,
        high_bitdepth: color.high_bitdepth,
        twelve_bit: color.twelve_bit,
        monochrome: color.monochrome,
        subsampling_x: color.subsampling_x,
        subsampling_y: color.subsampling_y,
        chroma_sample_position: color.chroma_sample_position,
        config_obus: Vec::new(),
    })
}

fn skip_timing_info(bits: &mut Bits<'_>) -> Result<(), FormatError> {
    let _display_tick = bits.bits(32)?;
    let _time_scale = bits.bits(32)?;
    if bits.flag()? {
        let _ticks = bits.uvlc()?;
    }
    Ok(())
}

/// `buffer_delay_length_minus_1 + 1`.
fn decoder_model_info(bits: &mut Bits<'_>) -> Result<u32, FormatError> {
    let length = bits.bits(5)? + 1;
    let _decoding_tick = bits.bits(32)?;
    let _removal = bits.bits(5)?;
    let _presentation = bits.bits(5)?;
    Ok(length)
}

struct Color {
    high_bitdepth: bool,
    twelve_bit: bool,
    monochrome: bool,
    subsampling_x: bool,
    subsampling_y: bool,
    chroma_sample_position: u8,
}

fn color_config(bits: &mut Bits<'_>, profile: u8) -> Result<Color, FormatError> {
    let high_bitdepth = bits.flag()?;
    let (twelve_bit, bit_depth) = if profile == 2 && high_bitdepth {
        let twelve = bits.flag()?;
        (twelve, if twelve { 12 } else { 10 })
    } else {
        (false, if high_bitdepth { 10 } else { 8 })
    };
    let monochrome = if profile == 1 { false } else { bits.flag()? };
    let described = bits.flag()?;
    let (primaries, transfer, matrix) = if described {
        (bits.bits(8)?, bits.bits(8)?, bits.bits(8)?)
    } else {
        (2, 2, 2)
    };
    if monochrome {
        let _range = bits.flag()?;
        return Ok(Color {
            high_bitdepth,
            twelve_bit,
            monochrome: true,
            subsampling_x: true,
            subsampling_y: true,
            chroma_sample_position: 0,
        });
    }
    // BT.709 primaries, sRGB transfer, identity matrix: the spec forces 4:4:4 full range.
    if primaries == 1 && transfer == 13 && matrix == 0 {
        return Ok(Color {
            high_bitdepth,
            twelve_bit,
            monochrome: false,
            subsampling_x: false,
            subsampling_y: false,
            chroma_sample_position: 0,
        });
    }
    let _range = bits.flag()?;
    let (subsampling_x, subsampling_y) = if profile == 0 {
        (true, true)
    } else if profile == 1 {
        (false, false)
    } else if bit_depth == 12 {
        let x = bits.flag()?;
        let y = if x { bits.flag()? } else { false };
        (x, y)
    } else {
        (true, false)
    };
    let chroma_sample_position = if subsampling_x && subsampling_y {
        bits.bits(2)? as u8
    } else {
        0
    };
    let _separate_uv = bits.flag()?;
    Ok(Color {
        high_bitdepth,
        twelve_bit,
        monochrome: false,
        subsampling_x,
        subsampling_y,
        chroma_sample_position,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    /// Writes `n` bits, most significant bit first.
    struct Writer {
        bytes: Vec<u8>,
        bit: u8,
        acc: u8,
    }

    impl Writer {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit: 0,
                acc: 0,
            }
        }

        fn bits(&mut self, n: u32, value: u32) {
            for i in (0..n).rev() {
                let one = ((value >> i) & 1) as u8;
                self.acc = (self.acc << 1) | one;
                self.bit += 1;
                if self.bit == 8 {
                    self.bytes.push(self.acc);
                    self.acc = 0;
                    self.bit = 0;
                }
            }
        }

        fn flag(&mut self, value: bool) {
            self.bits(1, u32::from(value));
        }

        fn finish(mut self) -> Vec<u8> {
            if self.bit != 0 {
                self.acc <<= 8 - self.bit;
                self.bytes.push(self.acc);
            }
            self.bytes
        }
    }

    /// Main, 8-bit, 4:2:0, level 2.0, 320x240, Rec. 709 limited, wrapped as one OBU.
    fn sample_sequence_obu() -> Vec<u8> {
        let mut w = Writer::new();
        w.bits(3, 0); // profile
        w.flag(false); // still
        w.flag(false); // reduced
        w.flag(false); // timing
        w.flag(false); // initial display delay
        w.bits(5, 0); // one operating point
        w.bits(12, 0); // idc
        w.bits(5, 0); // level 2.0, no tier bit
        w.bits(4, 8); // width bits minus 1 -> 9
        w.bits(4, 7); // height bits minus 1 -> 8
        w.bits(9, 319);
        w.bits(8, 239);
        w.flag(false); // frame id numbers
        w.flag(false); // 128x128 superblock
        w.flag(false); // filter intra
        w.flag(false); // intra edge
        w.flag(false); // interintra
        w.flag(false); // masked
        w.flag(false); // warped
        w.flag(false); // dual filter
        w.flag(false); // order hint
        w.flag(false); // choose screen content
        w.flag(false); // force screen content off, so integer mv is not written
        w.flag(false); // superres
        w.flag(false); // cdef
        w.flag(false); // restoration
        w.flag(false); // high bitdepth
        w.flag(false); // mono
        w.flag(true); // colour description
        w.bits(8, 1);
        w.bits(8, 1);
        w.bits(8, 1);
        w.flag(false); // limited range
        w.bits(2, 0); // chroma sample position
        w.flag(false); // separate uv
        w.flag(false); // film grain
        let payload = w.finish();
        let mut obu = vec![0x0a]; // type 1, size field present
        obu.push(payload.len() as u8); // leb128, one byte
        obu.extend(payload);
        obu
    }

    #[test]
    fn sequence_header_builds_an_av1c_that_parses_back() {
        let obu = sample_sequence_obu();
        assert_eq!(
            obu,
            [
                0x0a, 0x0d, 0x00, 0x00, 0x00, 0x04, 0x3c, 0xff, 0xbc, 0x00, 0x02, 0x02, 0x02, 0x02,
                0x00,
            ]
        );
        let config = Av1Config::from_obus(&obu).unwrap();
        assert_eq!(config.profile, 0);
        assert_eq!(config.level, 0);
        assert!(!config.tier);
        assert!(!config.high_bitdepth);
        assert!(!config.twelve_bit);
        assert!(!config.monochrome);
        assert!(config.subsampling_x && config.subsampling_y);
        assert_eq!(config.chroma_sample_position, 0);
        assert_eq!(config.config_obus, obu);

        let mut record = Vec::new();
        config.build(&mut record);
        assert_eq!(record[0], 0x81);
        let again = Av1Config::from_extradata(&record).unwrap();
        assert_eq!(again.profile, config.profile);
        assert_eq!(again.level, config.level);
        assert_eq!(again.config_obus, obu);
        assert_eq!(Av1Config::from_extradata(&obu).unwrap(), config);
    }

    #[test]
    fn a_buffer_without_a_sequence_header_is_refused() {
        // Temporal delimiter OBU, type 2, empty payload, size present: 0x12 0x00.
        assert!(matches!(
            Av1Config::from_obus(&[0x12, 0x00]),
            Err(FormatError::Invalid(_))
        ));
        assert!(matches!(
            Av1Config::parse(&[0x81]),
            Err(FormatError::Truncated(_))
        ));
    }
}
