//! Noise suppression: RNNoise with a reduction limit and a speech-keyed gate
//! (ADR-0130 decision 5).
//!
//! RNNoise works on 480-sample frames of 48 kHz mono. The DSP collects input in a FIFO,
//! denoises every full frame and plays the result from an output queue that starts with
//! `480 - gcd(B, 480)` frames of silence for a block size `B`, so it never runs dry. With
//! RNNoise's own one-frame delay the latency is `960 - gcd(B, 480)` frames: 480 when `B`
//! is a multiple of 480. All buffers and the RNNoise states (one per channel of the graph
//! format) are made in `make`; `process`, `set` and `reset` allocate nothing.

use std::sync::Arc;

use rbs_core::media::MAX_CHANNELS;
use rbs_denoise::{Denoiser, FRAME_SIZE, SAMPLE_RATE};
use rbs_graph::node::NodeTypeId;
use rbs_params::{Field, ParamError, Values};

use crate::{AudioDsp, AudioFilterType, DspSetup, FilterMeter, amp_to_db, clean, db_to_amp};

/// Type id of [`NoiseSuppression`].
pub const NOISE_SUPPRESSION: &str = "org.rbs.audio-filter.noise-suppression";

/// RNNoise works on samples in the i16 range.
const SCALE: f32 = 32768.0;
/// How long the voice probability must stay under the threshold before the voice gate
/// closes, in frames (200 ms).
const GATE_HOLD_FRAMES: u32 = 20;

/// Index of each field (the order of [`FIELDS`]).
const MAX_REDUCTION: usize = 0;
const MODE: usize = 1;
const VOICE_GATE: usize = 2;
const VOICE_THRESHOLD: usize = 3;

/// `mode` options: one pass for all channels (index 0) or each channel (index 1).
const MODES: [(&str, &str); 2] = [("mono", "One pass"), ("separate", "Each channel")];

const FIELDS: [Field; 4] = [
    Field::number("max_reduction", "Max reduction", 6.0, 80.0, 30.0, 1.0, "dB").with_help(
        "The most the noise is lowered. Less keeps the voice natural; more removes more noise.",
    ),
    Field::choice("mode", "Channels", &MODES, "mono").with_help(
        "One pass suits a microphone. Each channel keeps a stereo source apart and costs more.",
    ),
    Field::toggle("voice_gate", "Mute between words", false)
        .with_help("Also lowers the sound by the max reduction while no speech is heard."),
    Field::number(
        "voice_threshold",
        "Speech threshold",
        0.0,
        100.0,
        60.0,
        1.0,
        "%",
    )
    .with_help("How sure the filter must be that someone speaks before the sound opens."),
];

/// The Noise suppression filter type.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoiseSuppression;

/// The latency for blocks of `block_frames`: `960 - gcd(B, 480)`.
fn latency_for(block_frames: u32) -> u32 {
    let frame = FRAME_SIZE as u32;
    let (mut a, mut b) = (block_frames.max(1), frame);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    2 * frame - a
}

impl AudioFilterType for NoiseSuppression {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new(NOISE_SUPPRESSION)
    }
    fn name(&self) -> &'static str {
        "Noise suppression"
    }
    fn fields(&self) -> &'static [Field] {
        &FIELDS
    }
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError> {
        Ok(Box::new(NoiseDsp::new(setup, values)?))
    }
    fn latency_frames(&self, setup: DspSetup) -> u32 {
        if setup.format.sample_rate.hz() == SAMPLE_RATE {
            latency_for(setup.block_frames)
        } else {
            0
        }
    }
}

/// A fixed-capacity ring of samples, made once.
#[derive(Debug)]
struct Ring {
    buf: Vec<f32>,
    head: usize,
    len: usize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0.0; capacity.max(1)],
            head: 0,
            len: 0,
        }
    }
    fn clear_with_silence(&mut self, silence: usize) {
        self.buf.fill(0.0);
        self.head = 0;
        self.len = silence.min(self.buf.len());
    }
    fn push(&mut self, x: f32) {
        if self.len < self.buf.len() {
            let at = (self.head + self.len) % self.buf.len();
            self.buf[at] = x;
            self.len += 1;
        }
    }
    /// The oldest sample, or silence when empty (cannot happen when sized as below).
    fn pop(&mut self) -> f32 {
        if self.len == 0 {
            return 0.0;
        }
        let x = self.buf[self.head];
        self.head = (self.head + 1) % self.buf.len();
        self.len -= 1;
        x
    }
}

/// The noise suppression settings in the units `process` uses.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Settings {
    max_reduction_db: f32,
    separate: bool,
    voice_gate: bool,
    threshold: f32,
}

impl Settings {
    fn of(v: &Values) -> Self {
        Self {
            max_reduction_db: v.number(MAX_REDUCTION).clamp(6.0, 80.0),
            separate: v.choice(MODE) == 1,
            voice_gate: v.toggle(VOICE_GATE),
            threshold: v.number(VOICE_THRESHOLD).clamp(0.0, 100.0) / 100.0,
        }
    }
}

/// The noise suppression DSP.
pub struct NoiseDsp {
    s: Settings,
    channels: usize,
    states: Vec<Denoiser>,
    /// Input collected toward the next frame, per state.
    frames_in: Vec<[f32; FRAME_SIZE]>,
    /// Denoised output waiting to be played, per channel.
    out: Vec<Ring>,
    /// Samples collected in `frames_in`.
    filled: usize,
    /// Silence the output queues start with: `480 - gcd(B, 480)`.
    prefill: usize,
    latency: u32,
    /// Voice gate: current gain, and frames since speech was last heard.
    gate_gain: f32,
    quiet_frames: u32,
    frame_out: [f32; FRAME_SIZE],
    meter: Arc<FilterMeter>,
}

impl NoiseDsp {
    /// A noise suppressor for `setup` with `values`. Refuses rates other than 48 kHz.
    pub fn new(setup: DspSetup, values: &Values) -> Result<Self, ParamError> {
        if setup.format.sample_rate.hz() != SAMPLE_RATE {
            return Err(ParamError::Unsupported(
                "noise suppression needs 48 kHz audio",
            ));
        }
        let channels = setup.format.channels.channels().clamp(1, MAX_CHANNELS);
        let s = Settings::of(values);
        let mut states = Vec::with_capacity(channels);
        for _ in 0..channels {
            let mut d = Denoiser::new()
                .map_err(|_| ParamError::Unsupported("the noise model is unavailable"))?;
            d.set_max_reduction_db(s.max_reduction_db);
            states.push(d);
        }
        let latency = latency_for(setup.block_frames);
        let prefill = latency as usize - FRAME_SIZE;
        let block = setup.block_frames as usize;
        let capacity = prefill + FRAME_SIZE + block + 1;
        let mut dsp = Self {
            s,
            channels,
            states,
            frames_in: vec![[0.0; FRAME_SIZE]; channels],
            out: (0..channels).map(|_| Ring::new(capacity)).collect(),
            filled: 0,
            prefill,
            latency,
            gate_gain: 1.0,
            quiet_frames: 0,
            frame_out: [0.0; FRAME_SIZE],
            meter: Arc::new(FilterMeter::default()),
        };
        dsp.reset();
        Ok(dsp)
    }

    /// Denoises the collected frame of every pass and queues the result.
    fn run_frame(&mut self) {
        let passes = if self.s.separate { self.channels } else { 1 };
        let mut voice = 0.0f32;
        let mut e_in = 0.0f32;
        let mut e_out = 0.0f32;
        let start_gain = self.gate_gain;
        for p in 0..passes {
            let (states, frames) = (&mut self.states, &self.frames_in);
            let (Some(state), Some(input)) = (states.get_mut(p), frames.get(p)) else {
                continue;
            };
            voice = voice.max(state.process_frame(&mut self.frame_out, input));
            e_in += input.iter().map(|x| x * x).sum::<f32>();
            e_out += self.frame_out.iter().map(|y| y * y).sum::<f32>();
            // The voice gate's gain for this frame is decided below; queue first and
            // apply it while queueing the frame of the last pass.
            if self.s.separate {
                if let Some(ring) = self.out.get_mut(p) {
                    for &y in &self.frame_out {
                        ring.push(y / SCALE);
                    }
                }
            }
        }

        // Voice gate: speech opens it at once (this frame's probability describes the
        // input a frame ahead of the output, so the opening leads the speech); it closes
        // after `GATE_HOLD_FRAMES` without speech.
        let target = if !self.s.voice_gate {
            1.0
        } else if voice >= self.s.threshold {
            self.quiet_frames = 0;
            1.0
        } else {
            self.quiet_frames = self.quiet_frames.saturating_add(1);
            if self.quiet_frames > GATE_HOLD_FRAMES {
                db_to_amp(-self.s.max_reduction_db)
            } else {
                1.0
            }
        };
        self.gate_gain = target;

        if !self.s.separate {
            // One pass: the same output on every channel, ramped across the frame.
            let step = (target - start_gain) / FRAME_SIZE as f32;
            for ring in &mut self.out {
                let mut g = start_gain;
                for &y in &self.frame_out {
                    g += step;
                    ring.push(clean(y / SCALE * g));
                }
            }
        } else if start_gain != 1.0 || target != 1.0 {
            // Each channel was queued unscaled; scale the frame just queued in place.
            let step = (target - start_gain) / FRAME_SIZE as f32;
            for ring in &mut self.out {
                let cap = ring.buf.len();
                let first = (ring.head + ring.len + cap - FRAME_SIZE) % cap;
                let mut g = start_gain;
                for i in 0..FRAME_SIZE {
                    g += step;
                    let at = (first + i) % cap;
                    ring.buf[at] = clean(ring.buf[at] * g);
                }
            }
        }

        let reduction = if e_in > 1.0e-3 && e_out > 0.0 {
            (-10.0 * (e_out * target * target / e_in).log10()).max(0.0)
        } else {
            0.0
        };
        self.meter.store_voice(voice);
        self.meter.store(
            self.meter.input_peak_db(),
            if reduction.is_finite() {
                reduction
            } else {
                0.0
            },
        );
    }
}

impl AudioDsp for NoiseDsp {
    fn set(&mut self, values: &Values) {
        let new = Settings::of(values);
        let mode_changed = new.separate != self.s.separate;
        if new.max_reduction_db != self.s.max_reduction_db {
            for d in &mut self.states {
                d.set_max_reduction_db(new.max_reduction_db);
            }
        }
        self.s = new;
        if mode_changed {
            // The second state was not fed in mono mode: start every state clean
            // (about 10 ms of silence while they converge; ADR-0130 decision 5).
            self.reset();
        }
    }

    fn reset(&mut self) {
        for d in &mut self.states {
            d.reset();
        }
        for f in &mut self.frames_in {
            f.fill(0.0);
        }
        for r in &mut self.out {
            r.clear_with_silence(self.prefill);
        }
        self.filled = 0;
        self.gate_gain = 1.0;
        self.quiet_frames = 0;
    }

    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        let n = channels.len().min(self.channels);
        if n == 0 {
            return;
        }
        let mut peak = 0.0f32;
        // Pass 1: collect the whole block, denoising every frame it completes. Reading
        // all input before writing any output keeps the queues as sized in `new`.
        for f in 0..frames {
            if self.s.separate {
                for (ch, frame) in channels.iter().take(n).zip(&mut self.frames_in) {
                    let x = ch.get(f).copied().unwrap_or(0.0);
                    let x = if x.is_finite() { x } else { 0.0 };
                    peak = peak.max(x.abs());
                    frame[self.filled] = x * SCALE;
                }
            } else {
                let mut sum = 0.0f32;
                for ch in channels.iter().take(n) {
                    let x = ch.get(f).copied().unwrap_or(0.0);
                    let x = if x.is_finite() { x } else { 0.0 };
                    peak = peak.max(x.abs());
                    sum += x;
                }
                self.frames_in[0][self.filled] = sum / n as f32 * SCALE;
            }
            self.filled += 1;
            if self.filled == FRAME_SIZE {
                self.filled = 0;
                self.run_frame();
            }
        }
        // Pass 2: play the oldest queued samples.
        for (c, ch) in channels.iter_mut().enumerate().take(n) {
            let Some(ring) = self.out.get_mut(c) else {
                continue;
            };
            for slot in ch.iter_mut().take(frames) {
                *slot = clean(ring.pop());
            }
        }
        let reduction = self.meter.gain_reduction_db();
        self.meter.store(amp_to_db(peak), reduction);
    }

    fn meter(&self) -> Option<Arc<FilterMeter>> {
        Some(Arc::clone(&self.meter))
    }

    fn latency_frames(&self) -> u32 {
        self.latency
    }
}
