//! Studio side of the test-double helper (ADR-0109).
//!
//! The binary in this crate is the tree root a real CEF build replaces. CEF is a
//! process tree (browser, GPU, renderer, utilities). This process is one executable
//! so tests can run without downloading CEF. The studio does not link or load CEF.
//!
//! The studio allocates the frame buffer before spawn. Dropping that buffer does not
//! join the child. A process exit is Unavailable and counts toward back-off.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use rbs_browser_ipc::{
    Configure, FrameBytes, MAX_PAYLOAD, decode_frame, encode_configure, restart_delay, seal_alpha,
};

/// Result of starting the helper. The frame buffer is the studio's; `child` is the
/// only join handle, and it is not inside the frame.
#[derive(Debug)]
pub struct HelperChild {
    child: Option<Child>,
    /// Kept open so a `--hold` child stays blocked in its stdin read. Dropping the
    /// frame buffer does not drop this, and does not join the child.
    stdin: Option<ChildStdin>,
    /// Studio-owned pixels. Empty when the child exited before a frame.
    pub frame: Vec<u8>,
    /// Width of `frame` when a frame arrived.
    pub width: u32,
    /// Height of `frame` when a frame arrived.
    pub height: u32,
    /// The helper is not running.
    pub unavailable: bool,
    /// Exit counted toward back-off. A live child stays at 0.
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

    /// Kills and reaps the child on the control path. A dead child does not hold a slot.
    pub fn stop(&mut self) {
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
        if self.child.take().is_some() || !self.unavailable {
            self.attempts = self.attempts.max(1);
            self.retry_after = restart_delay(self.attempts);
            self.unavailable = true;
        }
    }
}

impl Drop for HelperChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Allocates the studio buffer, then spawns `program`. A child that exits is
/// Unavailable. This does not panic when the child exits.
pub fn spawn_helper(
    program: &Path,
    args: &[&str],
    config: &Configure,
) -> Result<HelperChild, String> {
    let pixels = (config.width as usize)
        .saturating_mul(config.height as usize)
        .saturating_mul(4);
    // The studio owns this buffer before the process exists.
    let frame = vec![0u8; pixels];
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "helper stdin was not piped".to_owned())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "helper stdout was not piped".to_owned())?;
    let message = encode_configure(config);
    let _ = stdin.write_all(&message);
    let _ = stdin.flush();
    let read = read_message(&mut stdout);
    let mut hosted = HelperChild {
        child: Some(child),
        stdin: Some(stdin),
        frame,
        width: 0,
        height: 0,
        unavailable: false,
        attempts: 0,
        retry_after: Duration::ZERO,
    };
    if let Ok(bytes) = read {
        if let Some(decoded) = decode_frame(&bytes) {
            copy_frame(&mut hosted, &decoded, config.transparent);
        }
    }
    let mut saw_exit = false;
    if args.contains(&"--hold") {
        // The frame is already in `hosted`; the parent polls liveness on the control thread.
    } else {
        for _ in 0..50_000 {
            let status = hosted
                .child
                .as_mut()
                .ok_or_else(|| "helper was already reaped".to_owned())?
                .try_wait()
                .map_err(|error| error.to_string())?;
            if status.is_some() {
                saw_exit = true;
                break;
            }
            std::thread::yield_now();
        }
    }
    if saw_exit {
        if let Some(mut child) = hosted.child.take() {
            let _ = child.wait();
        }
        hosted.stdin.take();
        hosted.attempts = 1;
        hosted.retry_after = restart_delay(1);
        hosted.unavailable = true;
    }
    Ok(hosted)
}

fn copy_frame(hosted: &mut HelperChild, decoded: &FrameBytes, transparent: bool) {
    if decoded.rgba.len() != hosted.frame.len() {
        return;
    }
    hosted.frame.copy_from_slice(&decoded.rgba);
    seal_alpha(&mut hosted.frame, transparent);
    hosted.width = decoded.width;
    hosted.height = decoded.height;
}

fn read_message(input: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut len_buf = [0u8; 4];
    if let Err(error) = input.read_exact(&mut len_buf) {
        return Err(error.to_string());
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_PAYLOAD {
        return Err("helper frame is larger than the studio cap".into());
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

/// The helper process. `--exit-immediately` exits before any frame. `--hold` writes
/// one frame and then waits for stdin to close. Otherwise it writes one frame and exits.
pub fn helper_main(hold: bool) -> Result<(), String> {
    let mut stdin = std::io::stdin();
    let raw = read_message(&mut stdin)?;
    let config = rbs_browser_ipc::decode_configure(&raw).ok_or_else(|| "configure".to_owned())?;
    let pixels = (config.width as usize)
        .saturating_mul(config.height as usize)
        .saturating_mul(4);
    // Bright checker so a missing CEF page is obvious on Program (not near-black).
    let mut rgba = vec![0u8; pixels];
    let width = config.width as usize;
    for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        let x = index % width;
        let y = index / width;
        let light = ((x / 32) + (y / 32)) % 2 == 0;
        if light {
            pixel[0] = 40;
            pixel[1] = 180;
            pixel[2] = 255;
        } else {
            pixel[0] = 20;
            pixel[1] = 40;
            pixel[2] = 120;
        }
        pixel[3] = 255;
    }
    let message = rbs_browser_ipc::encode_frame(config.width, config.height, &rgba);
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&message)
        .map_err(|error| error.to_string())?;
    stdout.flush().map_err(|error| error.to_string())?;
    if hold {
        // Block until the parent closes stdin or kills the process. The frame is
        // already flushed, so the parent can drop its buffer without waiting here.
        let mut rest = [0u8; 1];
        let _ = stdin.read(&mut rest);
    }
    Ok(())
}
