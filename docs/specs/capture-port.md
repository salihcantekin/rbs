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

- Audio input and application or system audio loopback: `rbs-audio-io` port ([audio-io-port.md](audio-io-port.md)).
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
/// Exception: ids carrying a single-use restore token change after every start, and
/// the session names the new one (`CaptureSession::target_to_store`).
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
    fn target_to_store(&self) -> Option<TargetId>;         // id to store from now on; default None
}
```

Dropping a session releases everything it holds, including what `start` acquired.

`CaptureStats` is non-exhaustive; adapters start from `CaptureStats::default()`.

`target_to_store` exists because some restore tokens are single use: the PipeWire screen-cast portal invalidates the token it restored and hands out a new one at every `start`. Contract:

- The host reads it after every successful `start` (on a control thread; not real-time). `Some(id)` is the id carrying the newest token of the same choice (for every session on that choice, also one started earlier); the host replaces the id it stores (in the scene) with it, otherwise the next run would show the picker again. `open(id)` succeeds for it; the conformance suite checks this.
- `None` means the opened id stays valid. After `start` returned `TargetNotFound`, nothing restores the choice any more and the user picks again.
- Within one process the adapter keeps accepting every id that ever named the choice, so sessions opened from an older stored id keep working until the process ends.
- Adapters whose ids never change keep the default `None`. Adding the method with a default is a minor version.
- Cost: a scene that stores such ids is rewritten after starts (a dirty project). The alternative, a stable id with the token kept by the adapter, needs adapter-owned persistence, which the port does not have. No host calls the method yet; wiring it into the scene binder is open.

`latest_frame` rules (real-time, called by the render thread on every tick):

- No allocation, no freeing, no blocking, no I/O, no logging, no locks of any kind. The hand-off from the adapter's capture thread or OS callback is lock-free, for example an atomic latest-slot swap over a pre-allocated slot ring.
- It returns the newest frame received since the previous call. Frames it replaced are counted in `dropped`; their OS objects (WGC `Direct3D11CaptureFrame`, `CVPixelBuffer`, PipeWire buffers) are released by the producer's own thread, never inside this call.
- `LeaseOwner::release` on a capture surface does not free either; it marks the slot free. When `stop` finds frames still held, the adapter keeps their slot ring alive until it is started again or dropped (the reference adapter) or until a thread of its own releases it once no frame references it (hardware adapters, see the native surface rules), so dropping the last held frame on the render thread never frees the ring there. A ring still referenced when the session is dropped must not be freed by the last lease on the render thread: hardware adapters hand it to their own thread; the reference adapter accepts that cost because it only runs in tests. The engine drops sessions on a background thread (media graph spec, live editing).
- Native frames have exactly one consumer, the `rbs-gpu` importer, which releases the lease only after its GPU work on the surface has completed.

### Invariants

Each is checked by the conformance suite, with the hook it needs in brackets. Without the hook the check is skipped and reported; with `strict` (hardware runners) a skip fails the run.

1. **Zero cost before start.** A factory and an opened but not started session own no threads, OS capture streams, device handles or GPU resources (P13). [`resources`] Exception, recorded for the owner: a PipeWire portal pick without a restore token keeps its started cast in the factory until a session starts it: at most four, each closed at the factory's first portal call more than 60 s after the pick, or when the factory is dropped (see the portal notes). Not yet accepted by the owner.
2. **Master time, at capture.** Every frame's timestamp is the capture instant in master time, converted at the edge from the OS timestamp (QPC / `SystemRelativeTime`, `mach_absolute_time` / host time, `CLOCK_MONOTONIC`, V4L2 buffer time) with a `DriftEstimator` fed by `(OS time, clock.now())` pairs taken back to back, and its `ClockMapping`. Timestamps are kept monotonic (`max(previous + 1 ns, mapped)`) and are never ahead of `clock.now()` at delivery (`min(mapped, now)`). Stamping at delivery time instead of capture time is a bug even though the suite cannot always tell; the reference adapter has its own offset and drifting clock domain and a unit test that bounds the conversion error.
3. **Monotonic and accounted.** Timestamps and sequence numbers of delivered frames strictly increase within a session, across `stop` and `start`. `sequence` counts frames received from the OS; every skipped sequence number is either counted in `dropped` or was delivered to another call. The suite checks this once over the whole run after `stop`, when the adapter is quiet (`sequence span <= delivered + dropped`), because per-frame checks would race with the OS thread counting drops between `latest_frame` and `stats`. Frames the OS drops internally (for example inside a WGC frame pool) are invisible to the adapter and not counted.
4. **Format honoured.** Every delivered frame matches `format()` (size, pixel format, colour) and its memory kind is in the request. The pixel format and memory kind are the first entries of the request lists the adapter supports for that target.
5. **Latest wins.** After a pause, the frame returned is the newest one: its age at the call is at most one frame interval (the largest gap the suite observed between consecutive frames) plus `max_latency` (default 100 ms), and a second call right away returns nothing or a frame captured after the first call (`timestamp > time of the first call`, no slack), never an older queued one.
6. **Size changes are visible in the frame.** A resized window or display produces frames with the new `format.size`; `format()` returns the new format from the first such frame on. Consumers detect the change by comparing formats. Pixel format and memory kind never change mid-session. [`resize`]
7. **Target loss is an error, not silence.** A closed window or unplugged display or camera makes `latest_frame` return `Err(TargetLost)` within `frame_timeout`. [`remove`]
8. **Stop and drop release.** After `stop` (with no frames held), and after dropping the session, resources return to the level before `open` (ADR-0013 rule 5). Exception: native surfaces kept for the handle-value grace of the native surface rules are released by the adapter's own thread within that grace, and pooled shared fences (one per session that was Active at the same time) live for the process. `start` after `stop` delivers frames again. Dropping a frame held across `stop` neither allocates nor frees. [`resources`, `process_threads`]
9. **Frame lifetime.** The consumer can hold up to `capture.frames.in_flight` frames. Holding fewer never stops delivery; holding all of them makes the adapter drop new frames (counted) and `latest_frame` still returns promptly.
10. **Declared means real.** Every declared pixel format and memory kind is delivered by at least one enumerable target. Exception, accepted by the owner on 2026-11-03: a camera adapter that delivers native formats without OS conversion (Media Foundation) cannot ask devices in its probe, so it declares every format it can deliver, and a target's `modes` say which that camera offers; the suite checks the declaration narrowed to the connected cameras. Hosts choose camera formats from `modes`, not from `capture.pixel_formats`. The rejected alternative was letting the OS convert missing formats (Media Foundation's video processor, one more CPU pass per frame).

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
- The conformance suite (`rbs_capture::conformance`) is a test helper used by adapter crates in this workspace, not a stable API yet: it may change in a minor version while the port is `0.x`. 2026-11-03: `Options.process_threads` changed from a thread count to a list of thread ids (`process_thread_count` became `process_thread_ids`).

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

## Camera helpers

`rbs_capture::camera` is an adapter-support module like `handoff` (versioned with the port crate, outside the port contract). It turns the modes a device reports into the port's `CameraMode` list and picks the mode for a request, so every camera adapter applies the same rules.

```rust
#[non_exhaustive]
pub struct NativeMode { pub width, pub height, pub pixel_format, pub min_rate, pub max_rate,
                        pub index: usize }   // + new; index = the adapter's own format handle
#[non_exhaustive]
pub struct ModeChoice { pub index, pub size: Size, pub pixel_format, pub rate: FrameRate }
pub fn camera_modes(modes: &[NativeMode]) -> Vec<CameraMode>;   // distinct, in listed order
pub fn choose_mode(modes: &[NativeMode], pixel_formats: &[PixelFormat],
                   size: Option<Size>, rate: Option<FrameRate>) -> Result<ModeChoice, CaptureError>;
pub fn rate_cmp(a: FrameRate, b: FrameRate) -> Ordering;        // exact
```

- `pixel_format` of a native mode is what the adapter delivers for it, after any conversion the OS does on the way; a device format deliverable in several pixel formats is listed once per format.
- Rules of `choose_mode`: the pixel format is the first requested one that some mode delivers (invariant 4), else `FormatUnsupported`; the size is the nearest to `request.size` (sum of width and height differences, ties to the larger picture), or the largest without one; among modes of that size, one whose rate range contains the requested rate, else the closest range, or the highest maximum rate without one; ties go to the mode listed first, so adapters list preferred device formats first. The rate is the request clamped into the range (exactly one of its bounds when clamped, so an adapter can hand the OS its own value back), or the range's maximum.
- Unit tests in the module cover each rule; it is platform independent and never runs on the render thread.

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
- `options.process_threads`: the process's threads, listed by the suite itself rather than reported by the adapter. Only meaningful when the test binary runs one test at a time; the reference adapter's conformance test is alone in its binary for that reason. `process_threads` returns thread ids (`conformance::process_thread_ids` reads `/proc/self/task`). The suite compares ids, not counts: threads that existed before `open` may end meanwhile, and a thread that did not exist before fails the check unless the kernel marks it exiting (`PF_EXITING` in `/proc/self/task/<id>/stat`). A joined thread can still be listed for a moment, because the kernel wakes the joiner before it removes the exiting task; counting it made the PipeWire conformance test fail on CI now and then, in both directions (an earlier session's exiting thread in the baseline, or the session's own joined thread after `drop`). No waiting is involved; checked over 20,000 spawn-and-join rounds on Linux 6.18, every joined thread still listed was marked exiting.
- `options.strict`: every skip is a failure. Self-hosted hardware runners use it.

Checks: probe returns within 1 s (the fastest of up to three calls, so one-time process costs do not count; a hang guard; the 50 ms probe budget is a performance budget, not a pass/fail check, and is an open measurement item in [performance budgets](performance-budgets.md) until a probe benchmark exists, ADR-0032) and never panics; `enumerate` agrees with `capture.target.enumerate` and returns only the asked kind; `pick` returns `Unsupported` when not declared (declared pickers need a user and are skipped); unknown id returns `TargetNotFound`; GPU memory kinds are refused without a matching import target; every declared pixel format and memory kind is delivered, and request order decides both; then for each enumerable target: no frames and no resources in Standby, an id from `target_to_store` opens, timestamps and sequence numbers per invariants 2 and 3, latest wins after a pause, no allocation or free in `latest_frame` plus drop, frames in flight, resize, stop, restart, resources back to baseline after stop and after drop, and finally target loss.

`rbs-capture-test` runs the suite strictly against the reference adapter and against wrappers that each break one invariant (allocates per frame, frames before start, delivers after stop, hides drops, lags one frame behind the newest, keeps a three-frame queue, leaks a frame on drop, ignores target loss, stale format after resize) to prove each check fails.

## Adapters planned

| Adapter crate | Platform | Targets | Memory | Phase |
| --- | --- | --- | --- | --- |
| `rbs-capture-test` | all | synthetic display, window, camera | system | 0 (CI and conformance reference) |
| `rbs-capture-wgc` | Windows 10 1903+ | display, window | d3d11 (NT shared texture + shared fence) | 0 (implemented) |
| `rbs-capture-dxgi` | Windows | display | d3d11 | 1 (fallback) |
| `rbs-capture-mf` | Windows | camera | system (native NV12, YUY2, I420, BGRA); d3d11 later | 2 (implemented, system memory; hardware run pending) |
| `rbs-capture-sck` | macOS 12.3+ | display, window (picker on 14+ planned) | metal (IOSurface, ready on delivery) | 0 (implemented; hardware run passed 2026-11-03) |
| `rbs-capture-avf` | macOS | camera | metal (IOSurface: NV12 or BGRA, ready on delivery) | 2 (implemented; hardware run pending) |
| `rbs-capture-pipewire` | Linux (Wayland, X11 via portal) | camera (PipeWire `Video/Source` nodes); display, window (screen-cast portal picker) | dmabuf (ready), system | 0 (implemented; portal tested against a test portal, desktop run pending) |
| `rbs-capture-v4l2` | Linux | camera | system, dmabuf | 2 |

### ScreenCaptureKit adapter notes

- Minimum deployment target macOS 12.3: the bindings link ScreenCaptureKit strongly, so an older macOS cannot load the binary; the probe's version check only catches a wrong deployment setting.
- `enumerate`, `open` and `start` wait for the replay daemon (`SCShareableContent`, `startCapture`). Warm calls take tens to a few hundred milliseconds; the first call of a process can exceed the 100 ms and 500 ms budgets above. The adapter waits at most 2 s and then returns a recoverable `Backend` error. This is an accepted exception for this adapter until measured on hardware.
- Resizes (invariant 6): the stream is configured at the target's pixel size. When the frame attachments (content rect, content scale, scale factor) show a new native size, the sample handler reconfigures the stream without waiting, and later frames carry the new size.
- Window pixel sizes use the scale of the display that shows the window's centre.
- A frame status of `Stopped` counts as target loss (invariant 7), because closing a captured window does not always reach the stream delegate.
- Cursor of window captures: ScreenCaptureKit draws the cursor into a window capture whenever the pointer is inside the window's frame, even where another window covers it, so the picture showed a second cursor over hidden content (seen on a Mac with RBS above a captured Terminal window, 2026-11-06). With `cursor` set, a window capture therefore draws the cursor only while no on-screen window stacked above the target (`CGWindowListCopyWindowInfo` above the window, desktop elements excluded, alpha above 0, window level below the Dock's) is under the pointer. System windows from the Dock level up are left out because their bounds do not say what they draw: on macOS 26 the Dock's window spans the whole display with alpha 1 and the window server's cursor window sits under the pointer. Over the menu bar, the visible Dock or an open popup menu the captured cursor can therefore still show. While such a session is Active, a watcher thread (`rbs-sck-cursor`) reads the pointer every 50 ms, reads the window list when the pointer moved and at least every 300 ms while it stands still, and on a change reconfigures the stream's `showsCursor` without waiting. A covered cursor can still show for one poll after the pointer moves (50 ms), or up to 300 ms when a window is raised over a pointer that stands still, plus the reconfiguration time. The thread exists only while a window capture with the cursor is Active (P13) and is joined by `stop`; display captures and window captures without the cursor have none. If the thread cannot start, the cursor is drawn as ScreenCaptureKit does by default.
- Soundness rests on ScreenCaptureKit not rendering into an IOSurface while its pixel buffer is retained. The hardware test `held_surface_is_not_rewritten` (`RBS_CAPTURE_HW=1`) checks it with the surface seed; it must pass on a Mac before `rbs-app` wires the adapter in.
- On macOS 15 and later, the system may show its recurring screen-recording reminder on ScreenCaptureKit calls. This is an OS alert, not a permission prompt the adapter starts, so `open` still never prompts.

### AVFoundation camera adapter notes

- Targets: every video capture device (`devicesWithMediaType:`, which also lists external and Continuity cameras; discovery sessions renamed the external device type in macOS 14, and a constant missing on older systems would stop the binary from loading). Ids are `camera:<uniqueID>`, which Apple keeps stable across reconnections and reboots. Listing needs no permission and opens nothing. Each target's size is its largest mode.
- Modes: every device format and frame-rate range, once as `nv12` and once as `bgra8`. AVFoundation's video data output delivers either from any device format (it converts on the way, in the capture pipeline), so the request order alone decides the pixel format. NV12 is `'420v'` (limited range), tagged `BT709_LIMITED`; reading the buffers' YCbCr matrix attachment instead is a follow-up. BGRA is tagged sRGB. Compressed formats stay out of scope (see Scope); AVFoundation decodes them before delivery, so a camera whose larger sizes are only MJPEG still offers them, decoded by the OS.
- Memory: Metal only (IOSurface, `SurfaceSync::Ready`), like ScreenCaptureKit. The Metal importer aliases BGRA and decodes NV12 on the GPU. System memory is not offered.
- Permission: `permission` maps `AVAuthorizationStatus` (restricted counts as denied). `open` and `start` refuse unless authorized, so creating the device input never shows the prompt; only `request_permission` does. Its process (or the app responsible for it, such as the terminal) needs `NSCameraUsageDescription` in its Info.plist, otherwise macOS ends the process at the request.
- `open` picks the device format and rate with `rbs_capture::camera::choose_mode` and creates nothing. `start` finds the same device format again (size, media subtype and a rate range containing the chosen rate), builds an `AVCaptureSession` with the device input and a video data output (late frames discarded), sets the active format and a fixed frame duration while the device is locked for configuration, and runs the session. `startRunning` blocks until the camera streams; cameras can take longer than the 500 ms `start` budget to wake up. This is an accepted exception, like the ScreenCaptureKit one, until measured on hardware.
- Errors: a device in use by another application or session is `DeviceBusy`, an unauthorized application `PermissionDenied`, a device that is gone at `start` `TargetNotFound`; a session that did not run is a recoverable `Backend` error.
- Target loss (invariant 7): `AVCaptureDeviceWasDisconnectedNotification` for the device and `AVCaptureSessionRuntimeErrorNotification` for the session set an atomic flag that `latest_frame` reads. Both observers are removed at `stop`.
- Hand-off: the same ring, graveyard and grace as the ScreenCaptureKit adapter. Each slot retains its `CVPixelBuffer`, which keeps AVFoundation from rendering into its IOSurface. Because the output's pool is small and stops delivering when the client holds too many buffers, free slots give their buffers back on the next sample (on the delegate's queue, never on the render thread); surfaces a frame was handed out with stay retained as `IOSurface`s for the grace. A plain retain does not keep the pool from reusing a surface for a new frame, but the pool skips surfaces that are in use (`IOSurfaceIsInUse`), and the Metal importer's cached textures may count as use: if they do, the pool grows or starves while the importer caches. Unverified; see the open items. Samples that cannot be delivered (no IOSurface, unexpected pixel format) and frames AVFoundation discards before the delegate sees them (`didDropSampleBuffer`) count as received and dropped.
- Timestamps: sample presentation times are on the session's `synchronizationClock` (usually the host clock, not always); they are converted to host time (`CMSyncConvertTime`), then mapped to the master clock with the drift estimator like ScreenCaptureKit's. A mapped time more than 1 s from the arrival time is taken to be on an unexpected clock and replaced by the arrival time.
- `reconfigure` is unsupported (cameras have no cursor; a rate change needs a new session).
- `stop` stops the session, removes the observers, detaches the delegate and waits for its serial queue to drain, so no delegate call runs after `stop` returns. A rate AVFoundation cannot be given as an exact `CMTime` is refused at `open` (`FormatUnsupported`); a device format that is gone at `start` (another device behind the id) is `FormatUnsupported` too.
- Soundness assumption: AVFoundation does not write into an IOSurface while its pixel buffer is retained, and its pool keeps delivering while three frames are held. The hardware test `held_surface_is_not_rewritten` (`RBS_CAPTURE_HW=1`, NV12 and BGRA) checks both; it must pass on a Mac before `rbs-app` wires the adapter in. CI runners have no camera, so CI covers probing, empty enumeration and the refusal checks only.

### Media Foundation camera adapter notes

- Targets: every video capture device from `MFEnumDeviceSources` (`MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID`). Ids are `camera:<symbolic link>`, which Windows keeps stable for a device on the same port across restarts. `enumerate` opens a media source per device to read its native media types through a short-lived synchronous source reader, then shuts it down (readers are created with `MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN`, so only the adapter shuts its sources down); this does not stream (the camera light stays off) but can exceed the 100 ms budget with several cameras, an accepted exception until measured. A camera whose source cannot be created (blocked by the privacy settings, or busy) is listed without modes.
- Modes: the native uncompressed types: NV12, YUY2, I420 (also IYUV) and RGB32 (delivered as `bgra8` with alpha set opaque during the copy). Compressed types (MJPEG, H.264) are out of scope (see Scope), so many USB cameras offer only their smaller sizes. The probe cannot ask devices, so it declares all four formats; which a camera offers is in its modes. The hardware conformance test narrows the declaration to the connected cameras (the invariant 10 exception, accepted by the owner on 2026-11-03).
- Colour: from `MF_MT_YUV_MATRIX` and `MF_MT_VIDEO_NOMINAL_RANGE`; without them Media Foundation's defaults (BT.601 below 720 lines, BT.709 from 720 lines, limited range). RGB is sRGB.
- Memory: system only. USB cameras deliver into system memory, so the one copy into the adapter's ring (counted in `cpu_copied`) stands in for the upload Media Foundation's Direct3D path would do; no GPU readback happens. A d3d11 path (frame-server textures, or the adapter uploading into NT shared textures like WGC's) is a follow-up once the importer decodes D3D YUV textures.
- Permission: Windows has no fast, prompt-free query for desktop apps outside WinRT, so `permission` is `Granted` and a camera blocked by the privacy settings shows up as `PermissionDenied` from `open` or `start` (`E_ACCESSDENIED`). `request_permission` keeps the default.
- COM and runtime: the adapter keeps the process's implicit multithreaded apartment alive (`CoIncrementMTAUsage`, like WGC), so it never changes the apartment of the caller's thread. `MFStartup` is held only around `enumerate` and `open` and while a session is Active, balanced by `MFShutdown`.
- `open` checks the device is listed, reads its types, picks one with `rbs_capture::camera::choose_mode` and shuts the source down (Standby holds strings only). `start` creates the media source and an asynchronous source reader with converters disabled (`MF_READWRITE_DISABLE_CONVERTERS`), sets the chosen native type with the chosen rate, and requests the first sample. It waits up to 2 s for the first sample or error, so a privacy block, a camera busy elsewhere or one already gone fails `start` itself (`PermissionDenied`, `DeviceBusy`, `TargetNotFound`); a camera slower than that starts anyway and later errors arrive as target loss. This exceeds the 500 ms `start` budget for cameras that wake slowly, an accepted exception like the AVFoundation one.
- Hand-off: the reader callback runs on Media Foundation's work queue threads. It copies the picture (from `IMF2DBuffer2::Lock2DSize`, or a contiguous lock with the type's default stride, flipping bottom-up RGB) into the buffer of a claimed ring slot, publishes it and requests the next sample outside the producer lock. Slot buffers are reused; the producer allocates only the small shared header of each frozen buffer, never pixel memory. Planes after the first are taken to follow the first after the picture's height in rows (no padded heights); a sample whose layout does not fit its buffer is dropped and counted.
- Timestamps: when the sample carries `MFSampleExtension_DeviceReferenceSystemTime` (performance-counter time of capture, 100 ns units), it is mapped to the master clock with the drift estimator, paired with `QueryPerformanceCounter`; otherwise, or when the mapping is more than 1 s from arrival, the arrival time is used.
- Target loss (invariant 7): a failed read status (for example `MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED` on unplug) or the reader's error or end-of-stream flag.
- `stop` marks the producer stopped, waits until no reader callback runs (a callback may have taken the reader just before and still request a sample), flushes the reader (waiting at most 500 ms for each, a hang guard), releases the reader and shuts the source down, all before `MFShutdown`. A ring still referenced by frames, or whose `Arc` a lease still holds for the moment between releasing its slot and dropping it, goes to the adapter's graveyard (`rbs-mf-janitor`, 100 ms grace for that moment; system buffers have no handle values to protect), so the last lease never frees on the render thread. I420 pictures whose chroma pitch is shorter than a chroma row are refused (dropped).
- `reconfigure` is unsupported.
- CI runners have no camera; CI covers probing, empty enumeration and the refusal checks, and the platform-neutral copy, colour and ring code in unit tests on every OS. `RBS_CAPTURE_HW=1 cargo test -p rbs-capture-mf` on Windows with a camera runs the full suite.

### PipeWire adapter notes

- Targets: every `Video/Source` node (cameras through the session manager's V4L2 or libcamera monitor, and other applications' video sources) is a `Camera` target with id `node:<node.name>`. Screens and windows are chosen with `pick` through the desktop portal (next section); `enumerate(Display | Window)` returns `Unsupported`. `permission` is `Granted`: node access is governed by the daemon (the camera portal only matters inside Flatpak), and screen casts are granted per choice in the portal's dialog.
- `open` reads the node's `EnumFormat` params and narrows the request to the first requested pixel format the node offers in a requested memory kind (BGRA or BGRx for `bgra8`, RGBA or RGBx for `rgba8`), so `format()` is exact before `start` and the negotiation cannot land elsewhere. Nodes that offer only YUV formats return `FormatUnsupported` until a conversion path exists.
- DMA-BUF: offered only for a `DrmRenderNode` import target that accepts `ready` sync, with exactly the importer's modifiers (linear when it names none), following PipeWire's DMA-BUF negotiation: with several modifiers the offer is `DONT_FIXATE` and the source, which allocates, fixes the modifier; the adapter waits for the fixed format. Single-plane buffers only. Each slot owns a duplicate of the buffer's fd and keeps the PipeWire buffer dequeued while any frame refers to it, so the producer never writes into a held buffer; the importer caches by inode, so a new fd number for the same buffer does not import twice. Up to five buffers (the ring slots: three frames in flight, the newest, and the one being published) stay dequeued, so the adapter asks for 8 buffers and requires at least 6; freed buffers go back when the next buffer arrives, or when their slot is claimed again. Buffers PipeWire removes (renegotiation, disconnect) are forgotten, never queued again.
- Soundness assumption (DMA-BUF): a source does not write into a buffer while the consumer keeps it dequeued, and after a renegotiation or `stop` does not reuse the memory of a buffer whose fd the adapter still holds. Sources that share one pool between several consumers or keep their pool across renegotiation could break this. It needs a hardware test on a Linux GPU (a held DMA-BUF is not rewritten, as `held_surface_is_not_rewritten` checks for ScreenCaptureKit) before `rbs-app` offers DMA-BUF from this adapter; system memory is unaffected.
- System memory: the adapter copies the mapped buffer into a reused buffer of its own and returns the PipeWire buffer at once; every such frame counts in `cpu_copied`. The fourth byte of BGRx and RGBx is set to opaque during the copy.
- Timestamps: the buffer header's `pts` (nanoseconds, `CLOCK_MONOTONIC`) is mapped to the master clock with the drift estimator; a missing header, or one more than 1 s older than arrival, falls back to the arrival time.
- Resizes: a source may renegotiate its size at any time; later frames and `format()` follow. Target loss: the node missing from the first registry round trip after `start` or leaving the registry later, or the stream going to `Error` or `Unconnected`. The stream sets `node.dont-fallback`, so the session manager never links another source (a different camera) in place of a missing target.
- One stream thread (`rbs-pipewire`) per Active session owns every PipeWire object; `enumerate` (at most 100 ms) and `open` (at most 500 ms) use a short-lived connection on the calling thread. A stopped session keeps a ring that frames still refer to; at the next `start` or the drop it is dropped there if idle, or else buried in the adapter's graveyard (`rbs-pipewire-janitor`), so the last lease, dropped on the render thread, never closes fds or frees there.
- Building needs the `libpipewire-0.3` and `libdbus-1` development headers and libclang (bindgen). The conformance test publishes its own test source node from a child process (so the adapter's threads and fds are counted alone) and needs a running daemon with WirePlumber; CI starts both and sets `RBS_REQUIRE_PIPEWIRE`. It covers shared memory only; the DMA-BUF path is untested until a Linux GPU runner exists.

### PipeWire screen-cast portal notes

Screens and windows on Linux (Wayland, and X11 desktops that run the portal) come from the XDG desktop portal's ScreenCast interface (`org.freedesktop.portal.ScreenCast`, version 4 or later for restore tokens), which hands out a PipeWire node and a daemon connection that can see it. The adapter reuses the node stream above for them.

- Capabilities: when the process has a session bus address (`DBUS_SESSION_BUS_ADDRESS` or `$XDG_RUNTIME_DIR/bus`; checked without connecting), the probe adds `display` and `window` to `capture.target.kinds` and `capture.target.picker`, and sets `capture.cursor` (cursors apply to portal targets; `reconfigure(Cursor)` on a camera node is `Unsupported`). Whether a portal answers is only known at `pick`: it returns `Unsupported` when the portal reports it cannot cast that kind (`AvailableSourceTypes`), and a recoverable `Backend` error when the portal does not answer or fails.
- Calls: each `pick` and `start` opens a private session bus connection (libdbus, no threads). All calls that show no dialog (property reads, `CreateSession`, `SelectSources`, `OpenPipeWireRemote`) share one 2 s budget; closing a request or session waits at most 500 ms. `Response` signals are only accepted from the portal's unique name, and a portal that leaves the bus while a dialog is up ends the wait with a recoverable `Backend` error, so `pick` cannot wait forever on a crashed portal.
- `pick(Display | Window)`: one portal session (`CreateSession`, `SelectSources` with one source of that type, `persist_mode` 2 so the choice survives restarts until the user revokes it, cursor embedded; then `Start`, which shows the dialog and blocks until the user answers). Response 1 is `Cancelled`, any other refusal `PermissionDenied`. The target's `size` is the portal's logical size of the stream and its name is "Picked display" or "Picked window" (the portal reports no names).
  - With a restore token, the pick's session is closed at once and the id is `portal:display:<token>` or `portal:window:<token>`; nothing is kept until `start` (P13).
  - Without one (a portal older than version 4, or the user declined persistence), the started cast is kept by the factory under `portal:<kind>:live-<n>`, so the session that opens it starts without a second dialog. This is an exception to invariant 1: an OS capture stream (and the desktop's sharing indicator) lives in the factory between the user's pick and the start. It is bounded: at most four such casts (the oldest is closed first), each closed after 60 s if no session started it (checked at the factory's next portal call, so no timer thread exists), and all closed when the factory is dropped. A live target can be started once: after that session stops, `open` and `start` return `TargetNotFound` and the UI picks again. Refusing such picks instead would make the portal unusable for users who decline persistence.
- `open` never talks to the portal: it checks the id and the request. Portal casts are delivered as BGRA or BGRx (every compositor offers them), so the request's first supported pixel format must be `bgra8`, otherwise `FormatUnsupported`. Only the request's first supported memory kind is offered (invariant 4): whether a compositor can share DMA-BUFs with the importer's modifiers is decided only when the stream links, and a link that cannot deliver it fails `start` with `FormatUnsupported` instead of falling back to system memory. For that, a portal `start` reports success only once the stream accepted a format, not when it reaches `Paused`: an `Error` state before that, or no format within 2 s, is `FormatUnsupported`. The DMA-BUF soundness assumption of the node notes applies to compositor casts too and is unverified; hosts request system memory for portal targets until a hardware check passes. The size is known from the first frame.
- `start` restores the choice with its newest token (a new portal session; `cursor_mode` embedded when the request asks for the cursor and the portal offers it, hidden otherwise), asks `OpenPipeWireRemote` for a daemon connection and links the stream to the cast's node by id on that connection. The portal answers a valid token without a dialog. A token that no longer restores (revoked, used elsewhere, or a monitor or window that is gone) makes the portal show its dialog instead: `start` waits at most 2 s for the `Start` answer (the factory's `with_restore_timeout` changes it; tests use 100 ms), then closes the request (which dismisses the dialog) and returns `TargetNotFound`; the UI asks the user to pick again. Worst case, `start` blocks about 2 s (setup budget) + 2 s (`Start`) + 0.5 s (closing) + 2 s (format negotiation). This is an accepted exception to the 500 ms `start` budget, like the ScreenCaptureKit one.
- Restore tokens are single use. A token counts as used once the portal answered `Start` or its dialog came up instead (also on cancel or denial); a failure before that (no bus, a slow or failed setup call, a portal still starting) keeps it, and the error is a recoverable `Backend` error. Restores of one choice run one after the other (a second `start` on the same choice waits for the first one's bounded restore), and the factory records each new token only if the one used is still the newest, so a restore that lost a race never overwrites a newer token. Every id that ever named the choice maps to it, and `target_to_store` returns the choice's newest id. When the portal used the last token without handing out a new one, the choice cannot be restored after the current casts stop.
- Two sessions that are Active at the same time on one choice each hold their own portal session and cast.
- An Active portal session owns, besides the stream thread, the session bus connection holding the portal session and the PipeWire connection the portal opened. `stop` ends the stream thread, then closes the portal session (`Session.Close`) and the connection, so the compositor stops casting; nothing is left in Standby. Target loss: the compositor removes the cast's node when the window or monitor goes away or the user stops sharing, which the stream sees as the node leaving the registry or the stream disconnecting (invariant 7). The bus connection is not read while Active.
- `reconfigure(Cursor)` and `reconfigure(FrameRate)` apply from the next `start`. A live target keeps the cursor mode of its pick (embedded).
- Locks: the factory's table of choices is a mutex used on control threads only, never across D-Bus calls; `latest_frame` is unchanged.
- D-Bus is accessed through libdbus (`dbus` crate) rather than zbus: zbus's blocking API leaves an `async-io` reactor thread for the rest of the process after its first connection (measured), which an adapter may not do (P13, invariant 8).
- Test: `tests/portal.rs` starts a private session bus and a test portal in a child process that implements the ScreenCast calls (request and response signals, single-use tokens, a dialog that never answers for used tokens, `OpenPipeWireRemote` returning a daemon connection) and publishes the node its casts point at. It checks pick for both kinds with the options sent (`types`, `multiple`, `persist_mode`, `cursor_mode`, restore token), token rotation and `target_to_store`, two sessions on one choice, cancel, deny, `Unsupported` versus an unanswered portal, live picks and their limit of four, closing on factory drop, the refusal of a used token without asking the portal again, the BGRA rule, and runs the conformance suite against picked displays (a wrapper lists one picked display as enumerable) with resize, target loss, `target_to_store` and process threads. The whole test takes under 2 s; a 30 s watchdog turns an unforeseen hang into a failure. Not tested: concurrent restores from two threads (serialised by design), the 60 s expiry of live picks, real portals (GNOME, KDE, wlroots) and DMA-BUF casts.

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
- Hardware run, 2026-11-03: `held_surface_is_not_rewritten` and the hardware conformance passed on an M4 Pro Mac (all six tests with `RBS_CAPTURE_HW=1`), so the adapter may be wired into `rbs-app`. The tests never request permission, so grant screen recording to the terminal (under Claude Code, its helper app) first.
- Open, needs a Mac: confirm the content-rect units used for resizes; measure `enumerate` and `start` times; measure drops at 120 Hz with five held buffers. `Sendable` is a crate-private blanket `Send` wrapper; keep its uses to the documented types.
- Owner approval: Salih, 2026-11-03 (early-phase merge policy).

### PipeWire adapter (video source nodes)

- Reviewer: a fresh independent AI session, given the code and specs only
- Date: 2026-11-03
- Verdict: request changes, all addressed before merge.
- Resolved before merge: a held buffer leaked when its slot came free between returning buffers and claiming one (now returned on claim); held pointers went stale when PipeWire removed buffers (now forgotten in `remove_buffer`); retired rings could be freed on the render thread (now dropped on the control thread when idle, else buried); the consumer fixed the DMA-BUF modifier itself (now the source does); a missing target could fall back to another camera or stay silent (`node.dont-fallback`, loss after the first round trip); `enumerate` waited 500 ms (now 100 ms); the conformance test now counts the adapter's fds and threads with the source in a child process; CI waits for the daemon and WirePlumber.
- Open: the DMA-BUF path has no test until a Linux GPU runner exists, and its no-rewrite assumption (see the PipeWire notes) needs a hardware check before `rbs-app` offers DMA-BUF from this adapter; no unit test isolates the held-buffer bookkeeping.
- Owner approval: Salih, 2026-11-03 (early-phase merge policy).

### PipeWire screen-cast portal

- Reviewer: a fresh independent AI session, given the code and specs only, two rounds
- Date: 2026-11-03
- Verdict: round 1 request changes; round 2 request changes (one should-fix, four minor); all addressed before merge except the optional timeout test.
- Resolved in round 1: single-use restore tokens broke a second `start` and stored scenes (now a choice table with the newest token per choice, compare-and-set updates, serialised restores, and `CaptureSession::target_to_store` for the host); a slow setup call or a failed property read used up the token (only a `Start` answer or its dialog does now); unbounded waits on the portal (one 2 s setup budget, 500 ms closes, responses accepted from the portal's owner only, a portal leaving the bus ends a pick); D-Bus calls under the factory's lock; unbounded live picks (at most four, 60 s); several memory kinds offered at once (first requested only, invariant 4); the thread-count check of the conformance suite failed now and then on CI (thread ids and `PF_EXITING`, no waiting).
- Resolved in round 2: a portal `start` reported success at `Paused`, before a format was agreed, so a link that cannot deliver the requested memory kind never failed `start` (now success only after a format is accepted, else `FormatUnsupported`); the `restoring` flag stayed set if a restore panicked, blocking later starts on that choice forever (now cleared by a drop guard that wakes the waiters); invariant 1 overstated the 60 s bound (reworded); the breaking change of the conformance helpers is recorded under semver.
- Open: the owner's acceptance of the invariant 1 exception for live picks; a manual run against real portals (GNOME, KDE, wlroots); DMA-BUF casts are unverified (hosts request system memory for portal targets); no host calls `target_to_store` yet (`rbs-app` must store the new id after each `start`); the setup-budget timeout path is untested (the mock answers property reads with an error, not silence); a detached thread caught in `do_exit` passes the thread check.
- Owner approval: early-phase merge policy (Salih, 2026-11-02); the invariant 1 exception awaits the owner's big review.

### AVFoundation camera adapter and camera helpers

- Reviewer: a fresh independent AI session, given the diff, specs and code only
- Date: 2026-11-03
- Verdict: approve with changes. No soundness bug and no Objective-C exception path found (`return_free` cannot race the render thread; formats and durations are set under the configuration lock and checked against the device's ranges; inputs and outputs are added only after `canAdd*`).
- Resolved before merge: sample times are converted from the session's synchronization clock, with an arrival-time fallback for times more than 1 s off; `stop` drains the delegate queue; rates are reduced before conversion and refused at `open` when they do not fit a `CMTime`, and a vanished device format is `FormatUnsupported` instead of a retried `TargetNotFound`; undeliverable samples are counted as drops; the pool-reuse claim is marked unverified.
- Open, needs a Mac with a camera: the hardware tests; a run through the `rbs-gpu` Metal importer (several seconds with texture caching) to check that cached textures do not starve AVFoundation's pool; measure `start` latency.

### Media Foundation camera adapter

- Reviewer: a fresh independent AI session, given the code and specs only
- Date: 2026-11-03
- Verdict: request changes; all addressed before merge.
- Resolved before merge: the probe reader shut the media source down on release, so `start` would have failed on every camera (readers now disconnect instead); a callback could request a sample after `stop`'s flush and outlive `MFShutdown` (`stop` waits for running callbacks); the last free of a ring could still happen on the render thread in the gap inside `Lease::drop` (100 ms grace, and the `Arc` count decides at `stop`); a camera gone during `start` is `TargetNotFound`; overlapping I420 chroma rows are refused; the declaration of native formats is recorded as an invariant 10 exception for the owner instead of only being narrowed in the test.
- Open: the owner's decision on the invariant 10 exception (or converting missing formats); a hardware run on Windows with a camera; `latest_frame` uses the same system-memory consumer as the PipeWire adapter (allocation-free, unit-tested), and its human hot-path approval (P15) is part of the owner's later review.

## Open questions

- Per-application capture (all windows of a process, as ScreenCaptureKit offers): a new `TargetKind` in a minor version.
- How the registry's device watcher tells consumers that the target list changed (event type lives in the registry spec).
- Whether `rbs_capture::handoff` should move to `rbs-core` (and under a semver guarantee) once another port (camera-less video inputs, NDI) needs it.
