# Spec: Video capture port

- Status: Draft
- Owner crate(s): `rbs-capture` (port), adapters `rbs-capture-*`
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0004](../adr/0004-single-master-clock.md), [0005](../adr/0005-gpu-compositing-with-wgpu.md), [0013](../adr/0013-zero-cost-when-unused.md)
- Related specs: [backend-ports.md](backend-ports.md), [media-types.md](media-types.md), [media-graph.md](media-graph.md)

## Purpose

The port through which RBS gets video from the operating system: displays (screens), windows and cameras. Every OS capture API (Windows Graphics Capture, DXGI desktop duplication, Media Foundation, ScreenCaptureKit, AVFoundation, the PipeWire screen-cast portal, X11, V4L2) is an adapter behind this one contract, so adapters can be added, replaced and chosen at runtime by capability.

## Scope

In scope:

- Discovering capture targets (displays, windows, cameras), or asking the system picker for one where the OS does not allow listing.
- Opening a capture session on one target, starting and stopping it, and pulling frames from it.
- Frame hand-off: master-clock timestamps, sequence numbers, GPU-resident frames by native handle, CPU frames as a fallback.
- Errors, capability keys and the conformance suite every capture adapter passes.

Out of scope:

- Audio input and application or system audio loopback: `rbs-audio-io` port.
- Importing native GPU surfaces into `wgpu` textures: `rbs-gpu` (ADR-0005). Capture adapters never depend on `wgpu`.
- Media files, browser sources, remote guests: separate source node types, not OS capture.
- Device hot-plug watching: one shared watcher owned by the registry (ADR-0013 rule 2). The port only offers `enumerate` on demand.
- Cropping, scaling and colour conversion: compositor and filters.

## Contract

All media types (`VideoFormat`, `PixelFormat`, `ColorSpace`, `Size`, `MemoryKind`, `VideoFrame`, `Lease`) come from `rbs_core::media` and `FrameRate` from `rbs_core::timing` ([media-types.md](media-types.md)). This port does not define its own copies. The Rust types in `rbs_capture` are the source of truth; the signatures below are kept in sync with them.

### Targets

```rust
pub enum TargetKind { Display, Window, Camera }

/// Opaque, adapter-defined id. Stable for the lifetime of the target; for displays
/// and cameras it should also be stable across restarts where the OS allows it,
/// because scenes store it.
pub struct TargetId(String);

pub struct CaptureTarget {
    pub id: TargetId,
    pub kind: TargetKind,
    /// Human readable name: monitor name, window title, camera model.
    pub name: String,
    /// Native size in pixels when known (displays, windows).
    pub size: Option<(u32, u32)>,
    /// Supported modes for cameras; empty for displays and windows.
    pub modes: Vec<CameraMode>,
}

pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub frame_rate: FrameRate,
}

pub enum TargetSelector {
    /// A target returned by `enumerate`.
    Id(TargetId),
    /// Let the operating system show its own picker (PipeWire portal, macOS
    /// content picker). Only valid when `capture.target.picker` includes the kind.
    Picker(TargetKind),
}
```

### Factory

```rust
pub trait CaptureFactory: AdapterFactory {
    /// Lists the targets of `kind` available now. Not real-time; may take up to
    /// 100 ms. Returns `Err(Unsupported)` for kinds the adapter only offers
    /// through the picker. Allocates no long-lived resources.
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError>;

    /// Creates a session for one target. The session starts in Standby: no
    /// frames, no capture threads, no OS capture stream. It may validate the
    /// target and keep a lightweight reference to it.
    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError>;
}

pub struct CaptureRequest {
    pub target: TargetSelector,
    /// Formats the consumer can use, best first. The adapter picks the first it
    /// can deliver natively; if none, it fails with `FormatUnsupported`.
    pub pixel_formats: Vec<PixelFormat>,
    /// Memory kinds the consumer can import, best first (for example `[D3D11, System]`).
    pub memory: Vec<MemoryKind>,
    /// Desired frame rate; `None` means the target's native rate. Displays and
    /// windows treat it as an upper bound; cameras pick the closest mode.
    pub frame_rate: Option<FrameRate>,
    /// Requested camera size; ignored for displays and windows.
    pub size: Option<Size>,
    /// Draw the cursor into the frame (displays and windows only).
    pub cursor: bool,
    /// The master clock to stamp frames with (ADR-0004).
    pub clock: Arc<dyn Clock>,
}
```

### Session

```rust
pub trait CaptureSession: Send {
    /// The negotiated output format. Changes only when the target is resized
    /// (invariant 6).
    fn format(&self) -> VideoFormat;

    /// Standby -> Active: start the OS capture stream. May allocate, start
    /// threads and block briefly (up to 500 ms; the media graph calls it during
    /// warm-up, never from the render tick).
    fn start(&mut self) -> Result<(), CaptureError>;

    /// Active -> Standby: stop the stream and release threads, pools and GPU
    /// resources started by `start`. Idempotent. Frames still held by the
    /// consumer stay valid until dropped.
    fn stop(&mut self);

    /// REAL-TIME. Called from the render thread on each tick. Returns the newest
    /// frame produced since the previous call, or `None` if there is none.
    /// Older undelivered frames are released and counted as dropped. Never
    /// blocks, never allocates, never does I/O.
    fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError>;

    /// Counters for diagnostics. Cheap; may be called from any tick.
    fn stats(&self) -> CaptureStats;
}

pub struct CaptureStats {
    /// Frames returned by `latest_frame`.
    pub delivered: u64,
    /// Frames produced by the OS but replaced by a newer one before delivery.
    pub dropped: u64,
    /// Frames copied to system memory because zero-copy was not possible (P4).
    pub cpu_copied: u64,
}
```

Dropping a session releases everything it holds, including what `start` acquired.

### Invariants (each is checked by the conformance suite)

1. **Zero cost before start.** A factory and an opened but not started session own no threads, OS capture streams or GPU resources (P13).
2. **Master time.** Every frame's timestamp is in master-clock time from the request's clock, converted at the edge with `rbs_core::time::ClockMapping`; it never lies in the future of `clock.now()` at delivery.
3. **Monotonic.** Timestamps and sequence numbers of delivered frames strictly increase within a session. A gap in sequence numbers equals the frames counted as dropped.
4. **Format honoured.** Every delivered frame matches `format()` (size, pixel format, memory kind) except as allowed by invariant 6. The pixel format and memory kind are the first entries of the request lists the adapter supports.
5. **Latest wins.** `latest_frame` never returns a frame older than one already returned, and never queues more than the adapter's declared `capture.frames.in_flight` frames.
6. **Size changes are visible in the frame.** A window or display that changes size produces frames whose `format.size` is the new size; `format()` returns the new format from the first such frame on. Consumers detect the change by comparing `frame.format` with the previous frame. Pixel format and memory kind never change mid-session.
7. **Target loss is an error, not a hang.** A closed window, unplugged display or camera makes `latest_frame` return `Err(TargetLost)`; the session can be dropped or reopened.
8. **Stop releases.** After `stop`, thread count and GPU allocations return to the level before `start` (ADR-0013 rule 5). `start` after `stop` works.
9. **Frame lifetime.** A frame that needs lifetime tracking carries a `rbs_core::media::Lease`; the adapter implements `LeaseOwner` and does not reuse a surface until every clone of its frame is dropped. When consumers hold all `capture.frames.in_flight` surfaces, the adapter drops new frames (counted in `dropped`) rather than blocking. Native surfaces carry their `SurfaceSync` (keyed mutex, fence, sync file) so `rbs-gpu` can wait before reading.
10. **Declared means real.** Every target kind, pixel format and memory kind declared in capabilities can be opened and delivers at least one frame.

### Capability keys

Constants in `rbs_capture::caps`.

| Key | Value | Meaning |
| --- | --- | --- |
| `capture.target.kinds` | Set of `display`, `window`, `camera` | Kinds `open` accepts |
| `capture.target.enumerate` | Set of kinds | Kinds `enumerate` can list |
| `capture.target.picker` | Set of kinds | Kinds selectable through `TargetSelector::Picker` |
| `capture.pixel_formats` | Set of `PixelFormat` names | Formats the adapter can deliver natively |
| `capture.memory` | Set of `MemoryKind` names | Memory kinds frames can be delivered in |
| `capture.cursor` | Flag | Cursor can be composited by the OS |
| `capture.frame_rate.max` | Int (frames per second) | Highest rate the adapter can deliver |
| `capture.frames.in_flight` | Int | Frames the consumer may hold at once |
| `capture.hdr` | Flag | Can deliver HDR (`rgba16f`, PQ or scRGB) for HDR displays |
| `adapter.hardware` | Flag | `true` for OS/GPU capture paths (from `rbs-core`) |

Selection example: "display capture, zero-copy on D3D11" is `capture.target.kinds includes display` and `capture.memory includes d3d11`.

## Errors and failure behaviour

```rust
pub enum CaptureError {
    Unsupported(&'static str),     // kind, selector or feature not offered
    TargetNotFound,                // id no longer exists
    TargetLost,                    // target went away while Active
    PermissionDenied,              // user or OS refused (macOS TCC, portal cancel)
    FormatUnsupported,             // none of the requested formats or memory kinds
    DeviceBusy,                    // camera opened exclusively elsewhere
    Backend { code: i64, recoverable: bool }, // OS error code, no allocation
}
impl std::error::Error for CaptureError {}
impl CaptureError { pub fn is_recoverable(&self) -> bool; }
```

- `TargetLost` and `DeviceBusy` are recoverable: the source node goes to its last good frame (P7) and the engine retries with back-off. `PermissionDenied` is not retried automatically; it is surfaced to the user.
- Errors returned from `latest_frame` carry no heap data, so building them never allocates on the render thread.
- Adapters never panic across the port boundary; the host wraps calls with `catch_unwind` as a last resort.

## Performance requirements

- `latest_frame` is on the render loop and listed in [hot paths](../architecture/hot-paths.md): no allocation, no locks that the capture thread can hold for longer than a pointer swap, no I/O, no logging.
- GPU adapters deliver frames in GPU memory (`MemoryKind` other than `System`) whenever the consumer accepts it; system-memory delivery is counted in `cpu_copied`.
- Glass-to-delivery latency for display capture: at most one frame interval on the reference machines (budget in [performance-budgets.md](performance-budgets.md)).

## Conformance tests

`rbs_capture::conformance::run(&factory, &mut pacer, &options) -> Result<Report, Failure>` runs the checks below for every declared target kind with the first enumerable target (or skips with a message when there is none, for example no camera or no display on a CI runner). `pacer` advances time between polls: tests of software adapters pass `TestPacer` (a `TestClock` that advances on each wait, so runs are deterministic); hardware adapters on self-hosted runners use `RealTimePacer` with the bounded `options.frame_timeout`. `options.thread_allocations` is a per-thread allocation counter the adapter's test binary provides through a counting global allocator; without it the allocation check is skipped and reported as skipped.

`rbs-capture-test` also runs the suite against deliberately broken wrappers to prove each check fails when its invariant is broken.

- Probe returns within 50 ms and never panics; an empty capability set means every other check is skipped.
- `enumerate` agrees with `capture.target.enumerate`; unsupported kinds return `Unsupported`.
- Open with each declared pixel format and memory kind; the negotiated format matches the request order (invariant 4).
- No frames before `start`; frames after `start` (invariant 1, 10).
- Timestamps in master time, not ahead of the clock, strictly increasing; sequence gaps equal `dropped` (invariants 2, 3).
- Polling slower than the frame rate returns only the newest frame and counts the rest as dropped (invariant 5).
- `stop`, then `start` again delivers frames; stop is idempotent (invariant 8).
- Unknown target id returns `TargetNotFound`; picker selector without `capture.target.picker` returns `Unsupported`.
- `latest_frame` does not allocate (allocation guard), once the session has delivered its first frame.

## Adapters planned

| Adapter crate | Platform | Targets | Memory | Phase |
| --- | --- | --- | --- | --- |
| `rbs-capture-test` | all | synthetic display, window, camera | system | 0 (CI and conformance reference) |
| `rbs-capture-wgc` | Windows 10 1903+ | display, window | d3d11 | 0 |
| `rbs-capture-dxgi` | Windows | display | d3d11 | 1 (fallback) |
| `rbs-capture-mf` | Windows | camera | system, d3d11 | 2 |
| `rbs-capture-sck` | macOS 12.3+ | display, window | metal (IOSurface) | 0 prototype |
| `rbs-capture-avf` | macOS | camera | metal (IOSurface) | 2 |
| `rbs-capture-pipewire` | Linux (Wayland, X11 via portal) | display, window (picker) | dmabuf, system | 0 prototype |
| `rbs-capture-v4l2` | Linux | camera | system, dmabuf | 2 |

## Open questions

- Region capture (part of a display) as a capture feature or always as a compositor crop. Current answer: compositor crop; revisit if an OS API makes region capture cheaper.
- Per-application capture (all windows of a process, as ScreenCaptureKit offers): a fourth `TargetKind` or a selector option.
- How the registry's device watcher tells capture adapters' consumers that the target list changed (event type lives in the registry spec).
