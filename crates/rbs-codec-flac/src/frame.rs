//! One FLAC frame (RFC 9639): a fixed-block header, one subframe per channel,
//! zero padding and a CRC-16 footer.
//!
//! Subframes are constant when every sample matches, otherwise a fixed predictor
//! of order 0 to 4 with one Rice partition (or an escaped partition), and a
//! verbatim subframe when that would be larger or a residual will not fit.

/// Fixed block size. The streamable subset allows 4096 at 48 kHz and below.
pub(crate) const BLOCK: usize = 4096;

const CRC8: [u8; 256] = crc8_table();
const CRC16: [u16; 256] = crc16_table();

const fn crc8_table() -> [u8; 256] {
    let mut table = [0_u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u8;
        let mut bit = 0;
        while bit < 8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const fn crc16_table() -> [u16; 256] {
    let mut table = [0_u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x8005;
            } else {
                crc <<= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0_u8;
    for &byte in data {
        crc = CRC8[usize::from(crc ^ byte)];
    }
    crc
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0_u16;
    for &byte in data {
        let index = usize::from((crc >> 8) as u8 ^ byte);
        crc = (crc << 8) ^ CRC16[index];
    }
    crc
}

pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    acc: u64,
    bits: u8,
}

struct Mark {
    len: usize,
    acc: u64,
    bits: u8,
}

impl BitWriter {
    pub(crate) fn with_capacity(bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes),
            acc: 0,
            bits: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.acc = 0;
        self.bits = 0;
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    fn mark(&self) -> Mark {
        Mark {
            len: self.bytes.len(),
            acc: self.acc,
            bits: self.bits,
        }
    }

    fn rewind(&mut self, mark: Mark) {
        self.bytes.truncate(mark.len);
        self.acc = mark.acc;
        self.bits = mark.bits;
    }

    fn total_bits(&self) -> u64 {
        self.bytes.len() as u64 * 8 + u64::from(self.bits)
    }

    fn put(&mut self, value: u32, n: u8) {
        if n == 0 {
            return;
        }
        let mask = if n == 32 {
            u64::from(u32::MAX)
        } else {
            (1_u64 << n) - 1
        };
        self.acc = (self.acc << n) | (u64::from(value) & mask);
        self.bits += n;
        while self.bits >= 8 {
            self.bits -= 8;
            self.bytes.push((self.acc >> self.bits) as u8);
            if self.bits == 0 {
                self.acc = 0;
            } else {
                self.acc &= (1_u64 << self.bits) - 1;
            }
        }
    }

    fn put_signed(&mut self, sample: i32, bps: u8) {
        let mask = if bps == 32 {
            u32::MAX
        } else {
            (1_u32 << bps) - 1
        };
        self.put(sample as u32 & mask, bps);
    }

    fn finish(&mut self) {
        if self.bits > 0 {
            self.bytes.push((self.acc << (8 - self.bits)) as u8);
            self.acc = 0;
            self.bits = 0;
        }
    }
}

/// Sample-rate field of a frame header. `Err` when the rate cannot be stored
/// without sending the decoder back to the streaminfo block (not streamable).
pub(crate) fn rate_fields(hz: u32) -> Result<(u8, Option<u16>), ()> {
    let code = match hz {
        88_200 => 0b0001,
        176_400 => 0b0010,
        192_000 => 0b0011,
        8_000 => 0b0100,
        16_000 => 0b0101,
        22_050 => 0b0110,
        24_000 => 0b0111,
        32_000 => 0b1000,
        44_100 => 0b1001,
        48_000 => 0b1010,
        96_000 => 0b1011,
        n if (1..=255_000).contains(&n) && n.is_multiple_of(1000) && n / 1000 <= 255 => {
            return Ok((0b1100, Some((n / 1000) as u16)));
        }
        n if (1..=65_535).contains(&n) => return Ok((0b1101, Some(n as u16))),
        n if n >= 10 && n.is_multiple_of(10) && n / 10 <= 65_535 => {
            return Ok((0b1110, Some((n / 10) as u16)));
        }
        _ => return Err(()),
    };
    Ok((code, None))
}

fn block_fields(n: u16) -> (u8, Option<u16>) {
    let code = match n {
        192 => 0b0001,
        576 => 0b0010,
        1152 => 0b0011,
        2304 => 0b0100,
        4608 => 0b0101,
        256 => 0b1000,
        512 => 0b1001,
        1024 => 0b1010,
        2048 => 0b1011,
        4096 => 0b1100,
        8192 => 0b1101,
        16384 => 0b1110,
        32768 => 0b1111,
        1..=256 => return (0b0110, Some(n - 1)),
        _ => return (0b0111, Some(n - 1)),
    };
    (code, None)
}

fn bps_code(bps: u8) -> u8 {
    match bps {
        16 => 0b100,
        24 => 0b110,
        _ => 0,
    }
}

fn push_utf8(out: &mut Vec<u8>, n: u32) -> bool {
    if n > 0x7FFF_FFFF {
        return false;
    }
    if n < 0x80 {
        out.push(n as u8);
    } else if n < 0x800 {
        out.push(0xC0 | (n >> 6) as u8);
        out.push(0x80 | (n & 0x3F) as u8);
    } else if n < 0x1_0000 {
        out.push(0xE0 | (n >> 12) as u8);
        out.push(0x80 | ((n >> 6) & 0x3F) as u8);
        out.push(0x80 | (n & 0x3F) as u8);
    } else if n < 0x20_0000 {
        out.push(0xF0 | (n >> 18) as u8);
        out.push(0x80 | ((n >> 12) & 0x3F) as u8);
        out.push(0x80 | ((n >> 6) & 0x3F) as u8);
        out.push(0x80 | (n & 0x3F) as u8);
    } else {
        out.push(0xF8 | (n >> 24) as u8);
        out.push(0x80 | ((n >> 18) & 0x3F) as u8);
        out.push(0x80 | ((n >> 12) & 0x3F) as u8);
        out.push(0x80 | ((n >> 6) & 0x3F) as u8);
        out.push(0x80 | (n & 0x3F) as u8);
    }
    true
}

fn zigzag(n: i32) -> u32 {
    ((n as u32) << 1) ^ ((n >> 31) as u32)
}

fn predict(order: usize, samples: &[i32], index: usize) -> Option<i32> {
    let p = match order {
        0 => 0_i64,
        1 => i64::from(samples[index - 1]),
        2 => 2 * i64::from(samples[index - 1]) - i64::from(samples[index - 2]),
        3 => {
            3 * i64::from(samples[index - 1]) - 3 * i64::from(samples[index - 2])
                + i64::from(samples[index - 3])
        }
        4 => {
            4 * i64::from(samples[index - 1]) - 6 * i64::from(samples[index - 2])
                + 4 * i64::from(samples[index - 3])
                - i64::from(samples[index - 4])
        }
        _ => return None,
    };
    i32::try_from(p).ok()
}

fn residual(sample: i32, prediction: i32) -> Option<i32> {
    let value = i64::from(sample) - i64::from(prediction);
    if value == i64::from(i32::MIN) {
        None
    } else {
        i32::try_from(value).ok()
    }
}

fn fill_residuals(samples: &[i32], order: usize, out: &mut Vec<i32>) -> bool {
    if order >= samples.len() {
        return false;
    }
    out.clear();
    for index in order..samples.len() {
        let Some(prediction) = predict(order, samples, index) else {
            return false;
        };
        let Some(value) = residual(samples[index], prediction) else {
            return false;
        };
        out.push(value);
    }
    true
}

fn rice_param(residuals: &[i32]) -> u8 {
    if residuals.is_empty() {
        return 0;
    }
    let mut sum = 0_u64;
    for &value in residuals {
        sum = sum.saturating_add(u64::from(value.unsigned_abs()));
    }
    let mean = sum / residuals.len() as u64;
    if mean == 0 {
        return 0;
    }
    (63 - mean.leading_zeros()).saturating_sub(1).min(14) as u8
}

fn max_quotient(residuals: &[i32], param: u8) -> u32 {
    residuals
        .iter()
        .map(|value| zigzag(*value) >> param)
        .max()
        .unwrap_or(0)
}

fn signed_width(residuals: &[i32]) -> u8 {
    let mut max = 0_u32;
    for &value in residuals {
        max = max.max(value.unsigned_abs());
    }
    if max == 0 {
        0
    } else {
        (32 - max.leading_zeros() + 1) as u8
    }
}

/// `Ok(param)` when every quotient stays small, `Err(bits)` when the partition
/// has to be stored unencoded.
fn tune_param(residuals: &[i32]) -> Result<u8, u8> {
    let mut param = rice_param(residuals);
    loop {
        if max_quotient(residuals, param) <= 64 {
            return Ok(param);
        }
        if param == 14 {
            return Err(signed_width(residuals));
        }
        param += 1;
    }
}

fn rice_cost(residuals: &[i32], order: usize, bps: u8) -> u64 {
    let header = 8 + u64::from(bps) * order as u64 + 6;
    match tune_param(residuals) {
        Ok(param) => {
            let mut bits = header + 4;
            let shift = u32::from(param);
            for &value in residuals {
                bits += u64::from((zigzag(value) >> shift) + 1 + u32::from(param));
            }
            bits
        }
        Err(width) => header + 9 + u64::from(width) * residuals.len() as u64,
    }
}

fn choose_order(samples: &[i32], bps: u8, scratch: &mut Vec<i32>) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for order in 0..5 {
        if fill_residuals(samples, order, scratch) {
            let cost = rice_cost(scratch, order, bps);
            let better = match best {
                None => true,
                Some((_, previous)) => cost < previous,
            };
            if better {
                best = Some((order, cost));
            }
        }
    }
    let (order, _) = best?;
    if fill_residuals(samples, order, scratch) {
        Some(order)
    } else {
        None
    }
}

fn put_rice(out: &mut BitWriter, folded: u32, param: u8) {
    let mut quotient = folded >> param;
    while quotient > 0 {
        let chunk = quotient.min(32);
        out.put(0, chunk as u8);
        quotient -= chunk;
    }
    out.put(1, 1);
    if param > 0 {
        out.put(folded & ((1_u32 << param) - 1), param);
    }
}

fn write_residual(out: &mut BitWriter, residuals: &[i32]) -> bool {
    out.put(0b00, 2);
    out.put(0, 4);
    match tune_param(residuals) {
        Ok(param) => {
            out.put(u32::from(param), 4);
            for &value in residuals {
                put_rice(out, zigzag(value), param);
            }
            true
        }
        Err(width) if width <= 31 => {
            out.put(0b1111, 4);
            out.put(u32::from(width), 5);
            for &value in residuals {
                out.put_signed(value, width);
            }
            true
        }
        Err(_) => false,
    }
}

fn write_constant(out: &mut BitWriter, sample: i32, bps: u8) {
    out.put(0, 1);
    out.put(0, 6);
    out.put(0, 1);
    out.put_signed(sample, bps);
}

fn write_verbatim(out: &mut BitWriter, samples: &[i32], bps: u8) {
    out.put(0, 1);
    out.put(0b000001, 6);
    out.put(0, 1);
    for &sample in samples {
        out.put_signed(sample, bps);
    }
}

fn write_fixed(
    out: &mut BitWriter,
    samples: &[i32],
    residuals: &[i32],
    order: usize,
    bps: u8,
) -> bool {
    out.put(0, 1);
    out.put(8 + order as u32, 6);
    out.put(0, 1);
    for &sample in &samples[..order] {
        out.put_signed(sample, bps);
    }
    write_residual(out, residuals)
}

fn write_subframe(out: &mut BitWriter, samples: &[i32], bps: u8, scratch: &mut Vec<i32>) {
    if samples.is_empty() {
        return;
    }
    if samples.iter().all(|sample| *sample == samples[0]) {
        write_constant(out, samples[0], bps);
        return;
    }
    let verbatim_bits = 8 + samples.len() as u64 * u64::from(bps);
    if let Some(order) = choose_order(samples, bps, scratch) {
        let mark = out.mark();
        let before = out.total_bits();
        if write_fixed(out, samples, scratch, order, bps)
            && out.total_bits() - before < verbatim_bits
        {
            return;
        }
        out.rewind(mark);
    }
    write_verbatim(out, samples, bps);
}

fn write_header(
    out: &mut BitWriter,
    channels: usize,
    n: usize,
    bps: u8,
    rate_hz: u32,
    frame_number: u32,
) -> Result<(), ()> {
    let (block_code, block_extra) = block_fields(u16::try_from(n).map_err(|_| ())?);
    let (rate_code, rate_extra) = rate_fields(rate_hz)?;
    out.bytes.push(0xFF);
    out.bytes.push(0xF8);
    out.bytes.push((block_code << 4) | rate_code);
    let channel_code = u8::try_from(channels - 1).map_err(|_| ())?;
    out.bytes.push((channel_code << 4) | (bps_code(bps) << 1));
    if !push_utf8(&mut out.bytes, frame_number) {
        return Err(());
    }
    if let Some(extra) = block_extra {
        if block_code == 0b0110 {
            out.bytes.push(extra as u8);
        } else {
            out.bytes.extend_from_slice(&extra.to_be_bytes());
        }
    }
    if let Some(extra) = rate_extra {
        match rate_code {
            0b1100 => out.bytes.push(extra as u8),
            _ => out.bytes.extend_from_slice(&extra.to_be_bytes()),
        }
    }
    let crc = crc8(&out.bytes);
    out.bytes.push(crc);
    Ok(())
}

/// One frame's samples and the header fields that describe them.
pub(crate) struct FrameIn<'a> {
    /// Planar samples, `channels` rows of `stride` with `n` live samples each.
    pub planar: &'a [i32],
    /// Channel count, 1 to 8.
    pub channels: usize,
    /// Samples in this frame. The last frame may be shorter than [`BLOCK`].
    pub n: usize,
    /// Row stride. [`BLOCK`] for a session; `n` when the buffer is packed.
    pub stride: usize,
    /// 16 or 24.
    pub bps: u8,
    /// Sample rate in Hz.
    pub rate_hz: u32,
    /// Fixed-block frame number.
    pub frame_number: u32,
}

/// Encodes one frame into `out` (cleared by the caller).
pub(crate) fn encode_planar(
    frame: &FrameIn<'_>,
    scratch: &mut Vec<i32>,
    out: &mut BitWriter,
) -> Result<(), ()> {
    if frame.n == 0
        || frame.channels == 0
        || frame.channels > 8
        || frame.planar.len() < frame.channels * frame.stride
    {
        return Err(());
    }
    write_header(
        out,
        frame.channels,
        frame.n,
        frame.bps,
        frame.rate_hz,
        frame.frame_number,
    )?;
    for channel in 0..frame.channels {
        let start = channel * frame.stride;
        write_subframe(
            out,
            &frame.planar[start..start + frame.n],
            frame.bps,
            scratch,
        );
    }
    out.finish();
    let crc = crc16(&out.bytes);
    out.bytes.extend_from_slice(&crc.to_be_bytes());
    Ok(())
}

/// Describes a rate the frame header cannot store. Kept so a bad rate has a
/// readable create error without allocating on the encode path.
pub(crate) fn rate_error(hz: u32) -> String {
    format!("flac sample rate {hz}")
}

#[cfg(test)]
pub(crate) fn tests_decode(data: &[u8]) -> Option<Vec<Vec<i32>>> {
    tests::channels_of(data)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct Reader<'a> {
        data: &'a [u8],
        bit: usize,
    }

    impl<'a> Reader<'a> {
        fn get(&mut self, n: u8) -> Option<u32> {
            let mut value = 0_u32;
            for _ in 0..n {
                let byte = *self.data.get(self.bit / 8)?;
                let shift = 7 - (self.bit % 8);
                value = (value << 1) | u32::from((byte >> shift) & 1);
                self.bit += 1;
            }
            Some(value)
        }

        fn get_signed(&mut self, n: u8) -> Option<i32> {
            let value = self.get(n)?;
            if n == 0 {
                return Some(0);
            }
            let sign = 1_u32 << (n - 1);
            if value & sign == 0 || n == 32 {
                Some(value as i32)
            } else {
                Some((value | (!0_u32 << n)) as i32)
            }
        }
    }

    fn read_utf8(data: &[u8], index: &mut usize) -> Option<u32> {
        let first = *data.get(*index)?;
        *index += 1;
        if first < 0x80 {
            return Some(u32::from(first));
        }
        let (extra, mut value) = if first & 0xE0 == 0xC0 {
            (1, u32::from(first & 0x1F))
        } else if first & 0xF0 == 0xE0 {
            (2, u32::from(first & 0x0F))
        } else if first & 0xF8 == 0xF0 {
            (3, u32::from(first & 0x07))
        } else if first & 0xFC == 0xF8 {
            (4, u32::from(first & 0x03))
        } else {
            return None;
        };
        for _ in 0..extra {
            let next = *data.get(*index)?;
            if next & 0xC0 != 0x80 {
                return None;
            }
            *index += 1;
            value = (value << 6) | u32::from(next & 0x3F);
        }
        Some(value)
    }

    fn block_size(code: u8, extra: &mut usize, data: &[u8]) -> Option<usize> {
        let n = match code {
            0b0001 => 192,
            0b0010 => 576,
            0b0011 => 1152,
            0b0100 => 2304,
            0b0101 => 4608,
            0b0110 => {
                let v = *data.get(*extra)? as usize;
                *extra += 1;
                v + 1
            }
            0b0111 => {
                let v = u16::from_be_bytes([*data.get(*extra)?, *data.get(*extra + 1)?]);
                *extra += 2;
                usize::from(v) + 1
            }
            0b1000..=0b1111 => 1_usize << code,
            _ => return None,
        };
        Some(n)
    }

    struct Decoded {
        channels: Vec<Vec<i32>>,
        bps: u8,
    }

    fn unzigzag(folded: u32) -> i32 {
        if folded & 1 == 0 {
            (folded >> 1) as i32
        } else {
            -((folded >> 1) as i32) - 1
        }
    }

    fn read_rice(reader: &mut Reader<'_>, param: u8) -> Option<i32> {
        let mut zeros = 0_u32;
        loop {
            if reader.get(1)? == 1 {
                break;
            }
            zeros += 1;
            if zeros > 10_000 {
                return None;
            }
        }
        let rest = if param == 0 { 0 } else { reader.get(param)? };
        Some(unzigzag((zeros << param) | rest))
    }

    fn read_residual(reader: &mut Reader<'_>, count: usize) -> Option<Vec<i32>> {
        let method = reader.get(2)?;
        if method != 0 {
            return None;
        }
        let order = reader.get(4)?;
        if order != 0 {
            return None;
        }
        let param = reader.get(4)?;
        let mut out = Vec::with_capacity(count);
        if param == 0b1111 {
            let width = reader.get(5)? as u8;
            for _ in 0..count {
                out.push(reader.get_signed(width)?);
            }
        } else {
            for _ in 0..count {
                out.push(read_rice(reader, param as u8)?);
            }
        }
        Some(out)
    }

    fn read_subframe(reader: &mut Reader<'_>, n: usize, bps: u8) -> Option<Vec<i32>> {
        if reader.get(1)? != 0 {
            return None;
        }
        let kind = reader.get(6)?;
        if reader.get(1)? != 0 {
            return None;
        }
        let mut samples = vec![0_i32; n];
        if kind == 0 {
            let sample = reader.get_signed(bps)?;
            samples.fill(sample);
            return Some(samples);
        }
        if kind == 1 {
            for sample in &mut samples {
                *sample = reader.get_signed(bps)?;
            }
            return Some(samples);
        }
        if (8..=12).contains(&kind) {
            let order = (kind - 8) as usize;
            for sample in samples.iter_mut().take(order) {
                *sample = reader.get_signed(bps)?;
            }
            let residuals = read_residual(reader, n - order)?;
            for (index, residual) in residuals.into_iter().enumerate() {
                let at = order + index;
                let prediction = predict(order, &samples, at)?;
                samples[at] = prediction.checked_add(residual)?;
            }
            return Some(samples);
        }
        None
    }

    pub(super) fn channels_of(data: &[u8]) -> Option<Vec<Vec<i32>>> {
        decode(data).map(|decoded| decoded.channels)
    }

    fn decode(data: &[u8]) -> Option<Decoded> {
        if data.len() < 8
            || crc16(&data[..data.len() - 2])
                != u16::from_be_bytes([data[data.len() - 2], data[data.len() - 1]])
        {
            return None;
        }
        if data[0] != 0xFF || data[1] != 0xF8 {
            return None;
        }
        let block_code = data[2] >> 4;
        let channel_code = data[3] >> 4;
        let bps = match (data[3] >> 1) & 0b111 {
            0b100 => 16,
            0b110 => 24,
            _ => return None,
        };
        if channel_code > 7 {
            return None;
        }
        let channels = usize::from(channel_code) + 1;
        let mut index = 4;
        let _frame = read_utf8(data, &mut index)?;
        let block = block_size(block_code, &mut index, data)?;
        let header_crc_at = index;
        index += 1;
        if crc8(&data[..header_crc_at]) != data[header_crc_at] {
            return None;
        }
        let body = &data[index..data.len() - 2];
        let mut reader = Reader { data: body, bit: 0 };
        let mut decoded = Vec::with_capacity(channels);
        for _ in 0..channels {
            decoded.push(read_subframe(&mut reader, block, bps)?);
        }
        let unread = body.len() * 8 - reader.bit;
        if unread >= 8 {
            return None;
        }
        if !reader.bit.is_multiple_of(8) {
            let byte = body[reader.bit / 8];
            let mask = (1_u8 << (8 - reader.bit % 8)) - 1;
            if byte & mask != 0 {
                return None;
            }
        }
        Some(Decoded {
            channels: decoded,
            bps,
        })
    }

    fn encode(channels: &[Vec<i32>], bps: u8) -> Vec<u8> {
        let n = channels[0].len();
        let mut planar = vec![0_i32; channels.len() * n];
        for (channel, samples) in channels.iter().enumerate() {
            planar[channel * n..channel * n + n].copy_from_slice(samples);
        }
        let mut scratch = Vec::new();
        let mut out = BitWriter::with_capacity(n * channels.len() * 3 + 64);
        encode_planar(
            &FrameIn {
                planar: &planar,
                channels: channels.len(),
                n,
                stride: n,
                bps,
                rate_hz: 48_000,
                frame_number: 0,
            },
            &mut scratch,
            &mut out,
        )
        .unwrap();
        out.bytes
    }

    #[test]
    fn silence_is_a_constant_frame_and_decodes_to_zero() {
        let frame = encode(&[vec![0; 4096], vec![0; 4096]], 16);
        assert_eq!(&frame[..5], &[0xFF, 0xF8, 0xCA, 0x18, 0x00]);
        let decoded = decode(&frame).unwrap();
        assert_eq!(decoded.bps, 16);
        assert!(decoded.channels.iter().all(|c| c.iter().all(|s| *s == 0)));
        assert!(frame.len() < 64, "{}", frame.len());
    }

    #[test]
    fn a_tone_round_trips_at_16_and_24_bit() {
        for bps in [16_u8, 24] {
            let scale = if bps == 16 { 32767.0 } else { 8_388_607.0 };
            let left: Vec<i32> = (0..4096)
                .map(|i| {
                    let s = (i as f32 * 0.07).sin() * 0.5;
                    (s * scale) as i32
                })
                .collect();
            let right: Vec<i32> = left.iter().map(|s| -s / 2).collect();
            let frame = encode(&[left.clone(), right.clone()], bps);
            let decoded = decode(&frame).unwrap();
            assert_eq!(decoded.channels[0], left);
            assert_eq!(decoded.channels[1], right);
            assert!(frame.len() < left.len() * 2 * 2, "{}", frame.len());
        }
    }

    #[test]
    fn a_short_last_block_round_trips() {
        let samples: Vec<i32> = (0..100).map(|i| i * 10).collect();
        let frame = encode(std::slice::from_ref(&samples), 16);
        assert_eq!(frame[1], 0xF8);
        let decoded = decode(&frame).unwrap();
        assert_eq!(decoded.channels[0], samples);
    }
}
