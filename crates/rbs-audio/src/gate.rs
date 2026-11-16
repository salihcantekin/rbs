//! What an audio encoder node lets through (ADR-0124 decisions 4 and 8).
//!
//! The gate is a type parameter of [`EncoderNode`](crate::EncoderNode), chosen when
//! the node is built: [`NoGate`] (the default: streams, the replay buffer) passes
//! every block as it is and compiles to nothing; [`PauseGate`] (a recording) leaves
//! out the samples inside a pause window, stamps later samples earlier by the time
//! left out, and fades 5 ms out before each cut and 5 ms in after it. [`HoldGate`]
//! (a stream) sends silence with the block's own timestamps while its stream is held
//! (ADR-0124 decisions 12 and 13); a [`PauseGate`] with a hold switch does the same
//! for a chain a recording shares with a stream.

use std::sync::Arc;
use std::time::Duration;

use rbs_core::media::{AudioBlock, AudioFormat, AudioSamples, MAX_CHANNELS};
use rbs_core::pause::{HoldPicture, HoldSwitch, PauseWindow, Window};
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

    /// Whether the encoder node reads its host-only "hold" input and hands its blocks
    /// to [`AudioGate::pass_hold`]. `false` ([`NoGate`]) compiles the read away;
    /// [`HoldGate`] and [`PauseGate`] set it.
    const HOLDS: bool = false;

    /// Whether this gate takes the "hold" input's blocks; read only when
    /// [`AudioGate::HOLDS`] is true. A [`PauseGate`] built without a hold says no, so
    /// a chain only recordings use reads nothing more.
    #[inline(always)]
    fn holds(&self) -> bool {
        Self::HOLDS
    }

    /// A block of the hold scene's mixer (ADR-0124 phase 2b), as it arrives: calls
    /// `submit` with the parts to encode in place of the program's. Never allocates.
    fn pass_hold<E>(
        &mut self,
        hold: &AudioBlock<'_>,
        submit: impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let _ = (hold, submit);
        Ok(())
    }
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
    cut: PauseCut,
    /// The hold of a stream sharing the chain (ADR-0124 decision 14), with the hold
    /// scene's sound (phase 2b); `None` for a chain only recordings use.
    hold: Option<HoldGate>,
}

/// What [`PauseGate`] cuts by its window.
#[derive(Debug)]
struct PauseCut {
    pause: Arc<PauseWindow>,
    window: Window,
    /// Interleaved or planar copies of faded parts: [`MAX_CHANNELS`] channels of
    /// `frames` samples, reserved when the gate is built.
    scratch: Vec<f32>,
    frames: usize,
    /// Parts that were longer than the scratch buffer and passed without a fade.
    unfaded: u64,
}

/// A hold switch and the silence sent while it is on: `frames` zero samples, every
/// channel's plane, reserved when the gate is built.
#[derive(Debug)]
struct Silence {
    hold: Arc<HoldSwitch>,
    zeros: Arc<[f32]>,
    frames: usize,
}

impl Silence {
    fn new(hold: Arc<HoldSwitch>, frames: usize) -> Self {
        let frames = frames.max(1);
        Self {
            hold,
            zeros: vec![0.0; frames].into(),
            frames,
        }
    }

    /// Calls `submit` with silence as long as `block`, in parts of at most the
    /// reserved length, each stamped where its samples start.
    fn pass<E>(
        zeros: &[f32],
        frames: usize,
        block: &AudioBlock<'_>,
        submit: &mut impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let format = block.format();
        let channels = format.channels.channels();
        let mut done = 0;
        while done < block.frames() {
            let n = (block.frames() - done).min(frames);
            let at = block
                .timestamp()
                .as_nanos()
                .saturating_add(nanos(format, done));
            let planes: [&[f32]; MAX_CHANNELS] = [&zeros[..n]; MAX_CHANNELS];
            match AudioBlock::planar(format, Timestamp::from_nanos(at), &planes[..channels]) {
                Ok(part) => submit(&part)?,
                // A format the zeros cannot carry: the block as it is, never a gap.
                Err(_) => return submit(&block.range(done, block.frames())),
            }
            done += n;
        }
        Ok(())
    }
}

/// Sends silence while its stream is held, and every block unchanged otherwise
/// (ADR-0124 decisions 12 and 13). Per block it pays one relaxed load and, not held,
/// one compare (is a scene hold's sound still to be closed?).
///
/// Held with [`HoldPicture::Scene`] (phase 2b) it sends the hold scene's blocks
/// ([`AudioGate::pass_hold`]) in place of the program's, on one timeline: `cursor` is
/// where the next part starts. The two mixers tick together, but each queue passes
/// one block a tick, so a scene block may reach the encoder up to a block after the
/// program's block of the same time. A program block that starts past the cursor
/// means the scene fell more than a block behind (or has no sound): silence fills to
/// it. A scene block, or the part of one, before the cursor is dropped. Leaving the
/// scene fills to the next program block, so there is no gap or overlap on either
/// edge; the scene's sound runs at most one block later than the program's would.
#[derive(Debug)]
pub struct HoldGate {
    silence: Silence,
    /// Nanoseconds where the next part starts while the scene's sound is sent;
    /// `None` otherwise.
    cursor: Option<u64>,
}

impl HoldGate {
    /// True while held or while a scene's sound is still to be closed: one relaxed
    /// load and, not held, one compare.
    #[inline(always)]
    fn engaged(&self) -> bool {
        self.silence.hold.is_held() || self.cursor.is_some()
    }

    /// A gate for `hold` whose silence covers blocks of up to `frames` sample frames
    /// in one part (longer blocks go in several).
    pub fn new(hold: Arc<HoldSwitch>, frames: usize) -> Self {
        Self {
            silence: Silence::new(hold, frames),
            cursor: None,
        }
    }

    /// Silence from `from` (nanoseconds) up to `to`, in `like`'s format, in parts no
    /// longer than `like`.
    fn fill<E>(
        &self,
        from: u64,
        to: u64,
        like: &AudioBlock<'_>,
        submit: &mut impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut at = from;
        while at < to {
            let n = frames_in(like.format(), to - at).min(like.frames());
            if n == 0 {
                break;
            }
            let part = like.range(0, n).with_timestamp(Timestamp::from_nanos(at));
            Silence::pass(&self.silence.zeros, self.silence.frames, &part, submit)?;
            at = at.saturating_add(nanos(like.format(), n));
        }
        Ok(())
    }

    /// Ends a scene's sound: silence up to `block`, which then goes out as the hold
    /// now says. Off the run path (only after a scene hold).
    #[cold]
    fn close_scene<E>(
        &mut self,
        cursor: u64,
        block: &AudioBlock<'_>,
        submit: &mut impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let start = block.timestamp().as_nanos();
        self.fill(cursor, start, block, submit)?;
        let from = if cursor > start {
            // Scene parts already went out past this block's start.
            frames_in(block.format(), cursor - start).min(block.frames())
        } else {
            0
        };
        let rest = block.range(from, block.frames());
        if rest.frames() == 0 {
            return Ok(());
        }
        if self.silence.hold.is_held() {
            Silence::pass(&self.silence.zeros, self.silence.frames, &rest, submit)
        } else {
            submit(&rest)
        }
    }
}

/// Whole sample frames in `ns` nanoseconds at `format`'s rate, rounded to nearest.
fn frames_in(format: AudioFormat, ns: u64) -> usize {
    let hz = u128::from(format.sample_rate.hz());
    ((u128::from(ns) * hz + 500_000_000) / 1_000_000_000) as usize
}

impl AudioGate for HoldGate {
    const HOLDS: bool = true;

    #[inline]
    fn pass<E>(
        &mut self,
        block: &AudioBlock<'_>,
        mut submit: impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        match self.silence.hold.held() {
            None if self.cursor.is_none() => submit(block),
            Some(HoldPicture::Scene) => {
                let start = block.timestamp().as_nanos();
                match self.cursor {
                    // The scene's first block: its sound starts with this block's time.
                    None => self.cursor = Some(start),
                    // The scene is more than a block behind: silence up to this block.
                    Some(c) if start > c => {
                        self.fill(c, start, block, &mut submit)?;
                        self.cursor = Some(start);
                    }
                    Some(_) => {}
                }
                // The program's block is not sent.
                Ok(())
            }
            _ => match self.cursor.take() {
                Some(c) => self.close_scene(c, block, &mut submit),
                None => Silence::pass(&self.silence.zeros, self.silence.frames, block, &mut submit),
            },
        }
    }

    fn pass_hold<E>(
        &mut self,
        hold: &AudioBlock<'_>,
        mut submit: impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        // Only while the scene's timeline is open: from the scene's first program
        // block until the next program block after it (so the block a late scene
        // sends just after a release or a change still goes out).
        let Some(c) = self.cursor else {
            return Ok(());
        };
        let start = hold.timestamp().as_nanos();
        let end = start.saturating_add(nanos(hold.format(), hold.frames()));
        if end <= c {
            return Ok(());
        }
        // A scene block lost on its way: silence over the gap.
        self.fill(c, start, hold, &mut submit)?;
        let part = if start < c {
            hold.range(frames_in(hold.format(), c - start), hold.frames())
        } else {
            *hold
        };
        submit(&part)?;
        self.cursor = Some(end);
        Ok(())
    }
}

impl PauseGate {
    /// A gate for `pause` that fades blocks of up to `frames` sample frames.
    pub fn new(pause: Arc<PauseWindow>, frames: usize) -> Self {
        Self {
            cut: PauseCut {
                pause,
                window: Window::NONE,
                scratch: vec![0.0; frames * MAX_CHANNELS],
                frames,
                unfaded: 0,
            },
            hold: None,
        }
    }

    /// The same gate also following `hold`: for a chain a recording shares with a
    /// stream (ADR-0124 decision 14). Held, it sends silence, or the hold scene's
    /// blocks ([`AudioGate::pass_hold`]) as [`HoldGate`] does (phase 2b), each part
    /// then cut by the window as the program's would be. One more relaxed load and
    /// one compare per block.
    #[must_use]
    pub fn with_hold(mut self, hold: Arc<HoldSwitch>) -> Self {
        self.hold = Some(HoldGate::new(hold, self.cut.frames));
        self
    }

    /// Parts passed without a fade because they were longer than the gate's blocks.
    pub fn unfaded(&self) -> u64 {
        self.cut.unfaded
    }
}

impl PauseCut {
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
    const HOLDS: bool = true;

    #[inline(always)]
    fn holds(&self) -> bool {
        self.hold.is_some()
    }

    fn pass<E>(
        &mut self,
        block: &AudioBlock<'_>,
        mut submit: impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        // Held: silence or the scene's sound in place of the block, then cut and
        // stamped as the block would have been.
        if let Some(hold) = &mut self.hold
            && hold.engaged()
        {
            let cut = &mut self.cut;
            return hold.pass(block, |part| cut.pass_unheld(part, &mut submit));
        }
        self.cut.pass_unheld(block, &mut submit)
    }

    fn pass_hold<E>(
        &mut self,
        hold: &AudioBlock<'_>,
        mut submit: impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let Some(gate) = &mut self.hold else {
            return Ok(());
        };
        let cut = &mut self.cut;
        gate.pass_hold(hold, |part| cut.pass_unheld(part, &mut submit))
    }
}

impl PauseCut {
    fn pass_unheld<E>(
        &mut self,
        block: &AudioBlock<'_>,
        submit: &mut impl FnMut(&AudioBlock<'_>) -> Result<(), E>,
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
        self.submit_part(&w, &block.range(0, cut.head), cut.head_shift, submit)?;
        self.submit_part(
            &w,
            &block.range(cut.tail, block.frames()),
            cut.tail_shift,
            submit,
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

    /// Runs `blocks` blocks of ones and twos through any gate.
    fn run_any<G: AudioGate>(gate: &mut G, blocks: u64, mut between: impl FnMut(u64)) -> Vec<Part> {
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
                assert_eq!(part.frames(), part.channel(1).unwrap().len());
                assert!(
                    part.channel(1)
                        .unwrap()
                        .iter()
                        .all(|s| *s == 0.0 || *s == 2.0)
                );
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

    #[test]
    fn a_held_stream_sends_silence_with_running_timestamps() {
        use rbs_core::pause::{HoldPicture, HoldSwitch};
        let hold = Arc::new(HoldSwitch::default());
        // Half the block length: a held block goes in two silent parts.
        let mut gate = HoldGate::new(Arc::clone(&hold), FRAMES / 2);
        let parts = run_any(&mut gate, 6, |k| match k {
            2 => hold.hold(HoldPicture::Freeze),
            4 => hold.release(),
            _ => {}
        });
        assert_continuous(&parts);
        let silent: Vec<bool> = parts
            .iter()
            .map(|p| p.left.iter().all(|s| *s == 0.0))
            .collect();
        assert_eq!(silent, [false, false, true, true, true, true, false, false]);
        assert_eq!(parts.last().unwrap().at, 5 * BLOCK);
        assert!(
            parts
                .iter()
                .all(|p| p.left.iter().all(|s| *s == 0.0 || *s == 1.0))
        );
    }

    #[test]
    fn a_recording_chain_held_by_a_stream_sharing_it_sends_silence() {
        use rbs_core::pause::{HoldPicture, HoldSwitch};
        let hold = Arc::new(HoldSwitch::default());
        let mut gate =
            PauseGate::new(Arc::new(PauseWindow::default()), FRAMES).with_hold(Arc::clone(&hold));
        let parts = run_any(&mut gate, 4, |k| match k {
            1 => hold.hold(HoldPicture::Black),
            3 => hold.release(),
            _ => {}
        });
        assert_continuous(&parts);
        let silent: Vec<bool> = parts
            .iter()
            .map(|p| p.left.iter().all(|s| *s == 0.0))
            .collect();
        assert_eq!(silent, [false, true, true, false]);
    }

    // ---- Hold scene (ADR-0124 phase 2b) ---------------------------------------------

    fn block_at<'a>(k: u64, left: &'a [f32], right: &'a [f32]) -> AudioBlock<'a> {
        AudioBlock::planar(
            AudioFormat::STEREO_48K,
            Timestamp::from_nanos(k * BLOCK),
            &[left, right],
        )
        .unwrap()
    }

    /// Ticks `0..ticks` through a hold gate. On each tick the scene's block of
    /// `k - lag` (fives) arrives before the program's block `k` (ones), unless
    /// `scene(k)` says the scene sends nothing; `between(k)` runs first.
    fn run_held<G: AudioGate>(
        gate: &mut G,
        ticks: u64,
        lag: u64,
        scene: impl Fn(u64) -> bool,
        mut between: impl FnMut(u64),
    ) -> Vec<Part> {
        let ones = [1.0_f32; FRAMES];
        let fives = [5.0_f32; FRAMES];
        let mut parts = Vec::new();
        let mut keep = |part: &AudioBlock<'_>| {
            parts.push(Part {
                at: part.timestamp().as_nanos(),
                left: part.channel(0).unwrap().to_vec(),
            });
            Ok::<(), ()>(())
        };
        for k in 0..ticks {
            between(k);
            if k >= lag && scene(k - lag) {
                gate.pass_hold(&block_at(k - lag, &fives, &fives), &mut keep)
                    .unwrap();
            }
            gate.pass(&block_at(k, &ones, &ones), &mut keep).unwrap();
        }
        parts
    }

    /// Every part starts exactly where the previous one ended.
    fn assert_exact(parts: &[Part]) {
        let rate = SampleRate::HZ_48000;
        let mut end = 0_u64;
        for part in parts {
            assert!(
                part.at.abs_diff(end) <= 1,
                "{} != {end}: {parts:?}",
                part.at
            );
            end = part.at + rate.duration_of(part.left.len() as u64).as_nanos() as u64;
        }
    }

    /// The value of the samples at `at` nanoseconds.
    fn value_at(parts: &[Part], at: u64) -> f32 {
        let p = parts.iter().rfind(|p| p.at <= at).unwrap();
        p.left[0]
    }

    #[test]
    fn a_scene_hold_sends_the_scene_on_one_timeline_with_either_lag() {
        for lag in [0, 1] {
            let hold = Arc::new(HoldSwitch::default());
            let mut gate = HoldGate::new(Arc::clone(&hold), FRAMES);
            let parts = run_held(
                &mut gate,
                20,
                lag,
                |_| true,
                |k| {
                    if k == 5 {
                        hold.hold(HoldPicture::Scene);
                    }
                    if k == 12 {
                        hold.release();
                    }
                },
            );
            assert_exact(&parts);
            assert_eq!(kept(&parts), 20 * FRAMES, "lag {lag}");
            // The program before, the scene (after at most one block of silence),
            // the program after.
            assert_eq!(value_at(&parts, 4 * BLOCK), 1.0);
            assert_eq!(value_at(&parts, 7 * BLOCK), 5.0, "lag {lag}");
            assert_eq!(value_at(&parts, 11 * BLOCK), 5.0, "lag {lag}");
            assert_eq!(value_at(&parts, 13 * BLOCK), 1.0);
            for p in &parts {
                let v = p.left[0];
                assert!(p.left.iter().all(|&s| s == v));
                if (5 * BLOCK..12 * BLOCK).contains(&p.at) {
                    assert!(v == 5.0 || v == 0.0, "lag {lag}: {parts:?}");
                }
            }
            assert!(gate.cursor.is_none());
        }
    }

    #[test]
    fn a_scene_that_falls_silent_is_filled_without_a_gap() {
        let hold = Arc::new(HoldSwitch::default());
        let mut gate = HoldGate::new(Arc::clone(&hold), FRAMES);
        // The scene's mixer sends blocks 0..8 only, a block late.
        let parts = run_held(
            &mut gate,
            16,
            1,
            |k| k < 8,
            |k| {
                if k == 3 {
                    hold.hold(HoldPicture::Scene);
                }
                if k == 13 {
                    hold.release();
                }
            },
        );
        assert_exact(&parts);
        assert_eq!(kept(&parts), 16 * FRAMES);
        assert_eq!(value_at(&parts, 5 * BLOCK), 5.0);
        assert_eq!(value_at(&parts, 10 * BLOCK), 0.0);
        assert_eq!(value_at(&parts, 14 * BLOCK), 1.0);
    }

    #[test]
    fn freeze_scene_freeze_and_release_keep_one_timeline() {
        let hold = Arc::new(HoldSwitch::default());
        let mut gate = HoldGate::new(Arc::clone(&hold), FRAMES);
        let parts = run_held(
            &mut gate,
            20,
            1,
            |_| true,
            |k| match k {
                3 => hold.hold(HoldPicture::Freeze),
                6 => hold.hold(HoldPicture::Scene),
                10 => hold.hold(HoldPicture::Black),
                14 => hold.release(),
                _ => {}
            },
        );
        assert_exact(&parts);
        assert_eq!(kept(&parts), 20 * FRAMES);
        assert_eq!(value_at(&parts, 4 * BLOCK), 0.0);
        assert_eq!(value_at(&parts, 8 * BLOCK), 5.0);
        assert_eq!(value_at(&parts, 12 * BLOCK), 0.0);
        assert_eq!(value_at(&parts, 15 * BLOCK), 1.0);
    }

    #[test]
    fn scene_blocks_are_ignored_unless_the_scene_is_held() {
        let hold = Arc::new(HoldSwitch::default());
        let mut gate = HoldGate::new(Arc::clone(&hold), FRAMES);
        let parts = run_held(
            &mut gate,
            6,
            0,
            |_| true,
            |k| {
                if k == 2 {
                    hold.hold(HoldPicture::Freeze);
                }
            },
        );
        assert_exact(&parts);
        assert_eq!(kept(&parts), 6 * FRAMES);
        assert!(parts.iter().all(|p| p.left[0] != 5.0));
        assert!(gate.cursor.is_none());
    }

    #[test]
    fn a_scene_hold_does_not_allocate() {
        let hold = Arc::new(HoldSwitch::default());
        let mut gate = HoldGate::new(Arc::clone(&hold), FRAMES);
        let ones = [1.0_f32; FRAMES];
        let fives = [5.0_f32; FRAMES];
        let counted = allocation_counter::measure(|| {
            for k in 0..12 {
                match k {
                    2 => hold.hold(HoldPicture::Scene),
                    6 => hold.hold(HoldPicture::Freeze),
                    7 => hold.hold(HoldPicture::Scene),
                    10 => hold.release(),
                    _ => {}
                }
                let sink = |part: &AudioBlock<'_>| {
                    std::hint::black_box(part.frames());
                    Ok::<(), ()>(())
                };
                if k > 0 && k != 5 {
                    gate.pass_hold(&block_at(k - 1, &fives, &fives), sink)
                        .unwrap();
                }
                gate.pass(&block_at(k, &ones, &ones), sink).unwrap();
            }
        });
        assert_eq!(counted.count_total, 0);
    }

    #[test]
    fn a_recordings_shared_chain_sends_the_scene_on_one_timeline_with_either_lag() {
        for lag in [0, 1] {
            let hold = Arc::new(HoldSwitch::default());
            let mut gate = PauseGate::new(Arc::new(PauseWindow::default()), FRAMES)
                .with_hold(Arc::clone(&hold));
            assert!(gate.holds());
            let parts = run_held(
                &mut gate,
                20,
                lag,
                |_| true,
                |k| match k {
                    3 => hold.hold(HoldPicture::Freeze),
                    5 => hold.hold(HoldPicture::Scene),
                    12 => hold.release(),
                    _ => {}
                },
            );
            assert_exact(&parts);
            assert_eq!(kept(&parts), 20 * FRAMES, "lag {lag}");
            assert_eq!(value_at(&parts, 2 * BLOCK), 1.0);
            assert_eq!(value_at(&parts, 4 * BLOCK), 0.0);
            assert_eq!(value_at(&parts, 7 * BLOCK), 5.0, "lag {lag}");
            assert_eq!(value_at(&parts, 11 * BLOCK), 5.0, "lag {lag}");
            assert_eq!(value_at(&parts, 13 * BLOCK), 1.0);
            assert!(gate.hold.as_ref().is_some_and(|h| h.cursor.is_none()));
        }
    }

    #[test]
    fn a_pause_gate_without_a_hold_takes_no_scene_blocks() {
        let mut gate = PauseGate::new(Arc::new(PauseWindow::default()), FRAMES);
        assert!(!gate.holds());
        let parts = run_held(&mut gate, 4, 0, |_| true, |_| {});
        assert_exact(&parts);
        assert_eq!(kept(&parts), 4 * FRAMES);
        assert!(parts.iter().all(|p| p.left[0] == 1.0));
    }

    #[test]
    fn a_shared_chains_scene_hold_does_not_allocate() {
        let hold = Arc::new(HoldSwitch::default());
        let mut gate =
            PauseGate::new(Arc::new(PauseWindow::default()), FRAMES).with_hold(Arc::clone(&hold));
        let ones = [1.0_f32; FRAMES];
        let fives = [5.0_f32; FRAMES];
        let counted = allocation_counter::measure(|| {
            for k in 0..12 {
                match k {
                    2 => hold.hold(HoldPicture::Scene),
                    6 => hold.hold(HoldPicture::Black),
                    7 => hold.hold(HoldPicture::Scene),
                    10 => hold.release(),
                    _ => {}
                }
                let sink = |part: &AudioBlock<'_>| {
                    std::hint::black_box(part.frames());
                    Ok::<(), ()>(())
                };
                if k > 0 && k != 5 {
                    gate.pass_hold(&block_at(k - 1, &fives, &fives), sink)
                        .unwrap();
                }
                gate.pass(&block_at(k, &ones, &ones), sink).unwrap();
            }
        });
        assert_eq!(counted.count_total, 0);
    }
}
