//! Studio side of the browser helper, and the test-double helper (ADR-0109).
//!
//! The binary in this crate is the tree root a real CEF build replaces (ADR-0132
//! decision 1). CEF is a process tree (browser, GPU, renderer, utilities). This
//! process is one executable so tests run without CEF. The studio does not link or
//! load CEF.
//!
//! The studio allocates the frame section before spawn ([`FrameRing::create`]) and
//! names it in `configure`. The helper paints into it and beats its heartbeat. The
//! studio copies the newest frame out on its control thread; dropping the section or
//! the copy never joins the child. A process exit or a lost heartbeat is Unavailable
//! and counts toward back-off.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use rbs_browser_ipc::{
    Configure, HEARTBEAT_PERIOD, MAX_PAYLOAD, encode_configure, heartbeat_lost, restart_delay,
    startup_lost,
};
use rbs_browser_shm::{FrameRing, RingReader, SlotLayout, SoundPacket, SoundRing};

/// A running (or exited) helper as the studio sees it.
#[derive(Debug)]
pub struct HelperChild {
    child: Option<Child>,
    /// Kept open: the helper runs until its stdin closes.
    stdin: Option<ChildStdin>,
    ring: FrameRing,
    reader: RingReader,
    /// The page's sound section, when the studio asked for sound.
    sound: Option<SoundRing>,
    sound_scratch: Vec<f32>,
    last_beat: u64,
    last_beat_at: Instant,
    /// Studio-owned pixels: the newest frame copied out of the section. Empty until
    /// the first frame.
    pub frame: Vec<u8>,
    /// Width of `frame` once a frame arrived.
    pub width: u32,
    /// Height of `frame` once a frame arrived.
    pub height: u32,
    /// The helper is not running (exited, killed, or lost its heartbeat).
    pub unavailable: bool,
    /// Exits counted toward back-off. A live child stays at 0.
    pub attempts: u32,
    /// ADR-0024 delay after `attempts`.
    pub retry_after: Duration,
}

impl HelperChild {
    /// True when the OS process is still running. Never waits.
    pub fn process_alive(&mut self) -> bool {
        match self.child.as_mut().and_then(|child| child.try_wait().ok()) {
            Some(None) => true,
            Some(Some(_)) => {
                self.note_exit();
                false
            }
            None => false,
        }
    }

    /// Copies the newest published frame into [`HelperChild::frame`]. Returns true
    /// when a new frame arrived. Control thread only; never waits.
    pub fn refresh(&mut self) -> bool {
        if !self.ring.take_latest(&mut self.reader, &mut self.frame) {
            return false;
        }
        self.width = self.ring.width();
        self.height = self.ring.height();
        true
    }

    /// Calls `each` for every sound packet the helper wrote since the last call, in
    /// order. Control thread only; never waits.
    pub fn read_sound(&mut self, each: impl FnMut(SoundPacket<'_>)) {
        if let Some(sound) = self.sound.as_ref() {
            sound.read(&mut self.sound_scratch, each);
        }
    }

    /// True when the helper has a sound section.
    pub fn has_sound(&self) -> bool {
        self.sound.is_some()
    }

    /// Pixel layout of [`HelperChild::frame`].
    pub fn layout(&self) -> SlotLayout {
        self.ring.layout()
    }

    /// Checks the heartbeat against `now` (the caller's clock). A helper whose counter
    /// has not moved for [`rbs_browser_ipc::HEARTBEAT_LIMIT`] is stopped and counted as
    /// an exit; before its first beat the limit is [`rbs_browser_ipc::STARTUP_LIMIT`]
    /// from spawn. Returns false when the heartbeat was lost.
    pub fn check_heartbeat(&mut self, now: Instant) -> bool {
        let beat = self.ring.heartbeat();
        if beat != self.last_beat {
            self.last_beat = beat;
            self.last_beat_at = now;
            return true;
        }
        let silent = now.saturating_duration_since(self.last_beat_at);
        let lost = if self.last_beat == 0 {
            startup_lost(silent)
        } else {
            heartbeat_lost(silent)
        };
        if lost {
            self.stop();
            self.note_exit();
            return false;
        }
        true
    }

    /// Frames the helper has published so far.
    pub fn frames_written(&self) -> u64 {
        self.ring.frames_written()
    }

    /// Closes stdin, the helper's stop signal, and gives it `grace` to exit on its
    /// own (an engine removes its temporary profile then); kills it after that. Not
    /// for the render thread: it can wait up to `grace`.
    pub fn close(&mut self, grace: Duration) {
        self.stdin.take();
        if let Some(child) = self.child.as_mut() {
            let deadline = Instant::now() + grace;
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        self.stop();
    }

    /// Kills and reaps the child on the control path. A dead child holds no slot.
    pub fn stop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.unavailable = true;
    }

    /// The studio's frame. Dropping the returned `Vec` does not touch the child.
    pub fn take_frame(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.frame)
    }

    fn note_exit(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        if !self.unavailable || self.attempts == 0 {
            self.attempts = self.attempts.max(1);
            self.retry_after = restart_delay(self.attempts);
            self.unavailable = true;
        }
    }
}

impl Drop for HelperChild {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Allocates the frame section, and the sound section when `config.sample_rate` is not
/// 0 (the studio takes page sound), then spawns `program` with `configure` on its
/// stdin. Returns at once: frames and exits are seen by later polls. Never panics when
/// the child exits.
pub fn spawn_helper(
    program: &Path,
    args: &[&str],
    config: &Configure,
    layout: SlotLayout,
) -> Result<HelperChild, String> {
    // The studio owns this section before the process exists (ADR-0109 decision 8).
    let ring = FrameRing::create(config.width, config.height, layout).map_err(|e| e.to_string())?;
    let sound = if config.sample_rate > 0 {
        Some(SoundRing::create().map_err(|e| e.to_string())?)
    } else {
        None
    };
    let mut configure = config.clone();
    configure.frames = ring.name().to_owned();
    configure.sound = sound
        .as_ref()
        .map_or_else(String::new, |sound| sound.name().to_owned());
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "helper stdin was not piped".to_owned())?;
    let _ = stdin.write_all(&encode_configure(&configure));
    let _ = stdin.flush();
    let reader = ring.reader();
    Ok(HelperChild {
        child: Some(child),
        stdin: Some(stdin),
        ring,
        reader,
        sound,
        sound_scratch: Vec::new(),
        last_beat: 0,
        last_beat_at: Instant::now(),
        frame: Vec::new(),
        width: 0,
        height: 0,
        unavailable: false,
        attempts: 0,
        retry_after: Duration::ZERO,
    })
}

/// How long a stopped helper may take to exit on its own before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(1);

/// Studio-side supervisor thread for one running helper (ADR-0109 decision 8). It
/// exists only while the source is Active (P13). At the page's frame rate it copies the
/// newest frame out of the section and hands it to `publish`, and it checks the
/// process and the heartbeat. It never runs on the render thread.
#[derive(Debug)]
pub struct Supervisor {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<HelperChild>>,
}

/// Where the supervisor hands page sound (ADR-0109 decision 6). Runs on the supervisor
/// thread, never on the render or audio thread.
pub trait PageSound: Send + 'static {
    /// First in each poll: the OS monotonic time just read
    /// ([`rbs_browser_shm::os_monotonic_nanos`]), for the edge mapping.
    fn observe(&mut self, os_now: u64);
    /// One packet the helper wrote, in order.
    fn packet(&mut self, packet: SoundPacket<'_>);
    /// Last in each poll, after its packets.
    fn pump(&mut self);
}

/// How often the supervisor reads page sound: often enough that a block is pushed
/// well inside the audio graph's latency.
pub const SOUND_POLL: Duration = Duration::from_millis(5);

impl Supervisor {
    /// Moves `child` onto a new supervisor thread.
    pub fn start(
        child: HelperChild,
        fps: u32,
        publish: impl FnMut(&[u8], u32, u32) + Send + 'static,
    ) -> Result<Self, String> {
        Self::start_inner(child, fps, publish, None)
    }

    /// As [`Supervisor::start`], and hands the page's sound to `sound`. Polls at
    /// least every [`SOUND_POLL`] while the child has a sound section.
    pub fn start_with_sound(
        child: HelperChild,
        fps: u32,
        publish: impl FnMut(&[u8], u32, u32) + Send + 'static,
        sound: Box<dyn PageSound>,
    ) -> Result<Self, String> {
        Self::start_inner(child, fps, publish, Some(sound))
    }

    fn start_inner(
        mut child: HelperChild,
        fps: u32,
        mut publish: impl FnMut(&[u8], u32, u32) + Send + 'static,
        mut sound: Option<Box<dyn PageSound>>,
    ) -> Result<Self, String> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let (stop_flag, exited_flag) = (Arc::clone(&stop), Arc::clone(&exited));
        // Poll twice per page frame so a new frame waits at most half a period.
        let mut period = Duration::from_secs(1) / fps.clamp(1, 60) / 2;
        if sound.is_some() && child.has_sound() {
            period = period.min(SOUND_POLL);
        } else {
            sound = None;
        }
        let thread = std::thread::Builder::new()
            .name("rbs-browser-supervisor".into())
            .spawn(move || {
                // A control thread, not a real-time one (ADR-0055).
                rbs_crash::report_panics_on_this_thread();
                while !stop_flag.load(Ordering::Acquire) {
                    if let Some(sound) = sound.as_mut() {
                        sound.observe(rbs_browser_shm::os_monotonic_nanos());
                        child.read_sound(|packet| sound.packet(packet));
                        sound.pump();
                    }
                    if child.refresh() {
                        publish(&child.frame, child.width, child.height);
                    }
                    if !child.process_alive() || !child.check_heartbeat(Instant::now()) {
                        exited_flag.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::park_timeout(period);
                }
                // A stop, not an exit: let the helper shut down cleanly, here and not
                // on the caller's thread.
                if !exited_flag.load(Ordering::Acquire) {
                    child.close(STOP_GRACE);
                }
                child
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            stop,
            exited,
            thread: Some(thread),
        })
    }

    /// True once the helper exited or lost its heartbeat. Never waits.
    pub fn exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Stops the helper and waits for the thread, up to [`STOP_GRACE`] plus one
    /// poll. Returns the stopped child. Tests and shutdown only.
    pub fn stop(&mut self) -> Option<HelperChild> {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let thread = self.thread.take()?;
        thread.thread().unpark();
        let mut child = thread.join().ok()?;
        child.stop();
        Some(child)
    }

    /// Asks the helper to stop and returns at once. The thread closes the helper
    /// (at most [`STOP_GRACE`], then a kill) and ends by itself. Control path.
    pub fn stop_detached(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_detached();
    }
}

/// Reads one length-prefixed control message.
pub fn read_message(input: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut len_buf = [0u8; 4];
    input
        .read_exact(&mut len_buf)
        .map_err(|error| error.to_string())?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_PAYLOAD {
        return Err("control message is larger than the cap".into());
    }
    let mut body = vec![0u8; len];
    input
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    let mut message = Vec::with_capacity(4 + len);
    message.extend_from_slice(&len_buf);
    message.extend_from_slice(&body);
    Ok(message)
}

/// How the test-double helper behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperMode {
    /// Paint one frame, beat once, exit.
    Once,
    /// Paint at `fps` and beat until stdin closes.
    Run,
    /// Paint one frame, then stay alive without beating (a hung page).
    Hang,
}

/// The test-double helper process: a moving bright checker, so a missing CEF page
/// is obvious on Program. Opaque (the helper applies `transparent` = false itself;
/// the studio does no per-frame pass).
pub fn helper_main(mode: HelperMode) -> Result<(), String> {
    helper_main_with(mode, false)
}

/// As [`helper_main`]; with `tone` and a sound section, `Run` also writes a 440 Hz
/// stereo tone at the asked rate (tests only: the app never passes it).
pub fn helper_main_with(mode: HelperMode, tone: bool) -> Result<(), String> {
    let mut stdin = std::io::stdin();
    let raw = read_message(&mut stdin)?;
    let config = rbs_browser_ipc::decode_configure(&raw).ok_or_else(|| "configure".to_owned())?;
    let ring = FrameRing::open(&config.frames, config.width, config.height)
        .map_err(|error| error.to_string())?;
    let mut writer = ring.writer();
    let width = config.width as usize;
    let mut shift = 0usize;
    let mut paint = |shift: usize| {
        ring.publish_with(&mut writer, |rgba| checker(rgba, width, shift));
    };
    paint(shift);
    ring.beat();
    match mode {
        HelperMode::Once => Ok(()),
        HelperMode::Hang => {
            let mut rest = [0u8; 1];
            let _ = stdin.read(&mut rest);
            Ok(())
        }
        HelperMode::Run => {
            let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                let mut rest = [0u8; 64];
                while matches!(stdin.read(&mut rest), Ok(n) if n > 0) {}
                let _ = closed_tx.send(());
            });
            let period = Duration::from_secs(1) / config.fps.clamp(1, 60);
            let mut since_beat = Duration::ZERO;
            let mut tone = (tone && !config.sound.is_empty())
                .then(|| SoundRing::open(&config.sound).ok())
                .flatten()
                .map(|ring| Tone::new(ring, config.sample_rate.max(8_000)));
            loop {
                match closed_rx.recv_timeout(period) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => return Ok(()),
                }
                if let Some(tone) = tone.as_mut() {
                    tone.write(rbs_browser_shm::os_monotonic_nanos());
                }
                shift = shift.wrapping_add(4);
                paint(shift);
                since_beat += period;
                if since_beat >= HEARTBEAT_PERIOD / 2 {
                    ring.beat();
                    since_beat = Duration::ZERO;
                }
            }
        }
    }
}

/// The test double's page sound: a continuous 440 Hz sine, written in packets that
/// cover the OS time since the last one.
struct Tone {
    ring: SoundRing,
    rate: u32,
    /// OS time of the next frame to write; 0 before the first packet.
    next_os: u64,
    frame: u64,
    left: Vec<f32>,
}

impl Tone {
    fn new(ring: SoundRing, rate: u32) -> Self {
        Self {
            ring,
            rate,
            next_os: 0,
            frame: 0,
            left: Vec::new(),
        }
    }

    fn write(&mut self, os_now: u64) {
        if self.next_os == 0 {
            // Start 10 ms back, so the first packet has sound in it.
            self.next_os = os_now.saturating_sub(10_000_000);
        }
        let due = os_now.saturating_sub(self.next_os);
        let frames = (u128::from(due) * u128::from(self.rate) / 1_000_000_000) as usize;
        if frames == 0 {
            return;
        }
        self.left.clear();
        let step = std::f64::consts::TAU * 440.0 / f64::from(self.rate);
        self.left.extend(
            (0..frames as u64).map(|i| (0.25 * ((self.frame + i) as f64 * step).sin()) as f32),
        );
        self.ring
            .write_planar(self.next_os, self.rate, &[&self.left, &self.left], frames);
        self.frame += frames as u64;
        self.next_os += (frames as u128 * 1_000_000_000 / u128::from(self.rate)) as u64;
    }
}

fn checker(rgba: &mut [u8], width: usize, shift: usize) {
    for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        let x = index % width.max(1) + shift;
        let y = index / width.max(1);
        let light = ((x / 32) + (y / 32)).is_multiple_of(2);
        let (r, g, b) = if light { (40, 180, 255) } else { (20, 40, 120) };
        pixel.copy_from_slice(&[r, g, b, 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checker_is_opaque_and_bright() {
        let mut rgba = vec![0u8; 2 * 2 * 4];
        checker(&mut rgba, 2, 0);
        assert_eq!(&rgba[..4], &[40, 180, 255, 255]);
        assert!(rgba.chunks_exact(4).all(|p| p[3] == 255));
    }
}
