//! What an audio encoder node lets through (ADR-0124 decisions 4 and 8).
//!
//! The gate is a type parameter of [`EncoderNode`](crate::EncoderNode), chosen when
//! the node is built: [`NoGate`] (the default: streams, the replay buffer) passes
//! every block as it is and compiles to nothing; [`PauseGate`] (a recording) leaves
//! out the samples inside a pause window, stamps later samples earlier by the time
//! left out, and fades 5 ms out before each cut and 5 ms in after it.

use std::sync::Arc;
use std::time::Duration;

use rbs_core::media::{AudioBlock, AudioFormat, AudioSamples, MAX_CHANNELS};
use rbs_core::pause::{PauseWindow, Window};
use rbs_core::time::Timestamp;

/// Length of the fade out before a cut and the fade in after it.
pub const PAUSE_FADE: Duration = Duration::from_millis(5);

/// Decides which samples of a block reach the encoder, and with which timestamps.
pub trait AudioGate: Send + 'static {
    /// Calls `submit` with each part of `block` to encode, in order. Never allocates.
    fn pass<E>(
        &mut self,
        block: &AudioBlock<'_>,
        submit: impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E>;
}

/// Passes every block unchanged.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoGate;

impl AudioGate for NoGate {
    #[inline(always)]
    fn pass<E>(
        &mut self,
        block: &AudioBlock<'_>,
        mut submit: impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        submit(block)
    }
}

/// Follows a recording's [`PauseWindow`].
///
/// Per block it pays one relaxed load (has the window changed?) and two compares
/// (is the block clear of the window and its fades?), then re-stamps the block by
/// the shift of the windows behind it. Only blocks that touch a window or its fades
/// are split or copied.
#[derive(Debug)]
pub struct PauseGate {
    pause: Arc<PauseWindow>,
    window: Window,
    /// Interleaved or planar copies of faded parts: [`MAX_CHANNELS`] channels of
    /// `frames` samples, reserved when the gate is built.
    scratch: Vec<f32>,
    frames: usize,
    /// Parts that were longer than the scratch buffer and passed without a fade.
    unfaded: u64,
}

impl PauseGate {
    /// A gate for `pause` that fades blocks of up to `frames` sample frames.
    pub fn new(pause: Arc<PauseWindow>, frames: usize) -> Self {
        Self {
            pause,
            window: Window::NONE,
            scratch: vec![0.0; frames * MAX_CHANNELS],
            frames,
            unfaded: 0,
        }
    }

    /// Parts passed without a fade because they were longer than the gate's blocks.
    pub fn unfaded(&self) -> u64 {
        self.unfaded
    }

    fn window(&mut self) -> Window {
        if self.pause.version() != self.window.epoch.wrapping_mul(2)
            && let Some(w) = self.pause.read()
        {
            self.window = w;
        }
        self.window
    }

    /// Submits `part` stamped earlier by `shift`, faded when it lies within
    /// [`PAUSE_FADE`] of the window's instants.
    fn submit_part<E>(
        &mut self,
        w: &Window,
        part: &AudioBlock<'_>,
        shift: u64,
        submit: &mut impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        if part.frames() == 0 {
            return Ok(());
        }
        let start = part.timestamp().as_nanos();
        let stamped = Timestamp::from_nanos(start.saturating_sub(shift));
        let end = start.saturating_add(nanos(part.format(), part.frames()));
        let fade = PAUSE_FADE.as_nanos() as u64;
        let fading_out =
            w.pause_at != u64::MAX && end > w.pause_at.saturating_sub(fade) && start < w.pause_at;
        let fading_in = w.pause_at != u64::MAX
            && w.resume_at != u64::MAX
            && start < w.resume_at.saturating_add(fade)
            && end > w.resume_at;
        if !(fading_out || fading_in) {
            return submit(&part.with_timestamp(stamped));
        }
        let channels = part.format().channels.channels();
        if part.frames() > self.frames {
            self.unfaded += 1;
            return submit(&part.with_timestamp(stamped));
        }
        let gain = |n: usize| {
            let t = start.saturating_add(nanos(part.format(), n));
            let mut g = 1.0_f32;
            if fading_out && t < w.pause_at {
                g = g.min((w.pause_at - t) as f32 / fade as f32);
            }
            if fading_in && t >= w.resume_at {
                g = g.min((t - w.resume_at) as f32 / fade as f32);
            }
            g.min(1.0)
        };
        let frames = part.frames();
        let scratch = &mut self.scratch[..frames * channels];
        match part.samples() {
            AudioSamples::Interleaved(samples) => {
                for (n, (out, input)) in scratch
                    .chunks_exact_mut(channels)
                    .zip(samples.chunks_exact(channels))
                    .enumerate()
                {
                    let g = gain(n);
                    for (o, i) in out.iter_mut().zip(input) {
                        *o = i * g;
                    }
                }
                let block = AudioBlock::interleaved(part.format(), stamped, scratch);
                match block {
                    Ok(block) => submit(&block),
                    Err(_) => submit(&part.with_timestamp(stamped)),
                }
            }
            AudioSamples::Planar(planar) => {
                for (out, input) in scratch.chunks_exact_mut(frames).zip(planar.iter()) {
                    for (n, (o, i)) in out.iter_mut().zip(input).enumerate() {
                        *o = i * gain(n);
                    }
                }
                let mut slices: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
                for (slot, chunk) in slices.iter_mut().zip(scratch.chunks_exact(frames)) {
                    *slot = chunk;
                }
                let block = AudioBlock::planar(part.format(), stamped, &slices[..channels]);
                match block {
                    Ok(block) => submit(&block),
                    Err(_) => submit(&part.with_timestamp(stamped)),
                }
            }
            #[allow(unreachable_patterns)] // `AudioSamples` is non-exhaustive.
            _ => submit(&part.with_timestamp(stamped)),
        }
    }
}

/// Nanoseconds of `frames` sample frames, as [`SampleRate::duration_of`] computes them.
///
/// [`SampleRate::duration_of`]: rbs_core::timing::SampleRate::duration_of
fn nanos(format: AudioFormat, frames: usize) -> u64 {
    format.sample_rate.duration_of(frames as u64).as_nanos() as u64
}

impl AudioGate for PauseGate {
    fn pass<E>(
        &mut self,
        block: &AudioBlock<'_>,
        mut submit: impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let w = self.window();
        let start = block.timestamp().as_nanos();
        let end = start.saturating_add(nanos(block.format(), block.frames()));
        let fade = PAUSE_FADE.as_nanos() as u64;
        // The run path: wholly before the next fade out, or wholly after the last
        // window's fade in.
        if end <= w.pause_at.saturating_sub(fade) {
            let t = Timestamp::from_nanos(start.saturating_sub(w.before));
            return submit(&block.with_timestamp(t));
        }
        if w.resume_at != u64::MAX && start >= w.resume_at.saturating_add(fade) {
            let t = Timestamp::from_nanos(start.saturating_sub(w.total()));
            return submit(&block.with_timestamp(t));
        }
        let cut = w.cut_block(start, block.frames(), block.format().sample_rate);
        self.submit_part(&w, &block.range(0, cut.head), cut.head_shift, &mut submit)?;
        self.submit_part(
            &w,
            &block.range(cut.tail, block.frames()),
            cut.tail_shift,
            &mut submit,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;
    use rbs_core::timing::SampleRate;

    const FRAMES: usize = 480;
    /// One 10 ms block in nanoseconds.
    const BLOCK: u64 = 10_000_000;

    /// One part as the encoder got it.
    #[derive(Debug)]
    struct Part {
        at: u64,
        left: Vec<f32>,
    }

    /// Runs `blocks` blocks of ones (left) and twos (right) from time 0 through
    /// `gate`, calling `between(k)` before block `k`.
    fn run(gate: &mut PauseGate, blocks: u64, mut between: impl FnMut(u64)) -> Vec<Part> {
        let left = [1.0_f32; FRAMES];
        let right = [2.0_f32; FRAMES];
        let mut parts = Vec::new();
        for k in 0..blocks {
            between(k);
            let block = AudioBlock::planar(
                AudioFormat::STEREO_48K,
                Timestamp::from_nanos(k * BLOCK),
                &[&left, &right],
            )
            .unwrap();
            gate.pass(&block, |part| {
                assert_eq!(part.channel(1).unwrap().len(), part.frames());
                parts.push(Part {
                    at: part.timestamp().as_nanos(),
                    left: part.channel(0).unwrap().to_vec(),
                });
                Ok::<(), ()>(())
            })
            .unwrap();
        }
        parts
    }

    /// Every part starts where the previous one ended, within one sample.
    fn assert_continuous(parts: &[Part]) {
        let rate = SampleRate::HZ_48000;
        let mut end = 0_u64;
        for part in parts {
            assert!(part.at.abs_diff(end) <= 20_834, "{} != {end}", part.at);
            end = part.at + rate.duration_of(part.left.len() as u64).as_nanos() as u64;
        }
    }

    fn kept(parts: &[Part]) -> usize {
        parts.iter().map(|p| p.left.len()).sum()
    }

    #[test]
    fn a_recording_never_paused_passes_every_block_unchanged() {
        let mut gate = PauseGate::new(Arc::new(PauseWindow::default()), FRAMES);
        let parts = run(&mut gate, 5, |_| {});
        assert_eq!(parts.len(), 5);
        for (k, part) in parts.iter().enumerate() {
            assert_eq!(part.at, k as u64 * BLOCK);
            assert!(part.left.iter().all(|&s| s == 1.0));
        }
    }

    #[test]
    fn a_pause_leaves_its_samples_out_and_closes_the_gap_with_fades() {
        let pause = Arc::new(PauseWindow::default());
        let mut gate = PauseGate::new(Arc::clone(&pause), FRAMES);
        // Pause 25 ms in (half way through block 2), resume at 62.5 ms (block 6).
        let parts = run(&mut gate, 10, |k| {
            if k == 0 {
                pause.pause(25_000_000).unwrap();
            }
            if k == 4 {
                pause.resume(62_500_000).unwrap();
            }
        });
        assert_continuous(&parts);
        // 100 ms of blocks minus 37.5 ms left out: 62.5 ms = 3000 samples.
        assert_eq!(kept(&parts), 3_000);
        let all: Vec<f32> = parts.iter().flat_map(|p| p.left.iter().copied()).collect();
        // 25 ms = sample 1200 is the cut; 5 ms = 240 samples fade on each side.
        assert!(all[..1200 - 240].iter().all(|&s| s == 1.0));
        assert!(all[1199] > 0.0 && all[1199] < 0.01, "{}", all[1199]);
        assert!(all[1200 - 120] > 0.45 && all[1200 - 120] < 0.55);
        assert_eq!(all[1200], 0.0, "the fade in starts from silence");
        assert!(all[1200 + 120] > 0.45 && all[1200 + 120] < 0.55);
        assert!(all[1200 + 240..].iter().all(|&s| s == 1.0));
    }

    #[test]
    fn two_pauses_add_up_and_a_cancelled_one_leaves_nothing_out() {
        let pause = Arc::new(PauseWindow::default());
        let mut gate = PauseGate::new(Arc::clone(&pause), FRAMES);
        let parts = run(&mut gate, 12, |k| match k {
            0 => {
                pause.pause(20_000_000).unwrap();
                pause.resume(30_000_000).unwrap();
            }
            4 => {
                pause.pause(60_000_000).unwrap();
            }
            5 => {
                pause.resume(80_000_000).unwrap();
            }
            8 => {
                pause.pause(110_000_000).unwrap();
            }
            9 => {
                pause.resume(100_000_000).unwrap(); // Before the pause: cancelled.
            }
            _ => {}
        });
        assert_continuous(&parts);
        // 120 ms of blocks minus 10 ms and 20 ms: 90 ms.
        assert_eq!(kept(&parts), 90 * 48);
        assert_eq!(gate.unfaded(), 0);
    }

    #[test]
    fn passing_does_not_allocate() {
        let pause = Arc::new(PauseWindow::default());
        let mut gate = PauseGate::new(Arc::clone(&pause), FRAMES);
        pause.pause(25_000_000).unwrap();
        pause.resume(42_000_000).unwrap();
        let left = [1.0_f32; FRAMES];
        let right = [2.0_f32; FRAMES];
        let counted = allocation_counter::measure(|| {
            for k in 0..10 {
                let block = AudioBlock::planar(
                    AudioFormat::STEREO_48K,
                    Timestamp::from_nanos(k * BLOCK),
                    &[&left, &right],
                )
                .unwrap();
                gate.pass(&block, |part| {
                    std::hint::black_box(part.frames());
                    Ok::<(), ()>(())
                })
                .unwrap();
            }
        });
        assert_eq!(counted.count_total, 0);
    }
}
