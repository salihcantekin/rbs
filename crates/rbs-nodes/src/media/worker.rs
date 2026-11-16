//! The decode worker of one media source (ADR-0108 decision 5).
//!
//! It owns the decoder. It paces each stream by master time: it decodes a stream only
//! while that stream's next stamp is at most [`LEAD`] ahead of now, and parks
//! otherwise. It never waits on a ring: a picture or block that does not fit is dropped
//! and counted. It stamps pictures and sound with master time from one anchor, set once
//! it holds the first picture, at now plus the lead, and keeps mapping forward across a
//! loop boundary so a loop leaves no timestamp gap.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rbs_audio::AudioGraphConfig;
use rbs_core::media::{
    ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::{Clock, Timestamp};
use rbs_gpu::yuv_decode::check_color;
use rbs_media_decode::{
    DecodeError, MAX_FRAME_BYTES, MediaDecoder, MediaDecoderFactory, PictureLayout, PictureSample,
    SampleColor, refuse_picture,
};

use super::pcm::Converter;
use super::picture::MediaNodeError;
use super::{LEAD, PICTURE_SLOTS, Shared, SoundBlock, SoundInlet};

/// Longest a worker parks before it looks at the clock again.
const PARK: Duration = Duration::from_millis(2);
/// A sound timestamp further than this from where the stream already is starts it
/// again at the stated time (a gap is filled with silence, an overlap is cut).
const SOUND_RESYNC: Duration = Duration::from_millis(50);
/// Sound chunks skipped per pass while catching up with a sound node attached late.
const CATCH_UP_CHUNKS: usize = 64;

/// One picture in flight, stamped with master time.
pub(crate) struct Pending {
    pub(crate) frame: VideoFrame,
    pub(crate) start: Timestamp,
    /// Media time of the picture, for the cursor.
    pub(crate) media: Duration,
    pub(crate) index: u32,
    pub(crate) mark: u8,
    pub(crate) bytes: usize,
}

/// What the worker hands the picture node once it holds the first picture.
pub(crate) struct Ready {
    pub(crate) pictures: rtrb::Consumer<Pending>,
    pub(crate) returns: rtrb::Producer<Pending>,
}

/// Counters the picture node reads.
#[derive(Default)]
pub(crate) struct Progress {
    pub(crate) decoder_held: AtomicBool,
    pub(crate) enqueued: AtomicU32,
    pub(crate) queued_frames: AtomicU32,
    pub(crate) queued_bytes: AtomicU64,
    pub(crate) failed: AtomicBool,
    /// The master time, in nanoseconds plus one, at which the worker last found
    /// nothing due: everything up to that time plus the lead is queued or dropped.
    pub(crate) settled_at: AtomicU64,
}

/// Everything one worker run needs.
pub(crate) struct Job {
    pub(crate) factory: Arc<dyn MediaDecoderFactory>,
    pub(crate) path: std::path::PathBuf,
    pub(crate) cursor: Duration,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: AudioGraphConfig,
    pub(crate) loop_flag: Arc<AtomicBool>,
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) epoch: u32,
    pub(crate) progress: Arc<Progress>,
    pub(crate) ready: mpsc::Sender<Result<Ready, MediaNodeError>>,
}

struct Released<'a>(&'a AtomicBool);

impl Drop for Released<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Runs one worker until the end of the file (without `loop`), a fault, or shutdown.
pub(crate) fn run(job: Job) {
    let mut decoder = match job.factory.open(&job.path) {
        Ok(decoder) => decoder,
        Err(error) => {
            let _ = job
                .ready
                .send(Err(MediaNodeError::Failed(error.to_string())));
            return;
        }
    };
    job.progress.decoder_held.store(true, Ordering::Release);
    let _released = Released(&job.progress.decoder_held);
    if let Err(error) = decoder.seek(job.cursor) {
        let _ = job
            .ready
            .send(Err(MediaNodeError::Failed(error.to_string())));
        return;
    }
    let has_audio = decoder.has_audio();
    job.shared.has_audio.store(has_audio, Ordering::Release);
    let mut pictures = PictureStream::new(job.cursor);
    let first = match pictures.first(&mut *decoder) {
        Ok(first) => first,
        Err(error) => {
            let _ = job.ready.send(Err(error));
            return;
        }
    };
    // The anchor is set before the picture node learns the worker is ready, so a
    // clock that moves after `wake` returns cannot move it.
    let anchor = job.clock.now() + LEAD;
    let (pictures_tx, pictures_rx) = rtrb::RingBuffer::new(PICTURE_SLOTS);
    let (returns_tx, returns_rx) = rtrb::RingBuffer::new(PICTURE_SLOTS + 2);
    if job
        .ready
        .send(Ok(Ready {
            pictures: pictures_rx,
            returns: returns_tx,
        }))
        .is_err()
    {
        return;
    }
    let mut run = Run {
        anchor,
        offset: Duration::ZERO,
        pictures,
        primed: Some(first),
        pictures_tx,
        returns_rx,
        index: 0,
        sound: SoundStream::new(&job.config, job.cursor),
        job: &job,
        decoder: &mut *decoder,
    };
    run.loop_until_done();
    job.shared.sound_from.store(0, Ordering::Release);
    if let Some(inlet) = run.sound.inlet.take() {
        job.shared.return_inlet(inlet);
    }
}

struct Run<'a> {
    job: &'a Job,
    decoder: &'a mut dyn MediaDecoder,
    /// Master time of the cursor's picture.
    anchor: Timestamp,
    /// Media time added by finished loop passes.
    offset: Duration,
    pictures: PictureStream,
    primed: Option<PictureSample>,
    pictures_tx: rtrb::Producer<Pending>,
    returns_rx: rtrb::Consumer<Pending>,
    index: u32,
    sound: SoundStream,
}

impl Run<'_> {
    /// Master time of media time `media` in the current pass, in nanoseconds.
    fn master(&self, media: Duration) -> i128 {
        self.anchor.as_nanos() as i128 - self.job.cursor.as_nanos() as i128
            + self.offset.as_nanos() as i128
            + media.as_nanos() as i128
    }

    fn base(&self) -> i128 {
        self.anchor.as_nanos() as i128 - self.job.cursor.as_nanos() as i128
    }

    fn loop_until_done(&mut self) {
        let job = self.job;
        loop {
            if job.shutdown.load(Ordering::Acquire) {
                return;
            }
            // Buffers the picture node handed back are freed here, off the render thread.
            while self.returns_rx.pop().is_ok() {}
            let now = job.clock.now();
            let horizon = now.as_nanos() as i128 + LEAD.as_nanos() as i128;
            let mut busy = false;

            // Pictures.
            if !self.pictures.done && self.master(self.pictures.next) <= horizon {
                busy = true;
                match self.next_picture() {
                    Ok(()) => {}
                    Err(()) => {
                        job.progress.failed.store(true, Ordering::Release);
                        return;
                    }
                }
            }

            // Sound.
            let attached = job.shared.sound_attached.load(Ordering::Acquire);
            let has_audio = job.shared.has_audio.load(Ordering::Acquire);
            if attached && has_audio {
                if self
                    .sound
                    .inlet
                    .as_ref()
                    .is_none_or(|i| i.blocks.is_abandoned())
                {
                    self.sound.inlet = job.shared.take_inlet();
                    self.sound.fill_spares(&job.config);
                }
                if !self.sound.done
                    && self.sound_next_master() <= horizon
                    && self.next_sound(now).is_err()
                {
                    job.progress.failed.store(true, Ordering::Release);
                    return;
                }
                busy |= self.cut_blocks(now, false);
            }

            // The end of the pass: both streams are done.
            let sound_done = !(attached && has_audio) || self.sound.done;
            if self.pictures.done && sound_done {
                if attached && has_audio {
                    self.finish_sound(now);
                }
                if !job.loop_flag.load(Ordering::Acquire) {
                    return;
                }
                let pass = self.pictures.pass_end;
                if self.decoder.seek(Duration::ZERO).is_err() {
                    job.progress.failed.store(true, Ordering::Release);
                    return;
                }
                // The next pass starts where this one's range ended.
                self.offset = self.offset.saturating_add(pass);
                self.pictures.restart();
                self.sound.restart();
                continue;
            }
            if !busy {
                job.progress
                    .settled_at
                    .store(now.as_nanos().saturating_add(1), Ordering::Release);
                let next = self.next_due().map_or(PARK, |due| {
                    let wait = due - horizon;
                    Duration::from_nanos(u64::try_from(wait.max(0)).unwrap_or(u64::MAX))
                });
                if !next.is_zero() {
                    thread::park_timeout(next.min(PARK));
                }
            }
        }
    }

    fn next_due(&self) -> Option<i128> {
        let mut due = None;
        if !self.pictures.done {
            due = Some(self.master(self.pictures.next));
        }
        if !self.sound.done && self.sound.inlet.is_some() {
            let s = self.sound_next_master();
            due = Some(due.map_or(s, |d: i128| d.min(s)));
        }
        due
    }

    fn next_picture(&mut self) -> Result<(), ()> {
        let job = self.job;
        let sample = match self.primed.take() {
            Some(sample) => sample,
            None => match self.pictures.pull(self.decoder) {
                Ok(Some(sample)) => sample,
                Ok(None) => {
                    self.pictures.done = true;
                    return Ok(());
                }
                Err(_) => return Err(()),
            },
        };
        let Some((start, _end)) = self.pictures.stamp(&sample) else {
            return Err(());
        };
        let picture = &sample.picture;
        if refuse_picture(picture.width, picture.height, picture.system_bytes()).is_some() {
            return Err(());
        }
        // Native surfaces land with the platform adapters (ADR-0108 decision 5).
        let Some(parts) = picture_parts(picture) else {
            return Err(());
        };
        let mark = parts.bytes.first().copied().unwrap_or(0);
        let bytes = parts.bytes.len();
        let index = self.index;
        self.index = self.index.wrapping_add(1);
        let start_master = to_timestamp(self.master(start));
        let fill = fill_count(bytes);
        let queued = job.progress.queued_frames.load(Ordering::Relaxed) as usize;
        let queued_bytes = job.progress.queued_bytes.load(Ordering::Relaxed) as usize;
        if queued >= fill || queued_bytes.saturating_add(bytes) > MAX_FRAME_BYTES {
            job.shared
                .stats
                .worker_dropped_pictures
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let Ok(frame) = system_frame(&parts, start_master) else {
            return Err(());
        };
        let item = Pending {
            frame,
            start: start_master,
            media: start,
            index,
            mark,
            bytes,
        };
        match self.pictures_tx.push(item) {
            Ok(()) => {
                job.progress.queued_frames.fetch_add(1, Ordering::Relaxed);
                job.progress
                    .queued_bytes
                    .fetch_add(bytes as u64, Ordering::Relaxed);
                job.progress.enqueued.fetch_add(1, Ordering::Release);
                job.shared.stats.uploaded.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                job.shared
                    .stats
                    .worker_dropped_pictures
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Master time of the sound frame the stream reaches next, in nanoseconds.
    fn sound_next_master(&self) -> i128 {
        self.base() + self.sound.frame_time(self.sound.end_frame())
    }

    fn next_sound(&mut self, now: Timestamp) -> Result<(), DecodeError> {
        let job = self.job;
        let rate = i128::from(job.config.format.sample_rate.hz());
        let too_old = now.as_nanos() as i128 - job.config.latency.as_nanos() as i128;
        for _ in 0..CATCH_UP_CHUNKS {
            let Some(sample) = self.decoder.next_sound()? else {
                self.sound.done = true;
                return Ok(());
            };
            let pcm = &sample.pcm;
            let src_rate = pcm.sample_rate.max(1);
            let src_frames = pcm.samples.len() / usize::from(pcm.channels.max(1));
            let length = Duration::from_nanos(
                (src_frames as u64).saturating_mul(1_000_000_000) / u64::from(src_rate),
            );
            let media = sample.pts.unwrap_or(self.sound.next_media);
            self.sound.next_media = media.saturating_add(length);
            // A sound node attached late skips what the graph can no longer play.
            if self.master(media.saturating_add(length)) < too_old {
                self.sound
                    .skip_to(self.sound.frame_of(self.offset + media + length, rate));
                continue;
            }
            let stated = self.sound.frame_of(self.offset + media, rate);
            self.sound.append(
                &sample,
                stated,
                job.config.format.sample_rate.hz(),
                job.config.format.channels.channels(),
            );
            return Ok(());
        }
        Ok(())
    }

    /// Ends the pass's sound: flushes the converter and pads with silence to the end
    /// of the range, then cuts every block that is complete.
    fn finish_sound(&mut self, now: Timestamp) {
        let rate = i128::from(self.job.config.format.sample_rate.hz());
        let end = self
            .sound
            .frame_of(self.offset + self.pictures.pass_end, rate);
        self.sound.flush(end);
        self.cut_blocks(now, !self.job.loop_flag.load(Ordering::Acquire));
    }

    /// Cuts complete blocks on the grid and pushes them. With `last`, a final partial
    /// block is padded. Returns whether anything was cut or dropped.
    fn cut_blocks(&mut self, now: Timestamp, last: bool) -> bool {
        let job = self.job;
        let config = &job.config;
        let stats = &job.shared.stats;
        let base = self.base();
        let late_before = now.as_nanos() as i128
            - config.latency.as_nanos() as i128
            - config.period().as_nanos() as i128;
        let Some(grid) = job.shared.grid() else {
            // No block time yet: keep only what a block could still use.
            let keep = self.sound.frame_at(late_before - base);
            self.sound.drop_before(keep);
            return false;
        };
        let mut busy = false;
        loop {
            let Some((stamp, first)) = self.sound.next_block(grid, base) else {
                return busy;
            };
            let frames = config.block_frames as i64;
            if !last && self.sound.end_frame() < first + frames {
                return busy;
            }
            if last && self.sound.end_frame() <= first {
                return busy;
            }
            busy = true;
            if stamp < late_before {
                stats.worker_dropped_blocks.fetch_add(1, Ordering::Relaxed);
                self.sound.consume_block(first, frames);
                continue;
            }
            let Some(mut buffer) = self.sound.buffer() else {
                stats.worker_dropped_blocks.fetch_add(1, Ordering::Relaxed);
                self.sound.consume_block(first, frames);
                continue;
            };
            self.sound.copy_block(first, frames, &mut buffer);
            self.sound.consume_block(first, frames);
            let block = SoundBlock {
                stamp: to_timestamp(stamp),
                epoch: job.epoch,
                samples: buffer,
            };
            let Some(inlet) = self.sound.inlet.as_mut() else {
                stats.worker_dropped_blocks.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            match inlet.blocks.push(block) {
                Ok(()) => {
                    if job.shared.sound_from.load(Ordering::Relaxed) == 0 {
                        job.shared.sound_from.store(
                            u64::try_from(stamp.max(0)).unwrap_or(0).saturating_add(1),
                            Ordering::Release,
                        );
                    }
                }
                Err(rtrb::PushError::Full(block)) => {
                    stats.worker_dropped_blocks.fetch_add(1, Ordering::Relaxed);
                    self.sound.spares.push(block.samples);
                }
            }
        }
    }
}

/// The picture stream: timestamps, the fallback for a missing one, and the pass end.
struct PictureStream {
    cursor: Duration,
    /// Media time the next picture is expected at.
    next: Duration,
    known: Option<Duration>,
    pass_end: Duration,
    done: bool,
}

impl PictureStream {
    fn new(cursor: Duration) -> Self {
        Self {
            cursor,
            next: cursor,
            known: None,
            pass_end: cursor,
            done: false,
        }
    }

    fn restart(&mut self) {
        self.cursor = Duration::ZERO;
        self.next = Duration::ZERO;
        self.pass_end = Duration::ZERO;
        self.done = false;
    }

    /// The first picture that ends after the cursor (an adapter may seek to the
    /// keyframe before it), checked against the size limits.
    fn first(&mut self, decoder: &mut dyn MediaDecoder) -> Result<PictureSample, MediaNodeError> {
        let sample = match self.pull(decoder) {
            Ok(Some(sample)) => sample,
            Ok(None) => return Err(MediaNodeError::Invalid("a file with a picture stream")),
            Err(error) => return Err(MediaNodeError::Failed(error.to_string())),
        };
        let picture = &sample.picture;
        if let Some(why) = refuse_picture(picture.width, picture.height, picture.system_bytes()) {
            return Err(MediaNodeError::Invalid(why.expected()));
        }
        if sample.pts.is_none() && sample.frame_duration.is_none() {
            return Err(MediaNodeError::Failed(DecodeError::NoTimestamp.to_string()));
        }
        Ok(sample)
    }

    fn pull(
        &mut self,
        decoder: &mut dyn MediaDecoder,
    ) -> Result<Option<PictureSample>, DecodeError> {
        loop {
            let Some(sample) = decoder.next_picture()? else {
                return Ok(None);
            };
            let start = sample.pts.unwrap_or(self.next);
            let duration = sample.frame_duration.or(self.known);
            let ends_after_cursor = duration.map_or(start >= self.cursor, |d| {
                start.saturating_add(d) > self.cursor
            });
            if ends_after_cursor {
                return Ok(Some(sample));
            }
            self.known = duration;
            self.next = duration.map_or(start, |d| start.saturating_add(d));
        }
    }

    /// Media start and end of `sample`; `None` when neither a timestamp nor a duration
    /// is known.
    fn stamp(&mut self, sample: &PictureSample) -> Option<(Duration, Duration)> {
        let start = sample.pts.unwrap_or(self.next);
        let duration = sample.frame_duration.or(self.known)?;
        self.known = Some(duration);
        let end = start.saturating_add(duration);
        self.next = end;
        self.pass_end = self.pass_end.max(end);
        Some((start, end))
    }
}

/// The sound stream on the graph's sample grid: converted frames waiting to be cut
/// into blocks, indexed in graph-rate frames from the run's base time.
struct SoundStream {
    rate: u32,
    channels: usize,
    block_frames: usize,
    converter: Option<Converter>,
    /// Interleaved frames from `head`.
    fifo: Vec<f32>,
    head: i64,
    /// Media time of the next source chunk when it states none.
    next_media: Duration,
    /// The next block: index on the grid, from the grid's reference stamp.
    block: Option<i64>,
    /// Frames still to cut from the front of the converter's next output.
    skip: u64,
    resync: bool,
    done: bool,
    inlet: Option<SoundInlet>,
    spares: Vec<Box<[f32]>>,
    scratch: Vec<f32>,
}

impl SoundStream {
    fn new(config: &AudioGraphConfig, cursor: Duration) -> Self {
        let rate = config.format.sample_rate.hz();
        let mut stream = Self {
            rate,
            channels: config.format.channels.channels().max(1),
            block_frames: config.block_frames as usize,
            converter: None,
            fifo: Vec::new(),
            head: 0,
            next_media: cursor,
            block: None,
            skip: 0,
            resync: true,
            done: false,
            inlet: None,
            spares: Vec::new(),
            scratch: Vec::new(),
        };
        stream.head = stream.frame_of(cursor, i128::from(rate));
        stream
    }

    fn restart(&mut self) {
        self.converter = None;
        self.next_media = Duration::ZERO;
        self.skip = 0;
        self.resync = true;
        self.done = false;
    }

    /// Buffers for the blocks in flight: one per ring slot plus one.
    fn fill_spares(&mut self, config: &AudioGraphConfig) {
        let want = super::sound::MediaSoundNode::ring_slots(config) + 1;
        let width = self.channels * self.block_frames;
        while self.spares.len() < want {
            self.spares.push(vec![0.0; width].into_boxed_slice());
        }
    }

    fn buffer(&mut self) -> Option<Box<[f32]>> {
        if let Some(inlet) = self.inlet.as_mut() {
            while let Ok(buffer) = inlet.returns.pop() {
                self.spares.push(buffer);
            }
        }
        self.spares.pop()
    }

    fn frame_of(&self, media: Duration, rate: i128) -> i64 {
        let n = media.as_nanos() as i128 * rate;
        i64::try_from((n + 500_000_000) / 1_000_000_000).unwrap_or(i64::MAX)
    }

    /// The frame at `nanos` after the base time, rounded down.
    fn frame_at(&self, nanos: i128) -> i64 {
        i64::try_from((nanos * i128::from(self.rate)).div_euclid(1_000_000_000)).unwrap_or(0)
    }

    /// Nanoseconds after the base time of frame `frame`.
    fn frame_time(&self, frame: i64) -> i128 {
        i128::from(frame) * 1_000_000_000 / i128::from(self.rate.max(1))
    }

    fn end_frame(&self) -> i64 {
        self.head + (self.fifo.len() / self.channels) as i64
    }

    fn drop_before(&mut self, frame: i64) {
        let n = (frame - self.head).clamp(0, (self.fifo.len() / self.channels) as i64);
        self.fifo.drain(..n as usize * self.channels);
        self.head += n;
    }

    fn skip_to(&mut self, frame: i64) {
        if frame > self.end_frame() {
            self.fifo.clear();
            self.head = frame;
            self.converter = None;
            self.resync = true;
            self.block = None;
        }
    }

    /// Appends one decoded chunk whose first frame is stated at `stated`.
    fn append(
        &mut self,
        sample: &rbs_media_decode::SoundSample,
        stated: i64,
        rate: u32,
        channels: usize,
    ) {
        let pcm = &sample.pcm;
        let end = self.end_frame();
        let tolerance = (SOUND_RESYNC.as_nanos() as i64 * i64::from(rate)) / 1_000_000_000;
        let restart = self.resync
            || self
                .converter
                .as_ref()
                .is_none_or(|c| c.src_rate() != pcm.sample_rate)
            || (stated - end).abs() > tolerance;
        if restart {
            let mut out = std::mem::take(&mut self.scratch);
            out.clear();
            if let Some(converter) = self.converter.as_mut() {
                converter.finish(&mut out);
            }
            self.extend(&out);
            self.scratch = out;
            let end = self.end_frame();
            if stated > end {
                self.fifo.extend(std::iter::repeat_n(
                    0.0,
                    (stated - end) as usize * self.channels,
                ));
            }
            self.skip = u64::try_from(self.end_frame() - stated).unwrap_or(0);
            self.converter = Some(Converter::new(pcm.sample_rate, rate, channels));
            self.resync = false;
        }
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        if let Some(converter) = self.converter.as_mut() {
            converter.push(&pcm.samples, pcm.channels, &mut out);
        }
        self.extend(&out);
        self.scratch = out;
    }

    /// Appends converted frames, minus any still to skip.
    fn extend(&mut self, out: &[f32]) {
        let frames = (out.len() / self.channels) as u64;
        let skip = self.skip.min(frames);
        self.skip -= skip;
        self.fifo
            .extend_from_slice(&out[skip as usize * self.channels..]);
    }

    /// Flushes the converter and pads or cuts to `end`.
    fn flush(&mut self, end: i64) {
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        if let Some(converter) = self.converter.as_mut() {
            converter.finish(&mut out);
        }
        self.extend(&out);
        self.scratch = out;
        self.converter = None;
        let have = self.end_frame();
        if have < end {
            self.fifo.extend(std::iter::repeat_n(
                0.0,
                (end - have) as usize * self.channels,
            ));
        } else if have > end {
            let keep = (end - self.head).max(0) as usize * self.channels;
            self.fifo.truncate(keep);
        }
        self.done = true;
    }

    /// The stamp (nanoseconds) and first frame of the next block on the grid whose
    /// reference block time is `grid`.
    fn next_block(&mut self, grid: Timestamp, base: i128) -> Option<(i128, i64)> {
        let period_num = i128::from(self.block_frames as u64) * 1_000_000_000;
        let rate = i128::from(self.rate.max(1));
        let stamp_of = |j: i64| grid.as_nanos() as i128 + i128::from(j) * period_num / rate;
        let j = match self.block {
            Some(j) => j,
            None => {
                // The block that holds the stream's first frame.
                let head_time = base + self.frame_time(self.head) - grid.as_nanos() as i128;
                let j = i64::try_from((head_time * rate).div_euclid(period_num)).ok()?;
                self.block = Some(j);
                j
            }
        };
        let stamp = stamp_of(j);
        let first = self.frame_at(stamp - base);
        Some((stamp, first))
    }

    fn copy_block(&self, first: i64, frames: i64, buffer: &mut [f32]) {
        for f in 0..frames {
            let at = first + f - self.head;
            for c in 0..self.channels {
                let v = if at >= 0 {
                    self.fifo
                        .get(at as usize * self.channels + c)
                        .copied()
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                if let Some(slot) = buffer.get_mut(f as usize * self.channels + c) {
                    *slot = v;
                }
            }
        }
    }

    fn consume_block(&mut self, first: i64, frames: i64) {
        self.drop_before(first + frames);
        if let Some(j) = self.block.as_mut() {
            *j += 1;
        }
    }
}

/// Picture slots the worker fills: at most [`PICTURE_SLOTS`] and at most 128 MiB.
fn fill_count(bytes: usize) -> usize {
    (MAX_FRAME_BYTES / bytes.max(1)).clamp(1, PICTURE_SLOTS)
}

fn to_timestamp(nanos: i128) -> Timestamp {
    Timestamp::from_nanos(u64::try_from(nanos.max(0)).unwrap_or(u64::MAX))
}

struct PictureBits<'a> {
    width: u32,
    height: u32,
    layout: PictureLayout,
    color: SampleColor,
    bytes: &'a [u8],
}

fn picture_parts(picture: &rbs_media_decode::Picture) -> Option<PictureBits<'_>> {
    match picture.data() {
        rbs_media_decode::PictureData::System {
            layout,
            color,
            bytes,
        } => Some(PictureBits {
            width: picture.width,
            height: picture.height,
            layout: *layout,
            color: *color,
            bytes,
        }),
        _ => None,
    }
}

fn system_frame(
    picture: &PictureBits<'_>,
    timestamp: Timestamp,
) -> Result<VideoFrame, DecodeError> {
    match picture.layout {
        PictureLayout::Rgba8 => rgba_frame(picture, timestamp),
        PictureLayout::Nv12 => {
            check_color(map_color(picture.color), 8).map_err(|error| {
                DecodeError::Failed(format!("colour is not decodable: {error}"))
            })?;
            Err(DecodeError::Unsupported(
                "nv12 is checked with check_color; YUV upload on the worker is not wired yet"
                    .to_owned(),
            ))
        }
        PictureLayout::P010 => {
            check_color(map_color(picture.color), 10).map_err(|error| {
                DecodeError::Failed(format!("colour is not decodable: {error}"))
            })?;
            Err(DecodeError::Unsupported(
                "p010 is checked with check_color; YUV upload on the worker is not wired yet"
                    .to_owned(),
            ))
        }
        _ => Err(DecodeError::Unsupported("picture layout".to_owned())),
    }
}

fn map_color(color: SampleColor) -> ColorSpace {
    match color {
        SampleColor::Srgb => ColorSpace::SRGB,
        SampleColor::Bt709Limited => ColorSpace::BT709_LIMITED,
        SampleColor::Bt709Full => ColorSpace::BT709_FULL,
        // A colour this node does not know is refused by `check_color` later.
        _ => ColorSpace::SRGB,
    }
}

fn rgba_frame(picture: &PictureBits<'_>, timestamp: Timestamp) -> Result<VideoFrame, DecodeError> {
    let format = VideoFormat::new(
        Size::new(picture.width, picture.height),
        PixelFormat::Rgba8,
        ColorSpace::SRGB,
    );
    let stride = (picture.width as usize).saturating_mul(4);
    let bytes = bytes::Bytes::copy_from_slice(picture.bytes);
    let planes = [PlaneLayout { offset: 0, stride }];
    let system = SystemFrame::new(format, bytes, &planes)
        .map_err(|error| DecodeError::Failed(format!("picture layout: {error}")))?;
    let frame = VideoFrame::system(timestamp, system, None);
    // The first clone of a `Bytes` made from a copy allocates its shared header. Do
    // that here, on the worker, so the picture node's `share` only counts a reference.
    drop(frame.share());
    Ok(frame)
}
