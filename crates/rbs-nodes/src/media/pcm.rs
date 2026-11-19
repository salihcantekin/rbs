//! Sound conversion on the media worker: channel remap and resampling to the audio
//! graph's format (ADR-0108 decision 5). Runs on the decode worker, not the audio
//! thread. The first block, and a larger block, may allocate; after that a sample
//! does not.

use std::sync::Arc;

use rbs_audio::resample::{AFTER, BEFORE, Kernel, TAPS, apply};
use rbs_core::timing::SampleRate;

/// One channel of input frames. Logical frame 0 is at `head`. Dropping history
/// advances `head`; it does not move samples.
struct Ring {
    buf: Vec<f32>,
    head: usize,
    len: usize,
}

impl Ring {
    /// `frames` leading zeros. The buffer is at least one slot so later indexes
    /// have a modulus.
    fn with_silence(frames: usize) -> Self {
        Self {
            buf: vec![0.0; frames.max(1)],
            head: 0,
            len: frames,
        }
    }

    /// Makes room for `extra` more frames. A short buffer is replaced once; a
    /// buffer that already fits is left alone.
    fn reserve_additional(&mut self, extra: usize) {
        let need = self.len.saturating_add(extra);
        if need <= self.buf.len() {
            return;
        }
        let mut cap = self.buf.len().max(1);
        while cap < need {
            let next = cap.saturating_mul(2);
            if next == cap {
                cap = need;
                break;
            }
            cap = next;
        }
        let mut buf = vec![0.0; cap];
        self.copy_linear(&mut buf[..self.len]);
        self.buf = buf;
        self.head = 0;
    }

    fn copy_linear(&self, dst: &mut [f32]) {
        let n = dst.len().min(self.len);
        if n == 0 || self.buf.is_empty() {
            return;
        }
        let cap = self.buf.len();
        let first = (cap - self.head).min(n);
        dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        if n > first {
            dst[first..n].copy_from_slice(&self.buf[..n - first]);
        }
    }

    /// Physical index of logical frame `logical`. `logical` is at most `len`, and
    /// `len` never exceeds the buffer, so the sum wraps at most once.
    fn phys(&self, logical: usize) -> usize {
        let cap = self.buf.len();
        let sum = self.head + logical;
        if sum >= cap { sum - cap } else { sum }
    }

    fn push_sample(&mut self, sample: f32) {
        let at = self.phys(self.len);
        self.buf[at] = sample;
        self.len += 1;
    }

    fn extend_channel(&mut self, samples: &[f32], src: usize, from: usize) {
        let frames = samples.len() / src;
        self.reserve_additional(frames);
        for frame in samples.chunks_exact(src) {
            self.push_sample(frame[from]);
        }
    }

    fn extend_silence(&mut self, n: usize) {
        self.reserve_additional(n);
        for _ in 0..n {
            self.push_sample(0.0);
        }
    }

    fn discard(&mut self, n: usize) {
        let n = n.min(self.len);
        if n == 0 || self.buf.is_empty() {
            return;
        }
        let cap = self.buf.len();
        self.head += n;
        if self.head >= cap {
            self.head -= cap;
        }
        self.len -= n;
    }

    /// The `TAPS` frames starting at logical `start`.
    fn dot(&self, weights: &[f32; TAPS], start: usize) -> f32 {
        let cap = self.buf.len();
        let at = self.phys(start);
        if at + TAPS <= cap {
            apply(weights, &self.buf[at..at + TAPS])
        } else {
            let mut tmp = [0.0f32; TAPS];
            let first = cap - at;
            tmp[..first].copy_from_slice(&self.buf[at..]);
            tmp[first..].copy_from_slice(&self.buf[..TAPS - first]);
            apply(weights, &tmp)
        }
    }
}

/// Converts interleaved PCM of one source rate and channel count to interleaved PCM at
/// the graph's rate and channel count.
pub(crate) struct Converter {
    src_rate: u32,
    dst_rate: u32,
    channels: usize,
    /// Shared with every other user of this rate pair (`Kernel::shared`). Absent when
    /// the rates match or a rate is not a sample rate, in which case samples are
    /// remapped only.
    kernel: Option<Arc<Kernel>>,
    /// Remapped input, one ring per output channel. Logical frame 0 is `BEFORE`
    /// frames before the next output position.
    input: Vec<Ring>,
    /// Position of the next output frame in `input` frames.
    pos: f64,
    step: f64,
    frames_in: u64,
    frames_out: u64,
    weights: [f32; TAPS],
}

impl Converter {
    /// A converter from `src_rate` to `dst_rate` with `channels` output channels.
    ///
    /// The rate pair's kernel is the process-wide shared table. Building it, when
    /// this pair is not already cached, happens here on the caller (the decode
    /// worker), once per converter.
    pub(crate) fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let kernel = match (SampleRate::new(src_rate), SampleRate::new(dst_rate)) {
            (Some(from), Some(to)) if src_rate != dst_rate => Some(Kernel::shared(from, to)),
            _ => None,
        };
        let input = (0..channels).map(|_| Ring::with_silence(BEFORE)).collect();
        Self {
            src_rate,
            dst_rate,
            channels,
            kernel,
            input,
            pos: BEFORE as f64,
            step: f64::from(src_rate.max(1)) / f64::from(dst_rate.max(1)),
            frames_in: 0,
            frames_out: 0,
            weights: [0.0; TAPS],
        }
    }

    /// The source rate this converter was made for.
    pub(crate) fn src_rate(&self) -> u32 {
        self.src_rate
    }

    /// Appends `samples` (interleaved, `src_channels` wide) and writes every output
    /// frame now possible to `out` (interleaved, the output channel count).
    pub(crate) fn push(&mut self, samples: &[f32], src_channels: u16, out: &mut Vec<f32>) {
        let src = usize::from(src_channels.max(1));
        let frames = samples.len() / src;
        if self.kernel.is_none() {
            out.reserve(frames * self.channels);
            for frame in samples.chunks_exact(src) {
                for channel in 0..self.channels {
                    out.push(frame[channel.min(src - 1)]);
                }
            }
            self.frames_in += frames as u64;
            self.frames_out += frames as u64;
            return;
        }
        for (channel, ring) in self.input.iter_mut().enumerate() {
            let from = channel.min(src - 1);
            ring.extend_channel(samples, src, from);
        }
        self.frames_in += frames as u64;
        self.run(out, None);
    }

    /// Ends the stream: writes the remaining output frames, as many as the input's
    /// length stands for at the output rate.
    pub(crate) fn finish(&mut self, out: &mut Vec<f32>) {
        if self.kernel.is_none() {
            return;
        }
        let want = self.frames_in * u64::from(self.dst_rate) / u64::from(self.src_rate.max(1));
        for ring in &mut self.input {
            ring.extend_silence(TAPS);
        }
        self.run(out, Some(want));
    }

    /// How many output frames `run` will emit for the current position.
    fn output_count(&self, available: usize, limit: Option<u64>) -> usize {
        let mut produced = self.frames_out;
        let mut pos = self.pos;
        let mut count = 0usize;
        loop {
            if limit.is_some_and(|l| produced >= l) {
                break;
            }
            let i = pos as usize;
            if i + AFTER >= available {
                break;
            }
            count += 1;
            produced += 1;
            pos += self.step;
        }
        count
    }

    fn run(&mut self, out: &mut Vec<f32>, limit: Option<u64>) {
        if self.kernel.is_none() {
            return;
        }
        let available = self.input.first().map_or(0, |ring| ring.len);
        if let Some(n) = self
            .output_count(available, limit)
            .checked_mul(self.channels)
        {
            out.reserve(n);
        }
        let Some(kernel) = self.kernel.as_ref() else {
            return;
        };
        loop {
            if limit.is_some_and(|l| self.frames_out >= l) {
                break;
            }
            let i = self.pos as usize;
            if i + AFTER >= available {
                break;
            }
            kernel.coefficients(self.pos - i as f64, &mut self.weights);
            let start = i - BEFORE;
            for ring in &self.input {
                out.push(ring.dot(&self.weights, start));
            }
            self.frames_out += 1;
            self.pos += self.step;
        }
        // Keep `BEFORE` frames of history before the next position, by index.
        let keep_from = (self.pos as usize).saturating_sub(BEFORE);
        if keep_from > 0 {
            for ring in &mut self.input {
                ring.discard(keep_from);
            }
            self.pos -= keep_from as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_same_rate_only_remaps_channels() {
        let mut c = Converter::new(48_000, 48_000, 2);
        let mut out = Vec::new();
        c.push(&[0.25, 0.5, 0.75], 1, &mut out);
        assert_eq!(out, [0.25, 0.25, 0.5, 0.5, 0.75, 0.75]);
        let mut out = Vec::new();
        let mut c = Converter::new(48_000, 48_000, 1);
        c.push(&[0.1, 0.2, 0.3, 0.4], 2, &mut out);
        assert_eq!(out, [0.1, 0.3]);
    }

    #[test]
    fn one_second_at_44_1_khz_becomes_one_second_at_48_khz() {
        let mut c = Converter::new(44_100, 48_000, 2);
        let mut out = Vec::new();
        // In uneven chunks, as a decoder hands them over.
        let input = vec![0.5f32; 44_100 * 2];
        for chunk in input.chunks(1_234 * 2) {
            c.push(chunk, 2, &mut out);
        }
        c.finish(&mut out);
        assert_eq!(out.len(), 48_000 * 2);
        // A constant stays constant away from the edges.
        for s in &out[2 * 1_000..2 * 47_000] {
            assert!((s - 0.5).abs() < 1e-3, "{s}");
        }
    }

    /// The history ring reads the same taps as a linear buffer that drops its prefix
    /// with `drain`. One push and a run of short chunks are both checked: those two
    /// shapes are not compared with each other, because subtracting the dropped
    /// prefix from the `f64` position is not the same bits as never subtracting it.
    fn history_ring_matches_a_drained_buffer(src_rate: u32, dst_rate: u32, chunks: &[&[f32]]) {
        let mut got = Converter::new(src_rate, dst_rate, 2);
        let mut expect = Drained::new(src_rate, dst_rate, 2);
        let mut out = Vec::new();
        let mut reference = Vec::new();
        for chunk in chunks {
            got.push(chunk, 2, &mut out);
            expect.push(chunk, &mut reference);
            assert_eq!(out, reference);
        }
        got.finish(&mut out);
        expect.finish(&mut reference);
        assert_eq!(out, reference);
    }

    #[test]
    fn the_history_ring_matches_the_drained_buffer() {
        let frames = 4_000;
        let mut stereo = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            stereo.push(((i as f32) * 0.01).sin());
            stereo.push(0.25);
        }
        for (src, dst) in [(44_100, 48_000), (48_000, 44_100)] {
            history_ring_matches_a_drained_buffer(src, dst, &[stereo.as_slice()]);
            let chunks: Vec<&[f32]> = stereo.chunks(34).collect();
            history_ring_matches_a_drained_buffer(src, dst, &chunks);
        }
    }

    /// Previous history buffer: one `Vec` per channel, prefix removed with `drain`.
    struct Drained {
        kernel: Kernel,
        input: Vec<Vec<f32>>,
        pos: f64,
        step: f64,
        frames_in: u64,
        frames_out: u64,
        weights: [f32; TAPS],
        dst_rate: u32,
        src_rate: u32,
    }

    impl Drained {
        fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
            let from = SampleRate::new(src_rate).unwrap();
            let to = SampleRate::new(dst_rate).unwrap();
            Self {
                kernel: Kernel::new(from, to),
                input: (0..channels).map(|_| vec![0.0; BEFORE]).collect(),
                pos: BEFORE as f64,
                step: f64::from(src_rate) / f64::from(dst_rate),
                frames_in: 0,
                frames_out: 0,
                weights: [0.0; TAPS],
                dst_rate,
                src_rate,
            }
        }

        fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) {
            let frames = samples.len() / 2;
            for (channel, buffer) in self.input.iter_mut().enumerate() {
                buffer.extend(samples.chunks_exact(2).map(|frame| frame[channel]));
            }
            self.frames_in += frames as u64;
            self.run(out, None);
        }

        fn finish(&mut self, out: &mut Vec<f32>) {
            let want = self.frames_in * u64::from(self.dst_rate) / u64::from(self.src_rate);
            for buffer in &mut self.input {
                buffer.extend(std::iter::repeat_n(0.0, TAPS));
            }
            self.run(out, Some(want));
        }

        fn run(&mut self, out: &mut Vec<f32>, limit: Option<u64>) {
            let available = self.input.first().map_or(0, Vec::len);
            loop {
                if limit.is_some_and(|l| self.frames_out >= l) {
                    break;
                }
                let i = self.pos as usize;
                if i + AFTER >= available {
                    break;
                }
                self.kernel
                    .coefficients(self.pos - i as f64, &mut self.weights);
                for buffer in &self.input {
                    out.push(apply(&self.weights, &buffer[i - BEFORE..i - BEFORE + TAPS]));
                }
                self.frames_out += 1;
                self.pos += self.step;
            }
            let keep_from = (self.pos as usize).saturating_sub(BEFORE);
            if keep_from > 0 {
                for buffer in &mut self.input {
                    buffer.drain(..keep_from.min(buffer.len()));
                }
                self.pos -= keep_from as f64;
            }
        }
    }

    #[test]
    fn a_sample_after_the_first_block_does_not_grow_capacity() {
        let mut c = Converter::new(44_100, 48_000, 1);
        let mut out = Vec::new();
        let burst = vec![0.2f32; 4_096];
        c.push(&burst, 1, &mut out);
        out.clear();
        let input_cap = c.input.first().map(|ring| ring.buf.len()).unwrap_or(0);
        let out_cap = out.capacity();
        assert!(input_cap > 0);
        for _ in 0..300 {
            out.clear();
            c.push(&[0.2], 1, &mut out);
            let cap = c.input.first().map(|ring| ring.buf.len()).unwrap_or(0);
            assert_eq!(cap, input_cap);
            assert_eq!(out.capacity(), out_cap);
        }
    }

    #[test]
    fn converters_of_one_rate_pair_share_the_kernel() {
        let a = Converter::new(44_100, 48_000, 1);
        let b = Converter::new(44_100, 48_000, 2);
        let other = Converter::new(48_000, 44_100, 1);
        let same = Converter::new(48_000, 48_000, 1);
        assert!(Arc::ptr_eq(
            a.kernel.as_ref().unwrap(),
            b.kernel.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(
            a.kernel.as_ref().unwrap(),
            other.kernel.as_ref().unwrap()
        ));
        assert!(same.kernel.is_none());
    }
}
