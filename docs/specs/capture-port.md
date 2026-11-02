# Spec: Video capture port

- Status: Draft
- Owner crate(s): `rbs-capture` (port), adapters `rbs-capture-*`
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0004](../adr/0004-single-master-clock.md), [0005](../adr/0005-gpu-compositing-with-wgpu.md), [0013](../adr/0013-zero-cost-when-unused.md), [0018](../adr/0018-media-frame-representation.md)
- Related specs: [backend-ports.md](backend-ports.md), [media-types.md](media-types.md), [media-graph.md](media-graph.md)

## Purpose

The port through which RBS gets video from the operating system: displays (screens), windows and cameras. Every OS capture API (Windows Graphics Capture, DXGI desktop duplication, Media Foundation, ScreenCaptureKit, AVFoundation, the PipeWire screen-cast portal, X11, V4L2) is an adapter behind this one contract, so adapters can be added, replaced and chosen at runtime by capability.

## Scope

In scope:

- Discovering capture targets (displays, windows, cameras), or asking the system picker for one where the OS does not allow listing, and the permission state for each kind.
- Opening a capture session on one target, starting, stopping and reconfiguring it, and pulling frames from it.
- Frame hand-off: master-clock timestamps, sequence numbers, GPU-resident frames by native handle on the consumer's device, CPU frames as a fallback.
- Errors, capability keys and the conformance suite every capture adapter passes.

Out of scope:

- Audio input and application or system audio loopback: `rbs-audio-io` port.
- Importing native GPU surfaces into `wgpu` textures: `rbs-gpu` (ADR-0005). Capture adapters never depend on `wgpu`.
- Compressed camera modes (MJPEG, H.264). Most USB webcams deliver 1080p30 and above only compressed, so this limits early cameras to their uncompressed modes (typically up to 720p30, or 1080p at low rates over USB 2). They will be added in a later minor version as compressed `CameraMode`s delivered as packets to a decoder node through the codec port; the cost is one decode per camera frame, on the GPU where the platform allows.
- Media files, browser sources, remote guests: separate source node types, not OS capture.
- Device hot-plug watching: one shared watcher owned by the registry (ADR-0013 rule 2). The port only offers `enumerate` on demand.
- Cropping (including region capture), scaling and colour conversion: compositor and filters.

## Contract

Media types (`VideoFormat`, `PixelFormat`, `ColorSpace`, `Size`, `MemoryKind`, `VideoFrame`, `SystemFrame`, `NativeSurface`, `SurfaceSync`, `Lease`, `LeaseOwner`) come from `rbs_core::media`, and `FrameRate`, `Cadence`, `DriftEstimator` and `ClockMapping` from `rbs_core::timing` and `rbs_core::time` ([media-types.md](media-types.md)). This port does not define its own copies. The Rust types in `rbs_capture` are the source of truth; the signatures below are kept in sync with them.

### Targets and permissions

```rust
#[non_exhaustive] pub enum TargetKind { Display, Window, Camera }

/// Opaque, adapter-defined id. Stable for the lifetime of the target; for displays
/// and cameras also across restarts where the OS allows it, because scenes store it.
/// Ids returned by `pick` carry the OS restore token (PipeWire portal restore token,
/// macOS content filter archive), so `open` can reuse them without a new prompt.
pub struct TargetId(String);

#[non_exhaustive]
pub struct CaptureTarget { pub id, pub kind, pub name: String,
    pub size: Option<(u32, u32)>, pub modes: Vec<CameraMode> }   // + new / with_size / with_modes

#[non_exhaustive]
pub struct CameraMode { pub width, pub height, pub pixel_format,
    pub min_frame_rate: FrameRate, pub max_frame_rate: FrameRate } // + fixed / ranged

#[non_exhaustive] pub enum Permission { Granted, Denied, NotDetermined }
```

### Factory

```rust
pub trait CaptureFactory: AdapterFactory {
    /// Targets of `kind` available now. Not real-time; up to 100 ms. Never prompts:
    /// returns PermissionNotDetermined / PermissionDenied instead. Unsupported for
    /// kinds only offered through `pick`. Keeps no resources afterwards.
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError>;

    /// A session in Standby. Never prompts and never activates the device (a camera's
    /// light stays off): no threads, OS streams, device handles or GPU resources
    /// until `start`. Negotiates the format from what enumerate/pick reported.
    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError>;

    /// Shows the OS picker and blocks until the user chooses (control thread only).
    /// Cancel returns PermissionDenied. Default: Unsupported.
    fn pick(&self, kind: TargetKind) -> Result<CaptureTarget, CaptureError>;

    /// Current permission state; fast, never prompts. Default: Granted.
    fn permission(&self, kind: TargetKind) -> Permission;

    /// Shows the OS permission prompt if not determined; blocks (control thread only).
    fn request_permission(&self, kind: TargetKind) -> Result<Permission, CaptureError>;
}

#[non_exhaustive]
pub struct CaptureRequest {
    pub target: TargetId,
    pub pixel_formats: Vec<PixelFormat>,   // best first; first natively deliverable wins
    pub memory: Vec<MemoryKind>,           // best first; same rule
    pub gpu: Option<GpuImportTarget>,      // required for any GPU memory kind
    pub frame_rate: Option<FrameRate>,     // displays/windows: upper bound; cameras: clamped to the mode range
    pub size: Option<Size>,                // cameras only
    pub cursor: bool,
    pub clock: Arc<dyn Clock>,             // master clock (ADR-0004)
}   // + new / with_* builders

/// Where native frames are imported; filled in by rbs-gpu (P4).
#[non_exhaustive]
pub struct GpuImportTarget {
    pub device: GpuDeviceId,               // Luid | Uuid | DrmRenderNode { major, minor } | MetalRegistryId
    pub dmabuf_modifiers: Vec<(u32, u64)>, // accepted (fourcc, modifier); empty = linear only
    pub sync: Vec<SyncKind>,               // Ready | KeyedMutex | TimelineFence | SyncFile, best first
}
```

Zero-copy negotiation: a GPU memory kind is only delivered when the adapter can produce surfaces the device in `gpu` can open directly (same adapter LUID, same DRM device, an accepted modifier, an accepted sync primitive). Otherwise that memory kind is skipped in the request order, and if nothing is left `open` returns `FormatUnsupported`. On Windows Graphics Capture this means creating the frame pool on a D3D11 device of the same adapter and handing out shared NT handles with a keyed mutex or fence; on PipeWire it means offering exactly the importer's modifiers in the stream negotiation.

### Session

```rust
pub trait CaptureSession: Send {
    fn format(&self) -> VideoFormat;                       // changes only on resize (invariant 6)
    fn start(&mut self) -> Result<(), CaptureError>;       // Standby -> Active, may block up to 500 ms; no-op when Active
    fn stop(&mut self);                                    // Active -> Standby, idempotent
    fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError>; // REAL-TIME
    fn stats(&self) -> CaptureStats;                       // delivered, dropped, cpu_copied
    fn reconfigure(&mut self, change: CaptureChange) -> Result<(), CaptureError>; // Cursor | FrameRate; default Unsupported
}
```

Dropping a session releases everything it holds, including what `start` acquired. `CaptureStats` is non-exhaustive; adapters start from `CaptureStats::default()`.

`latest_frame` rules (real-time, called by the render thread on every tick):

- No allocation, no freeing, no blocking, no I/O, no logging, no locks of any kind. The hand-off from the adapter's capture thread or OS callback is lock-free, for example an atomic latest-slot swap over a pre-allocated slot ring.
- It returns the newest frame received since the previous call. Frames it replaced are counted in `dropped`; their OS objects (WGC `Direct3D11CaptureFrame`, `CVPixelBuffer`, PipeWire buffers) are released by the producer's own thread, never inside this call.
- `LeaseOwner::release` on a capture surface does not free either; it marks the slot free. The engine keeps its own reference to the adapter's lease owner until the session has been dropped on a background thread, so the last reference is never dropped on a real-time thread (media types spec, lifetime).
- Native frames have exactly one consumer, the `rbs-gpu` importer, which releases the lease only after its GPU work on the surface has completed.

### Invariants

Each is checked by the conformance suite, with the hook it needs in brackets. Without the hook the check is skipped and reported; with `strict` (hardware runners) a skip fails the run.

1. **Zero cost before start.** A factory and an opened but not started session own no threads, OS capture streams, device handles or GPU resources (P13). [`resources`]
2. **Master time, at capture.** Every frame's timestamp is the capture instant in master time, converted at the edge from the OS timestamp (QPC / `SystemRelativeTime`, `mach_absolute_time` / host time, `CLOCK_MONOTONIC`, V4L2 buffer time) with a `DriftEstimator` fed by `(OS time, clock.now())` pairs taken back to back, and its `ClockMapping`. Timestamps are kept monotonic (`max(previous + 1 ns, mapped)`) and are never ahead of `clock.now()` at delivery (`min(mapped, now)`). Stamping at delivery time instead of capture time is a bug even though the suite cannot always tell; the reference adapter has its own offset and drifting clock domain and a unit test that bounds the conversion error.
3. **Monotonic and accounted.** Timestamps and sequence numbers of delivered frames strictly increase within a session, across `stop` and `start`. `sequence` counts frames received from the OS; every skipped sequence number is either counted in `dropped` or was delivered to another call. Frames the OS drops internally (for example inside a WGC frame pool) are invisible to the adapter and not counted.
4. **Format honoured.** Every delivered frame matches `format()` (size, pixel format, colour) and its memory kind is in the request. The pixel format and memory kind are the first entries of the request lists the adapter supports for that target.
5. **Latest wins.** After a pause, the frame returned is the newest one: its age (`now - timestamp`) is at most `max_latency` (default 100 ms).
6. **Size changes are visible in the frame.** A resized window or display produces frames with the new `format.size`; `format()` returns the new format from the first such frame on. Consumers detect the change by comparing formats. Pixel format and memory kind never change mid-session. [`resize`]
7. **Target loss is an error, not silence.** A closed window or unplugged display or camera makes `latest_frame` return `Err(TargetLost)` within `frame_timeout`. [`remove`]
8. **Stop and drop release.** After `stop`, and after dropping the session, resources return to the level before `open` (ADR-0013 rule 5). `start` after `stop` delivers frames again. [`resources`]
9. **Frame lifetime.** The consumer can hold up to `capture.frames.in_flight` frames. Holding fewer never stops delivery; holding all of them makes the adapter drop new frames (counted) and `latest_frame` still returns promptly.
10. **Declared means real.** Every declared pixel format and memory kind is delivered by at least one enumerable target.

### Capability keys

Constants in `rbs_capture::caps`.

| Key | Value | Meaning |
| --- | --- | --- |
| `capture.target.kinds` | Set of `display`, `window`, `camera` | Kinds `open` accepts |
| `capture.target.enumerate` | Set of kinds | Kinds `enumerate` can list |
| `capture.target.picker` | Set of kinds | Kinds `pick` supports |
| `capture.pixel_formats` | Set of `PixelFormat::as_str` names | Formats the adapter can deliver natively |
| `capture.memory` | Set of `MemoryKind::as_str` names | Memory kinds frames can be delivered in |
| `capture.cursor` | Flag | Cursor can be composited by the OS |
| `capture.frame_rate.max` | Int (frames per second) | Highest rate the adapter can deliver |
| `capture.frames.in_flight` | Int | Frames the consumer may hold at once |
| `capture.hdr` | Flag | Can deliver HDR (`rgba16f`, PQ or scRGB) for HDR displays |
| `adapter.hardware` | Flag | `true` for OS/GPU capture paths (from `rbs-core`) |

Selection example: "display capture, zero-copy on D3D11" is `capture.target.kinds includes display` and `capture.memory includes d3d11`; whether frames are zero-copy on the engine's device is decided at `open` by `GpuImportTarget`.

## Errors and failure behaviour

```rust
#[non_exhaustive]
pub enum CaptureError {
    Unsupported(&'static str),     // kind, call or feature not offered
    TargetNotFound,                // id no longer exists
    TargetLost,                    // target went away while Active
    PermissionDenied,              // user or OS refused (macOS privacy, portal cancel)
    PermissionNotDetermined,       // call request_permission first
    FormatUnsupported,             // no requested format / memory kind / shareable device
    DeviceBusy,                    // camera opened exclusively elsewhere
    Backend { domain: ErrorDomain, code: i64, recoverable: bool }, // HResult | Errno | OsStatus | Library
}
impl CaptureError { pub const fn is_recoverable(&self) -> bool; }
```

- `TargetLost` and `DeviceBusy` are recoverable: the source node shows its last good frame (P7) and the engine retries with back-off. Permission errors are never retried automatically; they are surfaced to the user.
- Errors carry no heap data, so building them never allocates on the render thread.
- Adapters never panic across the port boundary; the host wraps calls with `catch_unwind` as a last resort.

## Evolution and semver

The port follows semver like the plugin SDK (ADR-0003).

- Enums and structs are `#[non_exhaustive]` and have constructors or builders, so variants (a new `TargetKind` such as per-application capture, a new error) and fields (HDR metadata) can be added in a minor version.
- New trait methods come with a default implementation that returns `Unsupported` (or the conservative answer, like `Permission::Granted` for adapters without a permission model), so existing adapters keep compiling. Changing or removing a method is a major version.
- New capability keys are minor; changing the meaning of a key is major.

## Performance requirements

- `latest_frame` is on the render loop and listed in [hot paths](../architecture/hot-paths.md). Its rules are above. The benchmark `cargo bench -p rbs-capture-test --bench latest_frame` measures the port's per-frame overhead with the reference adapter. First measurement (release build, cloud container, 2026-11-02): about 230 ns per delivered frame including the drift estimate and dropping the frame, about 40 ns when there is no new frame.
- GPU adapters deliver frames in GPU memory whenever the consumer accepts it; system-memory delivery is counted in `cpu_copied`.
- Glass-to-delivery latency for display capture: at most one frame interval on the reference machines ([performance-budgets.md](performance-budgets.md)).
- Idle cost: Installed adapters cost nothing; opened sessions in Standby own nothing (invariant 1), checked by the suite and by the idle-cost checks of the performance budgets.

## Pull, not push

The render tick pulls with `latest_frame`; adapters do not push frames into the graph.

- **Chosen: pull.** The compositor needs exactly one frame per source per tick, the newest one (ADR-0004: a late source contributes its previous frame and never blocks the tick). Pull makes that the only thing the hot path does, keeps OS callback threads out of the graph, and lets a source with a rate above the canvas rate cost nothing extra on the render thread.
- **Alternative: push callback** (the adapter calls into the graph per frame). It would run graph code on OS threads with unknown priority, need a queue per source anyway, and invite allocation and locking in callbacks.
- `latest_frame` takes no tick time: the render thread calls it at its tick, so "newest at the moment of the call" equals "newest at or before the tick" up to the tick's own scheduling delay. A consumer that needs frames for an earlier time (for example frame-accurate replay) uses a separate buffering node, not a port change. This answers the pull-vs-push open question of the media graph spec for capture sources.

## Conformance tests

`rbs_capture::conformance::run(&factory, &mut pacer, &options) -> Result<Report, Failure>`.

- `pacer`: `TestPacer` for software adapters (a `TestClock` that advances on each wait, so runs are deterministic), `RealTimePacer` for hardware adapters, bounded by `options.frame_timeout`.
- `options.measure_allocations`: runs a closure and returns the allocations plus frees it made on the calling thread (adapter tests use `allocation-counter`). Without it the allocation check is skipped.
- `options.hooks: Option<&dyn TestHooks>`: `stimulate(id)` (change content, for APIs that only deliver frames on change; called before every poll), `resize(id, w, h)`, `remove(id)` and `resources()` (threads, handles and GPU allocations the adapter owns). Hardware tests implement them with a helper window the test owns.
- `options.strict`: every skip is a failure. Self-hosted hardware runners use it.

Checks: probe within 50 ms and never panics; `enumerate` agrees with `capture.target.enumerate` and returns only the asked kind; `pick` returns `Unsupported` when not declared (declared pickers need a user and are skipped); unknown id returns `TargetNotFound`; every declared pixel format and memory kind is delivered, and request order decides both; then for each enumerable target: no frames and no resources in Standby, timestamps and sequence numbers per invariants 2 and 3, latest wins after a pause, no allocation or free in `latest_frame` plus drop, frames in flight, resize, stop, restart, resources back to baseline after stop and after drop, and finally target loss.

`rbs-capture-test` runs the suite strictly against the reference adapter and against wrappers that each break one invariant (allocates per frame, frames before start, delivers after stop, hides drops, queues behind the newest frame, leaks a frame on drop, ignores target loss, stale format after resize) to prove each check fails.

## Adapters planned

| Adapter crate | Platform | Targets | Memory | Phase |
| --- | --- | --- | --- | --- |
| `rbs-capture-test` | all | synthetic display, window, camera | system | 0 (CI and conformance reference) |
| `rbs-capture-wgc` | Windows 10 1903+ | display, window | d3d11 | 0 |
| `rbs-capture-dxgi` | Windows | display | d3d11 | 1 (fallback) |
| `rbs-capture-mf` | Windows | camera | system, d3d11 | 2 |
| `rbs-capture-sck` | macOS 12.3+ | display, window (picker on 14+) | metal (IOSurface) | 0 prototype |
| `rbs-capture-avf` | macOS | camera | metal (IOSurface) | 2 |
| `rbs-capture-pipewire` | Linux (Wayland, X11 via portal) | display, window (picker) | dmabuf, system | 0 prototype |
| `rbs-capture-v4l2` | Linux | camera | system, dmabuf | 2 |

## Review

- Reviewer: independent AI session (fresh context; given only the diff, principles, related ADRs, specs and code)
- Date: 2026-11-02
- Verdict: changes requested, then addressed in the same PR
- Concerns: (1) zero-copy could not be negotiated without the importing device -> `GpuImportTarget` with device id, DMA-BUF modifiers and sync kinds; a device mismatch is `FormatUnsupported`; (2) picker and permission flows did not fit `open` -> `pick`, `permission`, `request_permission`, restore tokens in ids, `open` and `enumerate` never prompt; (3) clock conversion untested -> rules in invariant 2, reference adapter has its own drifting clock domain with a conversion-error test, pull rationale for the missing tick time; (4) real-time rules weaker than P6 -> no locks at all, no freeing, OS objects released on the producer thread, frees counted; (5) suite did not check what the spec claimed -> resource, resize and remove hooks, in-flight, latest-wins age, exact drop accounting, memory order, leak check after drop, strict mode, eight negative wrappers; (6) WGC delivers only on change -> `stimulate` hook, `sequence` defined as frames received from the OS; (7) semver unstated -> non-exhaustive types with builders, default trait methods, `reconfigure`, `ErrorDomain`, semver section; (8) camera model -> frame-rate ranges, `open` must not activate the device, compressed modes out of scope with the cost stated; (9) doc gaps -> budget row added, media types spec exists (PR #7), `latest_frame` benchmark, pull vs push section.

## Open questions

- Per-application capture (all windows of a process, as ScreenCaptureKit offers): a new `TargetKind` in a minor version.
- How the registry's device watcher tells consumers that the target list changed (event type lives in the registry spec).
- Whether the latest-slot hand-off primitive should move to `rbs-core` once a second hardware adapter needs it.
