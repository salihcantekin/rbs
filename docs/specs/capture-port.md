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
    /// Cancel returns Cancelled (not a permission denial). Default: Unsupported.
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

Zero-copy negotiation: a GPU memory kind is only delivered when the adapter can produce surfaces the device in `gpu` can open directly (same adapter LUID, same DRM device, an accepted modifier, an accepted sync primitive). Otherwise that memory kind is skipped in the request order, and if nothing is left `open` returns `FormatUnsupported`. On Windows Graphics Capture this means creating the frame pool on a D3D11 device of the same adapter and handing out NT shared handles with a shared timeline fence (D3D12 importers cannot acquire keyed mutexes, see gpu-compositor.md, Native import); on PipeWire it means offering exactly the importer's modifiers in the stream negotiation.

Native surface rules for producers:

- **LUID encoding.** `GpuDeviceId::Luid` holds the eight bytes of the Windows `LUID` struct in memory order (`LowPart` then `HighPart`, both little-endian). The `adapter_luid: u64` of a `NativeSurface` is the same bytes read as a little-endian integer, that is `(HighPart as u32 as u64) << 32 | LowPart as u64`, the value `rbs_gpu::GpuContext::adapter_luid` reports.
- **Handle values.** Importers cache opened surfaces and fences by handle value, so a producer must not let a value it handed out name another object while an importer may still cache it. A surface whose handle was handed out in a frame stays open while any frame references it and for at least 10 s after the last reference ends (importers forget an idle cached handle within 8 s, whatever their tick rate), including after `stop` and after the session is dropped; the release happens off the render thread. A fence is never closed while the process runs; sessions reuse pooled fences and keep signalling increasing values, so a cached fence stays correct. Surfaces never handed out may be released at once. The importer also reopens a cached surface delivered with a different frame size and closes unused cached surfaces when a source stops (`Importer::close_unused`).
- **Surface budget.** Each session hands out at most `capture.frames.in_flight + 2` surfaces at a time (Windows Graphics Capture: 5), plus replaced surfaces during a resize. A resize that would keep more than 512 MiB of replaced surfaces waiting drops frames instead of allocating.

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
- `LeaseOwner::release` on a capture surface does not free either; it marks the slot free. When `stop` finds frames still held, the adapter keeps their slot ring alive until it is started again or dropped (the reference adapter) or until a thread of its own releases it once no frame references it (hardware adapters, see the native surface rules), so dropping the last held frame on the render thread never frees the ring there. A ring still referenced when the session is dropped must not be freed by the last lease on the render thread: hardware adapters hand it to their own thread; the reference adapter accepts that cost because it only runs in tests. The engine drops sessions on a background thread (media graph spec, live editing).
- Native frames have exactly one consumer, the `rbs-gpu` importer, which releases the lease only after its GPU work on the surface has completed.

### Invariants

Each is checked by the conformance suite, with the hook it needs in brackets. Without the hook the check is skipped and reported; with `strict` (hardware runners) a skip fails the run.

1. **Zero cost before start.** A factory and an opened but not started session own no threads, OS capture streams, device handles or GPU resources (P13). [`resources`]
2. **Master time, at capture.** Every frame's timestamp is the capture instant in master time, converted at the edge from the OS timestamp (QPC / `SystemRelativeTime`, `mach_absolute_time` / host time, `CLOCK_MONOTONIC`, V4L2 buffer time) with a `DriftEstimator` fed by `(OS time, clock.now())` pairs taken back to back, and its `ClockMapping`. Timestamps are kept monotonic (`max(previous + 1 ns, mapped)`) and are never ahead of `clock.now()` at delivery (`min(mapped, now)`). Stamping at delivery time instead of capture time is a bug even though the suite cannot always tell; the reference adapter has its own offset and drifting clock domain and a unit test that bounds the conversion error.
3. **Monotonic and accounted.** Timestamps and sequence numbers of delivered frames strictly increase within a session, across `stop` and `start`. `sequence` counts frames received from the OS; every skipped sequence number is either counted in `dropped` or was delivered to another call. The suite checks this once over the whole run after `stop`, when the adapter is quiet (`sequence span <= delivered + dropped`), because per-frame checks would race with the OS thread counting drops between `latest_frame` and `stats`. Frames the OS drops internally (for example inside a WGC frame pool) are invisible to the adapter and not counted.
4. **Format honoured.** Every delivered frame matches `format()` (size, pixel format, colour) and its memory kind is in the request. The pixel format and memory kind are the first entries of the request lists the adapter supports for that target.
5. **Latest wins.** After a pause, the frame returned is the newest one: its age at the call is at most one frame interval (the largest gap the suite observed between consecutive frames) plus `max_latency` (default 100 ms), and a second call right away returns nothing or a frame captured after the first call (`timestamp > time of the first call`, no slack), never an older queued one.
6. **Size changes are visible in the frame.** A resized window or display produces frames with the new `format.size`; `format()` returns the new format from the first such frame on. Consumers detect the change by comparing formats. Pixel format and memory kind never change mid-session. [`resize`]
7. **Target loss is an error, not silence.** A closed window or unplugged display or camera makes `latest_frame` return `Err(TargetLost)` within `frame_timeout`. [`remove`]
8. **Stop and drop release.** After `stop` (with no frames held), and after dropping the session, resources return to the level before `open` (ADR-0013 rule 5). Exception: native surfaces kept for the handle-value grace of the native surface rules are released by the adapter's own thread within that grace, and pooled shared fences (one per session that was Active at the same time) live for the process. `start` after `stop` delivers frames again. Dropping a frame held across `stop` neither allocates nor frees. [`resources`, `process_threads`]
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
    PermissionDenied,              // user or OS refused (macOS privacy, portal policy)
    PermissionNotDetermined,       // call request_permission first
    Cancelled,                     // user closed the picker; the UI must not show it as a denial
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

## Hand-off helpers

`rbs_capture::handoff` holds two platform-independent building blocks that hardware adapters share, so the rules above are implemented and tested once:

`rbs_capture::handoff` is an adapter-support module: it is versioned with the port crate but outside the port contract above, and may change in a minor version (third-party adapters that use it pin `rbs-capture` to a minor version). Its structs are still `#[non_exhaustive]` with constructors.

```rust
pub struct Ring;                 // fixed slots, each naming one surface by a u64 value
impl Ring {
    pub fn new(handles: &[u64]) -> Ring;               // allocates: at start, not per frame
    pub fn len(&self) -> usize;  pub fn is_empty(&self) -> bool;
    pub fn handle(&self, slot: usize) -> Option<u64>;
    pub fn claim(&self) -> Option<usize>;              // producer: a free slot
    pub fn set_handle(&self, slot: usize, handle: u64);// producer, claimed slot only
    pub fn publish(&self, slot: usize, meta: FrameMeta);// producer: newest wins, replaced = dropped
    pub fn abandon(&self, slot: usize);
    pub fn count_drop(&self);
    pub fn take(&self) -> Option<(usize, FrameMeta)>;  // consumer: newest, moves one reference
    pub fn retain(&self, slot: u64);  pub fn release(&self, slot: u64);
    pub fn misuses(&self) -> u64;                      // refused unmatched releases
    pub fn clear(&self) -> bool;      pub fn is_idle(&self) -> bool;
    pub fn handed_out(&self, slot: usize) -> bool;     // a consumer took the current value
    pub fn is_free(&self, slot: usize) -> bool;  pub fn dropped(&self) -> u64;
}
#[non_exhaustive]
pub struct FrameMeta { pub timestamp: u64, pub sequence: u64, pub source_time: u64,
                       pub sync_value: u64, pub width: u32, pub height: u32 }
impl FrameMeta {
    pub const fn new(timestamp: u64, sequence: u64, source_time: u64, width: u32, height: u32) -> FrameMeta;
    pub const fn with_sync_value(self, sync_value: u64) -> FrameMeta;
}

pub trait Grave: Send { fn is_idle(&self) -> bool; fn bytes(&self) -> u64; } // both required
pub struct Graveyard;            // usually a `static` of the adapter
impl Graveyard {
    pub const fn new(name: &'static str, grace: Duration, period: Duration) -> Graveyard;
    pub fn bury(&'static self, grave: Box<dyn Grave>); // starts the janitor thread `name` if needed
    pub fn grace(&self) -> Duration;
    pub fn bytes(&self) -> u64;  pub fn len(&self) -> usize;  pub fn is_empty(&self) -> bool;
}
```

- `Ring` only counts; it owns no surfaces and does not implement `LeaseOwner`. The adapter wraps it with the surfaces it names and makes the `LeaseOwner` promises there. Every method except `new` is lock-free and allocation-free.
- Calling protocol, which an adapter's `LeaseOwner` relies on: `set_handle`, `publish` and `abandon` only on a slot returned by `claim` and not yet published or abandoned; `release` exactly once per reference from `take` or `retain`. Breaking it is not undefined behaviour in `Ring` but voids the adapter's promises. An unmatched `release` is refused (the count never goes below zero), never panics, and is counted in `misuses`, which tests check is 0.
- `Graveyard` keeps a grave while it is referenced and for `grace` after it became idle, then drops it on its janitor thread, which exists only while the graveyard holds something (P13). `is_idle` is sampled once per period, so the grace starts at the first idle sample; it must stay true once true. `bytes` is read once at `bury`. A panic in a grave ends the janitor without losing the byte count, and the next `bury` starts a new one. Adapters bury stopped surface sets and replaced surfaces in it, which implements the native surface rules and keeps frees off the render thread.
- Benchmark: `cargo bench -p rbs-capture --bench handoff`. Release build, cloud container, 2026-11-02: about 29 ns per publish, take and release; about 8 ns for a take with no new frame (unchanged from the WGC-local ring it replaced: 29 ns and 8 ns).

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
- `options.gpu: Option<&GpuImportTarget>`: the device the test imports into, used for every GPU memory kind. For each declared GPU memory kind the suite also checks that opening it without an import target, and for a foreign device, returns `FormatUnsupported`. Without `options.gpu`, delivery of GPU memory kinds is skipped (a failure in strict mode). An adapter that declares only GPU memory kinds cannot open a session without an import target, so without `options.gpu` all session checks are skipped for it; the refusal checks still run.
- `options.process_threads`: a process-wide thread count measured by the suite itself rather than reported by the adapter (`conformance::process_thread_count` reads `/proc/self/task` on Linux). Only meaningful when the test binary runs one test at a time; the reference adapter's conformance test is alone in its binary for that reason.
- `options.strict`: every skip is a failure. Self-hosted hardware runners use it.

Checks: probe within 50 ms and never panics; `enumerate` agrees with `capture.target.enumerate` and returns only the asked kind; `pick` returns `Unsupported` when not declared (declared pickers need a user and are skipped); unknown id returns `TargetNotFound`; GPU memory kinds are refused without a matching import target; every declared pixel format and memory kind is delivered, and request order decides both; then for each enumerable target: no frames and no resources in Standby, timestamps and sequence numbers per invariants 2 and 3, latest wins after a pause, no allocation or free in `latest_frame` plus drop, frames in flight, resize, stop, restart, resources back to baseline after stop and after drop, and finally target loss.

`rbs-capture-test` runs the suite strictly against the reference adapter and against wrappers that each break one invariant (allocates per frame, frames before start, delivers after stop, hides drops, lags one frame behind the newest, keeps a three-frame queue, leaks a frame on drop, ignores target loss, stale format after resize) to prove each check fails.

## Adapters planned

| Adapter crate | Platform | Targets | Memory | Phase |
| --- | --- | --- | --- | --- |
| `rbs-capture-test` | all | synthetic display, window, camera | system | 0 (CI and conformance reference) |
| `rbs-capture-wgc` | Windows 10 1903+ | display, window | d3d11 (NT shared texture + shared fence) | 0 (implemented) |
| `rbs-capture-dxgi` | Windows | display | d3d11 | 1 (fallback) |
| `rbs-capture-mf` | Windows | camera | system, d3d11 | 2 |
| `rbs-capture-sck` | macOS 12.3+ | display, window (picker on 14+ planned) | metal (IOSurface, ready on delivery) | 0 (implemented; hardware run pending) |
| `rbs-capture-avf` | macOS | camera | metal (IOSurface) | 2 |
| `rbs-capture-pipewire` | Linux (Wayland, X11 via portal) | display, window (picker) | dmabuf, system | 0 prototype |
| `rbs-capture-v4l2` | Linux | camera | system, dmabuf | 2 |

### ScreenCaptureKit adapter notes

- Minimum deployment target macOS 12.3: the bindings link ScreenCaptureKit strongly, so an older macOS cannot load the binary; the probe's version check only catches a wrong deployment setting.
- `enumerate`, `open` and `start` wait for the replay daemon (`SCShareableContent`, `startCapture`). Warm calls take tens to a few hundred milliseconds; the first call of a process can exceed the 100 ms and 500 ms budgets above. The adapter waits at most 2 s and then returns a recoverable `Backend` error. This is an accepted exception for this adapter until measured on hardware.
- Resizes (invariant 6): the stream is configured at the target's pixel size. When the frame attachments (content rect, content scale, scale factor) show a new native size, the sample handler reconfigures the stream without waiting, and later frames carry the new size.
- Window pixel sizes use the scale of the display that shows the window's centre.
- A frame status of `Stopped` counts as target loss (invariant 7), because closing a captured window does not always reach the stream delegate.
- Soundness rests on ScreenCaptureKit not rendering into an IOSurface while its pixel buffer is retained. The hardware test `held_surface_is_not_rewritten` (`RBS_CAPTURE_HW=1`) checks it with the surface seed; it must pass on a Mac before `rbs-app` wires the adapter in.
- On macOS 15 and later, the system may show its recurring screen-recording reminder on ScreenCaptureKit calls. This is an OS alert, not a permission prompt the adapter starts, so `open` still never prompts.

## Review

Records written from the reviewer's own output (ADR-0014). The spec stays Draft.

### Round 1

- Reviewer: independent AI session (fresh context; given only the diff, principles, related ADRs, specs and code)
- Date: 2026-11-02
- Verdict: changes requested
- Concerns: (1) zero-copy cannot be negotiated without the importing device; (2) picker and permission flows do not fit `open` / `format()` / Standby; (3) clock conversion unspecified and untested, `latest_frame` takes no tick time; (4) real-time rules weaker than P6 (locks, frees and OS releases on the render thread, frees not counted); (5) the suite does not check what the spec claims (resources, in-flight, resize, target loss, latest wins, memory order, exact accounting, probe failure, leaks); (6) WGC delivers frames only on content change; (7) semver and evolution not stated; (8) camera model does not match real devices; (9) documentation and measurement gaps.

### Round 2 (re-review of the changes)

- Reviewer: the same independent AI session, given the updated diff
- Date: 2026-11-02
- Verdict: changes requested
- Resolved by the reviewer's check: round-1 concerns 2, 3, 6, 7, 8 and 9.
- Concerns: (1) no GPU adapter could pass the suite, which never set `gpu`; no device-mismatch test; (2) exact drop accounting races on real adapters; (3) latest wins missed a correctly counted queue, and a fixed 100 ms limit fails slow cameras; (4) the last lease of a frame held across `stop` freed the ring on the render thread; (5) the resource check relied only on the adapter's own report; (6) the review record in the spec was written by the author; (7) a cancelled picker was reported as a permission denial.
- Author's changes for round 2: `Options.gpu` and the `gpu import` check; `<=` accounting; interval-based age limit plus a second-call check and a lag-by-one wrapper; retired rings and a held-frame drop measurement after `stop`; `Options.process_threads` measured by the suite; this record; `CaptureError::Cancelled`.
### Round 3

- Reviewer: the same independent AI session
- Date: 2026-11-02
- Verdict: changes requested
- Resolved by the reviewer's check: round-2 concerns 1, 4, 5, 6 and 7.
- Concerns: (1) per-pair drop accounting still races (a drop counted early is charged to the previous pair); (2) a shallow queue still passed latest wins because the second-call check had one interval of slack.
- Open minor items listed by the reviewer, for the owner to accept: a retired ring keeps its buffers through Standby until `start` or drop (small exception to P13); a frame that outlives its dropped session frees the ring on the thread that drops it; the suite's own thread count works on Linux only. Fixed: `PermissionDenied` docs no longer mention a cancelled portal.
- Author's changes for round 3: accounting checked once over the run after `stop`; second call must return a frame captured after the first call, no slack; a three-frame-queue wrapper.
### Round 4

- Reviewer: the same independent AI session
- Date: 2026-11-02
- Verdict: approve (round-3 concerns 1 and 2 resolved, no regressions)
- Open minor items, for the owner to accept or schedule: the `<=` accounting does not catch an adapter that counts drops twice; a retired ring keeps its buffers through Standby until `start` or drop; a frame that outlives its dropped session frees the ring on the thread that drops it; the suite's own thread count works on Linux only; P15 asks for a human maintainer's approval of `latest_frame` (the owner's early-phase rule lets PRs merge on green CI; his later review covers it).

### WGC adapter and native surface rules (rounds 1-3)

- Reviewer: a fresh independent AI session, given the diff, specs and code only
- Date: 2026-11-02
- Round 1 verdict: request changes. Blocking: NT handle and fence values reused after stop/start could hit stale importer cache entries (stale texture, fence waits satisfied too early). Should-fix: resize retirement too short and early close, `Ring` made `LeaseOwner` promises it could not keep, last lease freed surfaces on the render thread, importer cap too small, `RoInitialize` on caller threads, unchecked agility, timestamp fallback, sticky errors, unstable window ids.
- Round 2 verdict: approve with changes (fence leak on failed start, freeze after window drags from the grave budget, uncounted ring graves, merge order with the importer, tick-based importer eviction).
- Round 3 verdict: approve with changes. The adapter is ready; merge with or after the importer's wall-time eviction (8 s), its larger surface cap and the matching gpu-compositor.md wording (rbs-gpu PR #18).
- Open for the owner: hot-path approval of the WGC hand-off (P15), approval of the native surface rules (P14), an end-to-end WGC-to-importer hardware test, hardware test hooks.

### Shared hand-off helpers

- Reviewer: a fresh independent AI session, given the diff, specs and code only
- Date: 2026-11-02
- Verdict: approve with changes. No soundness bug when the protocol is followed (ring ordering and janitor start/exit race checked).
- Resolved before merge: the module is marked adapter support outside the port contract, `FrameMeta` is `#[non_exhaustive]` with a constructor, `push` and `reap` are crate-private; the calling protocol is documented and `release` refuses underflow instead of wrapping or panicking; the janitor clears its flag and keeps the byte count on a panic; `Grave` methods are required; the janitor test waits on channels and joins the thread, covering exit and restart; janitor threads are named per adapter; spec listing matches the code; benchmark numbers recorded.
- Open: the WGC resize budget check-then-add is not atomic across sessions (overshoot by one texture each, predates this change); tokens carry no generation bits.
- Owner approval: Salih, 2026-11-02 (early-phase merge policy).

### ScreenCaptureKit adapter

- Reviewer: a fresh independent AI session, given the code and specs only
- Date: 2026-11-03
- Verdict: approve with changes. Ring protocol, latest_frame rules, retention after stop, timestamps, stop order and permission handling checked.
- Resolved before merge: start failures stop the stream; the stream follows target resizes; `Stopped` frames count as target loss; window sizes use their display's scale; the sample handler runs in an autorelease pool; the deployment target is documented; `open` checks the format before asking the OS; the attachment type and pixel format are checked; a hardware test for the surface-retention assumption was added.
- Open, needs a Mac: run `held_surface_is_not_rewritten` and the hardware conformance (blocks wiring the adapter into `rbs-app`); confirm the content-rect units used for resizes; measure `enumerate` and `start` times; measure drops at 120 Hz with five held buffers. `Sendable` is a crate-private blanket `Send` wrapper; keep its uses to the documented types.
- Owner approval: Salih, 2026-11-03 (early-phase merge policy).

## Open questions

- Per-application capture (all windows of a process, as ScreenCaptureKit offers): a new `TargetKind` in a minor version.
- How the registry's device watcher tells consumers that the target list changed (event type lives in the registry spec).
- Whether `rbs_capture::handoff` should move to `rbs-core` (and under a semver guarantee) once another port (camera-less video inputs, NDI) needs it.
