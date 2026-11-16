//! Sound conversion on the media worker: channel remap and resampling to the audio
//! graph's format (ADR-0108 decision 5). Runs on the worker thread only; it allocates.

use rbs_audio::resample::{AFTER, BEFORE, Kernel, TAPS, apply};
use rbs_core::timing::SampleRate;

/// Converts interleaved PCM of one source rate and channel count to interleaved PCM at
/// the graph's rate and channel count.
pub(crate) struct Converter {
    src_rate: u32,
    dst_rate: u32,
    channels: usize,
    kernel: Option<Kernel>,
    /// Remapped input, one buffer per output channel, starting `BEFORE` frames of
    /// history before the next output position.
    input: Vec<Vec<f32>>,
    /// Position of the next output frame in `input` frames.
    pos: f64,
    step: f64,
    frames_in: u64,
    frames_out: u64,
    weights: [f32; TAPS],
}

impl Converter {
    /// A converter from `src_rate` to `dst_rate` with `channels` output channels.
    pub(crate) fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let kernel = match (SampleRate::new(src_rate), SampleRate::new(dst_rate)) {
            (Some(from), Some(to)) if src_rate != dst_rate => Some(Kernel::new(from, to)),
            _ => None,
        };
        let input = (0..channels).map(|_| vec![0.0; BEFORE]).collect();
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
        for (channel, buffer) in self.input.iter_mut().enumerate() {
            let from = channel.min(src - 1);
            buffer.extend(samples.chunks_exact(src).map(|frame| frame[from]));
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
        for buffer in &mut self.input {
            buffer.extend(std::iter::repeat_n(0.0, TAPS));
        }
        self.run(out, Some(want));
    }

    fn run(&mut self, out: &mut Vec<f32>, limit: Option<u64>) {
        let Some(kernel) = self.kernel.as_ref() else {
            return;
        };
        let available = self.input.first().map_or(0, Vec::len);
        loop {
            if limit.is_some_and(|l| self.frames_out >= l) {
                break;
            }
            let i = self.pos as usize;
            if i + AFTER >= available {
                break;
            }
            kernel.coefficients(self.pos - i as f64, &mut self.weights);
            for buffer in &self.input {
                out.push(apply(&self.weights, &buffer[i - BEFORE..i - BEFORE + TAPS]));
            }
            self.frames_out += 1;
            self.pos += self.step;
        }
        // Keep `BEFORE` frames of history before the next position.
        let keep_from = (self.pos as usize).saturating_sub(BEFORE);
        if keep_from > 0 {
            for buffer in &mut self.input {
                buffer.drain(..keep_from.min(buffer.len()));
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
}
