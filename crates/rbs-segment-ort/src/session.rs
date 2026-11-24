//! One session: a worker thread that owns the ONNX Runtime session and turns the newest
//! submitted small picture into a matte. The render thread only touches a one-slot
//! mailbox with `try_lock`, so it never waits for the worker (principle P6).
//!
//! Zero cost when unused (P13): no thread exists before `warm`, and `close` joins it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use ort::environment::Environment;
use ort::session::Session;
use ort::value::Tensor;
use rbs_core::media::{FramePayload, PixelFormat, Size, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_segment::{Matte, MattePlane, PersonMaskSession, SegmentError, SubmitOutcome};

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
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    stop: AtomicBool,
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
        // The worker makes the session itself (DirectML compiles shaders for seconds the
        // first time), so `warm` returns at once and early pictures wait in the mailbox.
        let handle = std::thread::Builder::new()
            .name("rbs-segment-onnx".into())
            .spawn(move || worker_main(&shared, spec, &model, &runtime))
            .map_err(|_| SegmentError::Unavailable)?;
        self.worker = Some(handle);
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
        Ok(Matte::new(
            pending.timestamp,
            Size::new(dw, dh),
            MattePlane::Cpu(Arc::from(self.bytes.as_slice())),
        ))
    }
}

fn worker_main(shared: &Shared, spec: Spec, model: &[u8], runtime: &Path) {
    let engine = init_runtime(runtime)
        .and_then(|env| make_session(env, model))
        .map(|session| Engine::new(session, spec));
    let mut engine = match engine {
        Ok(engine) => engine,
        Err(error) => {
            if let Ok(mut mailbox) = shared.mailbox.lock() {
                mailbox.failed = Some(error);
            }
            return;
        }
    };
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
