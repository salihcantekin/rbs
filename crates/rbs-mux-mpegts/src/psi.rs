//! Program-specific information: PAT and PMT sections, and the MPEG-2 CRC.

/// PID of the program association table.
pub const PAT_PID: u16 = 0x0000;
/// PID of the program map table.
pub const PMT_PID: u16 = 0x1000;
/// PID of the first elementary stream; stream `i` uses `ES_PID_BASE + i`.
pub const ES_PID_BASE: u16 = 0x0100;

/// `stream_type` of an H.264 elementary stream.
pub const STREAM_TYPE_H264: u8 = 0x1b;
/// `stream_type` of AAC in ADTS framing.
pub const STREAM_TYPE_AAC_ADTS: u8 = 0x0f;

const PROGRAM_NUMBER: u16 = 1;
const TRANSPORT_STREAM_ID: u16 = 1;

/// CRC-32/MPEG-2 (polynomial 0x04C11DB7, initial value all ones, no reflection).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in data {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn finish_section(mut section: Vec<u8>) -> Vec<u8> {
    // section_length counts the bytes after the length field, CRC included.
    let length = section.len() - 3 + 4;
    section[1] = 0xb0 | ((length >> 8) as u8 & 0x0f);
    section[2] = length as u8;
    let crc = crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

/// The PAT section with one program whose PMT is on [`PMT_PID`].
pub fn pat_section() -> Vec<u8> {
    let [tsid_hi, tsid_lo] = TRANSPORT_STREAM_ID.to_be_bytes();
    let [prog_hi, prog_lo] = PROGRAM_NUMBER.to_be_bytes();
    finish_section(vec![
        0x00, // table_id: program_association_section
        0,
        0,
        tsid_hi,
        tsid_lo,
        0xc1, // version 0, current_next_indicator
        0,    // section_number
        0,    // last_section_number
        prog_hi,
        prog_lo,
        0xe0 | (PMT_PID >> 8) as u8,
        PMT_PID as u8,
    ])
}

/// The PMT section for `(stream_type, pid)` pairs, with the PCR on `pcr_pid`.
pub fn pmt_section(streams: &[(u8, u16)], pcr_pid: u16) -> Vec<u8> {
    let [prog_hi, prog_lo] = PROGRAM_NUMBER.to_be_bytes();
    let mut section = vec![
        0x02, // table_id: TS_program_map_section
        0,
        0,
        prog_hi,
        prog_lo,
        0xc1,
        0,
        0,
        0xe0 | (pcr_pid >> 8) as u8,
        pcr_pid as u8,
        0xf0, // program_info_length = 0
        0x00,
    ];
    for (stream_type, pid) in streams {
        section.extend_from_slice(&[
            *stream_type,
            0xe0 | (pid >> 8) as u8,
            *pid as u8,
            0xf0,
            0x00,
        ]);
    }
    finish_section(section)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_mpeg2_check_value() {
        assert_eq!(crc32(b"123456789"), 0x0376_e6e7);
    }

    #[test]
    fn sections_end_with_a_valid_crc() {
        for section in [
            pat_section(),
            pmt_section(&[(STREAM_TYPE_H264, 0x100)], 0x100),
        ] {
            // The CRC over a whole section, CRC included, is zero.
            assert_eq!(crc32(&section), 0);
            let length = (usize::from(section[1] & 0x0f) << 8) | usize::from(section[2]);
            assert_eq!(length + 3, section.len());
        }
    }
}
