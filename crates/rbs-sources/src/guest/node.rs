//! The guest source's nodes and its control handle (spec `guest-source.md`).
//!
//! A guest source is a picture node ([`GuestSourceNode`], render partition) and a
//! sound node (the browser sound node under [`GUEST_SOUND_NODE_TYPE`], audio
//! partition), fed by one worker. The host holds the [`GuestControl`]: it starts the
//! worker with the session's receiver once connected, tells it whether the source is
//! shown, and reads its state. Standby holds no thread, decoder or aligner (P13).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rbs_codec::AudioDecoder;
use rbs_core::media::{ColorSpace, FramePayload, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::time::Clock;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::SourceUpload;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};
use rbs_nodes::capture::CaptureNodeStats;
use rbs_peer::PeerReceiver;

use super::decoding::GuestDecoding;
use super::handoff::{GuestPictures, NodeSink};
use super::settings::GuestSettings;
use super::worker::{self, GuestCounts, GuestEnd, GuestWorker, MakeVideoDecoder, WorkerSettings};
use crate::browser_sound::{BrowserSound, BrowserSoundNode};

/// Type id of the guest source and of its picture node (ADR-0139 decision 12).
pub const GUEST_SOURCE_TYPE: &str = crate::types::GUEST_SOURCE;
/// Type id of its sound node (ADR-0106).
pub const GUEST_SOUND_NODE_TYPE: &str = "org.rbs.source.guest.sound";
/// The picture node's video output.
pub const GUEST_VIDEO_PIN: usize = 0;

/// The picture node's descriptor: one video output; the sound node has the audio.
pub fn guest_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(GUEST_SOURCE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// Why [`GuestControl::connect`] could not start the worker.
#[derive(Debug)]
pub enum GuestStartError {
    /// The Opus decoder could not be made, or no video decoder is registered.
    Decoder(rbs_codec::CreateError),
    /// The worker thread could not be spawned.
    Thread(std::io::Error),
}

impl std::fmt::Display for GuestStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decoder(e) => write!(f, "guest decoder: {e}"),
            Self::Thread(e) => write!(f, "guest worker thread: {e}"),
        }
    }
}

impl std::error::Error for GuestStartError {}

/// What the worker needs besides the receiver: the decoders, made by the host from
/// the studio's adapters.
pub struct GuestDecoders {
    /// Makes video decoder sessions (only while shown).
    pub video: MakeVideoDecoder,
    /// The Opus decoder.
    pub audio: Box<dyn AudioDecoder>,
}

struct Running {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Status {
    ended: Option<GuestEnd>,
    counts: GuestCounts,
}

/// The host's handle on one guest source: starts and stops its worker, sets whether
/// it is shown, and reads its state. Control thread.
pub struct GuestControl {
    n: u32,
    pictures: Arc<GuestPictures>,
    sound: Arc<BrowserSound>,
    clock: Arc<dyn Clock>,
    shown: Arc<AtomicBool>,
    running: Mutex<Option<Running>>,
    status: Arc<Mutex<Status>>,
    decoding: GuestDecoding,
    settings: GuestSettings,
}

impl GuestControl {
    fn new(
        n: u32,
        sound: Arc<BrowserSound>,
        clock: Arc<dyn Clock>,
        decoding: GuestDecoding,
        settings: GuestSettings,
    ) -> Self {
        Self {
            decoding,
            settings,
            n,
            pictures: GuestPictures::new(),
            sound,
            clock,
            shown: Arc::new(AtomicBool::new(true)),
            running: Mutex::new(None),
            status: Arc::new(Mutex::new(Status::default())),
        }
    }

    /// The source's settings, as read when it was made.
    pub fn settings(&self) -> &GuestSettings {
        &self.settings
    }

    /// Starts the worker on a connected session's receiver with the registered
    /// decoders and the source's jitter target (Active).
    pub fn connect(&self, receiver: Box<dyn PeerReceiver>) -> Result<(), GuestStartError> {
        let decoders = self.decoding.decoders().map_err(GuestStartError::Decoder)?;
        let settings = WorkerSettings {
            target: self.settings.jitter,
        };
        self.start(receiver, decoders, settings)
            .map_err(GuestStartError::Thread)
    }

    /// Starts the worker on a connected session's receiver (Active). A worker that
    /// runs is stopped first.
    pub fn start(
        &self,
        receiver: Box<dyn PeerReceiver>,
        decoders: GuestDecoders,
        settings: WorkerSettings,
    ) -> std::io::Result<()> {
        self.stop();
        if let Ok(mut s) = self.status.lock() {
            *s = Status::default();
        }
        let sink = NodeSink::new(Arc::clone(&self.pictures), Arc::clone(&self.sound));
        let worker = GuestWorker::new(
            receiver,
            Box::new(sink),
            Arc::clone(&self.clock),
            decoders.video,
            decoders.audio,
            settings,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::clone(&self.status);
        let join = worker::spawn(
            self.n,
            worker,
            Arc::clone(&self.clock),
            Arc::clone(&stop),
            Arc::clone(&self.shown),
            Box::new(move |end, counts| {
                if let Ok(mut s) = status.lock() {
                    s.ended = end;
                    s.counts = counts;
                }
            }),
        )?;
        if let Ok(mut r) = self.running.lock() {
            *r = Some(Running {
                stop,
                join: Some(join),
            });
        }
        Ok(())
    }

    /// Stops the worker and waits for it (it never blocks for long: one wait of at
    /// most 10 ms). Its decoders and aligner go with it; the last picture stays.
    pub fn stop(&self) {
        let running = self.running.lock().ok().and_then(|mut r| r.take());
        if let Some(mut r) = running {
            r.stop.store(true, Ordering::Relaxed);
            if let Some(join) = r.join.take() {
                let _ = join.join();
            }
        }
    }

    /// True while the worker thread runs.
    pub fn running(&self) -> bool {
        self.running
            .lock()
            .ok()
            .and_then(|r| {
                r.as_ref()
                    .and_then(|r| r.join.as_ref())
                    .map(|j| !j.is_finished())
            })
            .unwrap_or(false)
    }

    /// Why the last worker ended by itself (`guest_limits`, `guest_disconnected`).
    pub fn ended(&self) -> Option<GuestEnd> {
        self.status.lock().ok().and_then(|s| s.ended)
    }

    /// The last worker's counts, once it ended.
    pub fn counts(&self) -> GuestCounts {
        self.status.lock().map(|s| s.counts).unwrap_or_default()
    }

    /// Whether a shown scene draws the source (the binder's shown set).
    pub fn set_shown(&self, shown: bool) {
        self.shown.store(shown, Ordering::Relaxed);
    }
}

impl Drop for GuestControl {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The picture node: takes the newest due picture from the worker and draws it; keeps
/// the last one until the next.
pub struct GuestSourceNode {
    descriptor: NodeDescriptor,
    control: Arc<GuestControl>,
    upload: SourceUpload,
    /// A picture whose bytes were uploaded and that found no return slot yet.
    held: Option<VideoFrame>,
    stats: Arc<CaptureNodeStats>,
}

impl GuestSourceNode {
    /// The control handle.
    pub fn control(&self) -> Arc<GuestControl> {
        Arc::clone(&self.control)
    }
}

/// A guest source's two nodes and its control handle.
pub struct GuestSource {
    /// The picture node (render partition).
    pub node: GuestSourceNode,
    /// The sound node (audio partition).
    pub sound: BrowserSoundNode,
    /// The host's handle.
    pub control: Arc<GuestControl>,
    /// The picture node's counters and picture size, as a capture's.
    pub stats: Arc<CaptureNodeStats>,
}

/// Makes a guest source's nodes (Standby: no worker). `n` names its worker thread.
pub fn guest_source(
    n: u32,
    clock: Arc<dyn Clock>,
    audio: rbs_audio::AudioGraphConfig,
    decoding: GuestDecoding,
    settings: GuestSettings,
) -> GuestSource {
    let sound = BrowserSound::new(audio, Arc::clone(&clock));
    let control = Arc::new(GuestControl::new(
        n,
        Arc::clone(&sound),
        clock,
        decoding,
        settings,
    ));
    let stats = Arc::new(CaptureNodeStats::default());
    GuestSource {
        node: GuestSourceNode {
            descriptor: guest_descriptor(),
            control: Arc::clone(&control),
            upload: SourceUpload::new(),
            held: None,
            stats: Arc::clone(&stats),
        },
        stats,
        sound: BrowserSoundNode::new(sound).with_type(GUEST_SOUND_NODE_TYPE),
        control,
    }
}

impl Node<GpuRuntime> for GuestSourceNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // The worker starts when the host connects the session, not here.
        Ok(())
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // An NV12 1080p upload target, so the first system-memory picture does not
        // make one; other sizes are made when they come.
        let format = VideoFormat::new(
            Size::new(1920, 1080),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        );
        self.upload
            .prepare_for(ctx.shared, &mut ctx.local.pool, &format)
            .map_err(|_| NodeError::Backend("guest picture upload failed"))
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let pictures = &self.control.pictures;
        if let Some(f) = self.held.take()
            && let Err(f) = pictures.give_back(f)
        {
            self.held = Some(f);
        }
        let Some(frame) = pictures.take_due(ctx.tick().time) else {
            return Ok(());
        };
        let gpu = ctx.shared();
        let (_, outputs, local) = ctx.parts();
        let size = frame.format().size;
        let stats = &self.stats;
        let texture = match frame.payload() {
            // The importer keeps the lease until the GPU is done with the surface.
            FramePayload::Native { .. } => {
                stats.imported.fetch_add(1, Ordering::Relaxed);
                local.importer.import(gpu, &mut local.pool, frame)
            }
            FramePayload::System(_) => {
                // A second reference to the same bytes (no copy): the upload drops it,
                // and the last reference goes back to the worker below.
                let Some(copy) = frame.share() else {
                    return Ok(());
                };
                stats.uploaded.fetch_add(1, Ordering::Relaxed);
                let uploaded = self.upload.upload(gpu, &mut local.pool, copy);
                // The bytes go back to the worker, which frees them.
                if let Err(f) = pictures.give_back(frame) {
                    self.held = Some(f);
                }
                uploaded
            }
            _ => return Ok(()),
        };
        let Ok(texture) = texture else {
            stats.gpu_errors.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        drop(outputs.set_video(GUEST_VIDEO_PIN, Some(texture))?);
        stats.frames.fetch_add(1, Ordering::Relaxed);
        stats.picture_size.store(
            (u64::from(size.width) << 32) | u64::from(size.height),
            Ordering::Relaxed,
        );
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        self.upload.release(&mut ctx.local.pool);
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.held = None;
    }
}
