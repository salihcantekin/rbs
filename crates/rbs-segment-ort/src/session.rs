//! One session: a worker thread that owns the ONNX Runtime session and turns the newest
//! submitted small picture into a matte. The render thread only touches a one-slot
//! mailbox with `try_lock`, so it never waits for the worker (principle P6).
//!
//! Zero cost when unused (P13): no thread exists before `warm`, and `close` joins it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use ort::environment::Environment;
use ort::session::Session;
use ort::value::Tensor;
use rbs_core::media::{FramePayload, PixelFormat, Size, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_segment::{Matte, MattePlane, PersonMaskSession, SegmentError, StrayFilter, SubmitOutcome};

use crate::net::{self, Net, Spec};

/// A frame copied for the worker: the pixels as BGRA, tightly packed.
struct Pending {
    timestamp: Timestamp,
    size: Size,
    bgra: Vec<u8>,
}

#[derive(Default)]
struct Mailbox {
    /// Pre-sized buffers the render thread fills; the worker gives them back after use,
    /// so `submit` allocates nothing after `warm` (principle P6).
    free: Vec<Vec<u8>>,
    pending: Option<Pending>,
    matte: Option<Matte>,
    failed: Option<SegmentError>,
    reset: bool,
    /// The worker made its runtime session and ran its warm-up pictures.
    ready: bool,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    /// Signalled once when `ready` or `failed` is set.
    readied: Condvar,
    stop: AtomicBool,
}

/// How long `warm` waits on the control thread for the worker to be ready. The filter
/// shows nothing until its first matte (privacy), so a session that is ready when the
/// filter joins the schedule turns a gap of seconds (DirectML compiles its shaders on the
/// first run) into a few frames. A slower start goes on in the background.
const READY_WAIT: Duration = Duration::from_millis(1500);

/// Warm-up pictures the worker runs before it is ready: RVM's first run has empty
/// recurrent states and its second the real ones, and each shape is compiled once.
const WARM_UP_RUNS: usize = 2;

/// Waits up to `limit` for the worker to report ready or failed. Returns whether it did.
fn wait_ready(shared: &Shared, limit: Duration) -> bool {
    let Ok(mailbox) = shared.mailbox.lock() else {
        return false;
    };
    match shared
        .readied
        .wait_timeout_while(mailbox, limit, |m| !m.ready && m.failed.is_none())
    {
        Ok((m, _)) => m.ready || m.failed.is_some(),
        Err(_) => false,
    }
}

/// Marks the worker ready (or failed) and wakes a waiting `warm`.
fn set_ready(shared: &Shared, failed: Option<SegmentError>) {
    if let Ok(mut mailbox) = shared.mailbox.lock() {
        mailbox.ready = failed.is_none();
        if failed.is_some() {
            mailbox.failed = failed;
        }
    }
    shared.readied.notify_all();
}

/// Buffers in circulation: one being filled, one pending, one in the worker.
const BUFFERS: usize = 3;

pub(crate) struct OrtSession {
    spec: Spec,
    model: Arc<[u8]>,
    runtime: PathBuf,
    input_size: Size,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl OrtSession {
    pub(crate) fn new(spec: Spec, model: Arc<[u8]>, runtime: PathBuf, input_size: Size) -> Self {
        Self {
            spec,
            model,
            runtime,
            input_size,
            shared: Arc::new(Shared {
                mailbox: Mutex::new(Mailbox::default()),
                wake: Condvar::new(),
                readied: Condvar::new(),
                stop: AtomicBool::new(false),
            }),
            worker: None,
        }
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for OrtSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Copies a system-memory frame into `bgra` as tightly packed BGRA, reusing the buffer's
/// capacity. `None` when the frame is not RGBA or BGRA system memory.
fn to_bgra(frame: &VideoFrame, bgra: &mut Vec<u8>) -> Option<(Timestamp, Size)> {
    let FramePayload::System(system) = frame.payload() else {
        return None;
    };
    let format = system.format();
    let swap = match format.pixel_format {
        PixelFormat::Bgra8 | PixelFormat::Bgra8Srgb => false,
        PixelFormat::Rgba8 | PixelFormat::Rgba8Srgb => true,
        _ => return None,
    };
    let (data, stride) = system.plane(0)?;
    let width = format.size.width as usize;
    let height = format.size.height as usize;
    bgra.clear();
    for row in 0..height {
        let line = data.get(row * stride..row * stride + width * 4)?;
        if swap {
            for pixel in line.chunks_exact(4) {
                bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        } else {
            bgra.extend_from_slice(line);
        }
    }
    Some((frame.timestamp(), format.size))
}

impl PersonMaskSession for OrtSession {
    fn warm(&mut self) -> Result<(), SegmentError> {
        if self.worker.is_some() {
            return Ok(());
        }
        let capacity = self.input_size.width as usize * self.input_size.height as usize * 4;
        {
            let mut mailbox = self
                .shared
                .mailbox
                .lock()
                .map_err(|_| SegmentError::DeviceLost)?;
            mailbox.free = (0..BUFFERS - 1)
                .map(|_| Vec::with_capacity(capacity))
                .collect();
        }
        let shared = Arc::clone(&self.shared);
        let (spec, model, runtime) = (self.spec, Arc::clone(&self.model), self.runtime.clone());
        // The worker makes the session itself and runs warm-up pictures (DirectML
        // compiles shaders on the first runs). `warm` runs on the control thread and
        // waits for that up to `READY_WAIT`; a slower start goes on in the background and
        // early pictures wait in the mailbox.
        let size = self.input_size;
        let handle = std::thread::Builder::new()
            .name("rbs-segment-onnx".into())
            .spawn(move || worker_main(&shared, spec, &model, &runtime, size))
            .map_err(|_| SegmentError::Unavailable)?;
        self.worker = Some(handle);
        let _ = wait_ready(&self.shared, READY_WAIT);
        Ok(())
    }

    fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome {
        let Ok(mut mailbox) = self.shared.mailbox.try_lock() else {
            return SubmitOutcome::DroppedBusy;
        };
        let mut buffer = match mailbox.pending.take() {
            Some(old) => old.bgra,
            None => match mailbox.free.pop() {
                Some(buffer) => buffer,
                None => return SubmitOutcome::DroppedBusy,
            },
        };
        match to_bgra(frame, &mut buffer) {
            Some((timestamp, size)) => {
                mailbox.pending = Some(Pending {
                    timestamp,
                    size,
                    bgra: buffer,
                });
            }
            None => {
                mailbox.free.push(buffer);
                return SubmitOutcome::NotShareable;
            }
        }
        drop(mailbox);
        self.shared.wake.notify_one();
        SubmitOutcome::Accepted
    }

    fn latest(&mut self) -> Result<Option<Matte>, SegmentError> {
        let Ok(mut mailbox) = self.shared.mailbox.try_lock() else {
            return Ok(None);
        };
        if let Some(error) = mailbox.failed {
            return Err(error);
        }
        Ok(mailbox.matte.take())
    }

    fn reset(&mut self) {
        if let Ok(mut mailbox) = self.shared.mailbox.try_lock() {
            mailbox.reset = true;
            mailbox.matte = None;
        }
    }

    fn close(mut self: Box<Self>) {
        self.shutdown();
    }
}

/// The process-wide ONNX Runtime environment, made once from the library file.
fn init_runtime(path: &Path) -> Result<&'static Environment, SegmentError> {
    static ENV: OnceLock<Result<Environment, SegmentError>> = OnceLock::new();
    ENV.get_or_init(|| {
        let builder = ort::init_from(path).map_err(|_| SegmentError::Unavailable)?;
        builder
            .with_name("rbs")
            .with_telemetry(false)
            .build()
            .map_err(|_| SegmentError::Unavailable)
    })
    .as_ref()
    .map_err(|e| *e)
}

fn make_session(env: &Environment, model: &[u8]) -> Result<Session, SegmentError> {
    let builder = Session::builder(env).map_err(|_| SegmentError::Unavailable)?;
    // DirectML wants no memory pattern and no parallel execution; the processor path
    // keeps its defaults.
    #[cfg(windows)]
    let builder = {
        let gpu = [ort::ep::DirectML::default().build()];
        builder
            .with_execution_providers(gpu)
            .map_err(|_| SegmentError::Unavailable)?
            .with_memory_pattern(false)
            .map_err(|_| SegmentError::Unavailable)?
            .with_parallel_execution(false)
            .map_err(|_| SegmentError::Unavailable)?
    };
    let mut builder = builder;
    builder
        .commit_from_memory(model)
        .map_err(|_| SegmentError::Unavailable)
}

/// Everything the worker owns.
struct Engine {
    session: Session,
    spec: Spec,
    input: Vec<f32>,
    bytes: Vec<u8>,
    strays: StrayFilter,
    /// RVM's four recurrent states, as (shape, data); empty for the other networks.
    states: Vec<(Vec<usize>, Vec<f32>)>,
}

impl Engine {
    fn new(session: Session, spec: Spec) -> Self {
        let mut engine = Self {
            session,
            spec,
            input: Vec::new(),
            bytes: Vec::new(),
            strays: StrayFilter::new(),
            states: Vec::new(),
        };
        engine.reset_states();
        engine
    }

    fn reset_states(&mut self) {
        self.states.clear();
        if self.spec.net == Net::Rvm {
            self.states = (0..4).map(|_| (vec![1, 1, 1, 1], vec![0.0])).collect();
        }
    }

    /// Runs one picture and returns the alpha plane the network made.
    fn infer(&mut self, src: Tensor<f32>, ratio: f32) -> Result<Vec<f32>, SegmentError> {
        let lost = |_| SegmentError::DeviceLost;
        match self.spec.net {
            Net::MediaPipe => {
                let outputs = self
                    .session
                    .run(ort::inputs!["pixel_values" => src])
                    .map_err(lost)?;
                let (_, data) = outputs["alphas"]
                    .try_extract_tensor::<f32>()
                    .map_err(lost)?;
                Ok(data.to_vec())
            }
            Net::Modnet => {
                let outputs = self
                    .session
                    .run(ort::inputs!["input" => src])
                    .map_err(lost)?;
                let (_, data) = outputs["output"]
                    .try_extract_tensor::<f32>()
                    .map_err(lost)?;
                Ok(data.to_vec())
            }
            Net::Rvm => {
                let mut tensors = Vec::with_capacity(4);
                for (shape, data) in &self.states {
                    tensors.push(Tensor::from_array((shape.clone(), data.clone())).map_err(lost)?);
                }
                let [r1, r2, r3, r4]: [Tensor<f32>; 4] =
                    tensors.try_into().map_err(|_| SegmentError::DeviceLost)?;
                let ratio = Tensor::from_array(([1usize], vec![ratio])).map_err(lost)?;
                let outputs = self
                    .session
                    .run(ort::inputs![
                        "src" => src,
                        "r1i" => r1,
                        "r2i" => r2,
                        "r3i" => r3,
                        "r4i" => r4,
                        "downsample_ratio" => ratio
                    ])
                    .map_err(lost)?;
                let mut next = Vec::with_capacity(4);
                for name in ["r1o", "r2o", "r3o", "r4o"] {
                    let (shape, data) = outputs[name].try_extract_tensor::<f32>().map_err(lost)?;
                    let dims = shape.iter().map(|d| *d as usize).collect::<Vec<_>>();
                    next.push((dims, data.to_vec()));
                }
                let (_, pha) = outputs["pha"].try_extract_tensor::<f32>().map_err(lost)?;
                let pha = pha.to_vec();
                self.states = next;
                Ok(pha)
            }
        }
    }

    fn run(&mut self, pending: &Pending) -> Result<Matte, SegmentError> {
        let (dw, dh) = net::input_dims(self.spec.net, pending.size.width, pending.size.height);
        net::fill_input(
            self.spec.net,
            &pending.bgra,
            pending.size.width,
            pending.size.height,
            dw,
            dh,
            &mut self.input,
        );
        let shape = [1usize, 3, dh as usize, dw as usize];
        let src = Tensor::from_array((shape, self.input.clone()))
            .map_err(|_| SegmentError::DeviceLost)?;
        let alpha = self.infer(src, net::rvm_ratio(dw, dh))?;
        if alpha.len() != dw as usize * dh as usize {
            return Err(SegmentError::DeviceLost);
        }
        net::alpha_to_bytes(&alpha, &mut self.bytes);
        // A hat or lamp the network took for a second person is dropped (not joined).
        self.strays.apply(&mut self.bytes, dw, dh);
        Ok(Matte::new(
            pending.timestamp,
            Size::new(dw, dh),
            MattePlane::Cpu(Arc::from(self.bytes.as_slice())),
        ))
    }
}

fn worker_main(shared: &Shared, spec: Spec, model: &[u8], runtime: &Path, size: Size) {
    let engine = init_runtime(runtime)
        .and_then(|env| make_session(env, model))
        .map(|session| Engine::new(session, spec));
    let mut engine = match engine {
        Ok(engine) => engine,
        Err(error) => {
            set_ready(shared, Some(error));
            return;
        }
    };
    // Warm-up on a blank picture, so the first real picture does not pay for the
    // runtime's first-run work. A failure here shows up on the first real picture.
    let blank = Pending {
        timestamp: Timestamp::ZERO,
        size,
        bgra: vec![0; size.width as usize * size.height as usize * 4],
    };
    for _ in 0..WARM_UP_RUNS {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.run(&blank)));
        if !matches!(run, Ok(Ok(_))) {
            break;
        }
    }
    drop(blank);
    engine.reset_states();
    engine.strays = StrayFilter::new();
    set_ready(shared, None);
    loop {
        let pending = {
            let Ok(mut mailbox) = shared.mailbox.lock() else {
                return;
            };
            loop {
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                if mailbox.reset {
                    mailbox.reset = false;
                    engine.reset_states();
                }
                if let Some(pending) = mailbox.pending.take() {
                    break pending;
                }
                match shared.wake.wait(mailbox) {
                    Ok(guard) => mailbox = guard,
                    Err(_) => return,
                }
            }
        };
        // A panic in the runtime must not leave the session silent: it becomes an error
        // the filter can show (rule 8).
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.run(&pending)))
                .unwrap_or(Err(SegmentError::DeviceLost));
        if let Ok(mut mailbox) = shared.mailbox.lock() {
            if mailbox.free.len() < BUFFERS {
                mailbox.free.push(pending.bgra);
            }
            match outcome {
                Ok(matte) => mailbox.matte = Some(matte),
                Err(error) => mailbox.failed = Some(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            mailbox: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            readied: Condvar::new(),
            stop: AtomicBool::new(false),
        })
    }

    #[test]
    fn warm_does_not_wait_past_its_limit_for_a_worker_that_is_not_ready() {
        assert!(!wait_ready(&shared(), Duration::ZERO));
    }

    #[test]
    fn warm_returns_once_the_worker_is_ready_or_failed() {
        let ready = shared();
        let worker = Arc::clone(&ready);
        let thread = std::thread::spawn(move || set_ready(&worker, None));
        // Hang guard only: the wait ends when the worker signals.
        assert!(wait_ready(&ready, Duration::from_secs(20)));
        thread.join().unwrap();

        let failed = shared();
        set_ready(&failed, Some(SegmentError::Unavailable));
        assert!(wait_ready(&failed, Duration::ZERO));
        let mailbox = failed.mailbox.lock().unwrap();
        assert!(!mailbox.ready);
        assert_eq!(mailbox.failed, Some(SegmentError::Unavailable));
    }
}
