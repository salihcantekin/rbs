# Spec: Virtual camera port

- Status: Draft
- Owner crate(s): `rbs-vcam` (port, conformance), `rbs-vcam-ring` (adapter support: shared frame ring), `rbs-vcam-test` (test adapter), adapter `rbs-vcam-mf` (Windows) and its companion DLL `rbs-vcam-mf-source`; macOS and Linux adapters later (ADR-0066 decision 8); the node in `rbs-nodes`
- Related ADRs: [0066](../adr/0066-virtual-camera.md), [0003](../adr/0003-ports-and-adapters.md), [0013](../adr/0013-zero-cost-when-unused.md), [0025](../adr/0025-encoder-surface-export.md), [0028](../adr/0028-cross-process-preview-surfaces.md), [0031](../adr/0031-adapter-support-crates.md), [0038](../adr/0038-io-graph-nodes.md), [0047](../adr/0047-several-outputs-per-canvas.md), [0050](../adr/0050-command-registry-menus-and-toolbar.md), [0061](../adr/0061-studio-mode-in-the-desktop-app.md)
- Related specs: [backend-ports.md](backend-ports.md), [media-types.md](media-types.md), [io-nodes.md](io-nodes.md), [recording.md](recording.md)

## Purpose

The port through which RBS offers its picture to other apps as a camera. Every OS mechanism (`MFCreateVirtualCamera` on Windows now; a CoreMediaIO Camera Extension on macOS and `v4l2loopback` on Linux later) is an adapter behind this one contract; the engine sends frames and never sees the OS API.

## Scope

In scope: installation state, adding and removing the OS camera, opening a sending session, the non-blocking frame hand-off, the shared-memory frame ring for consumers in another process, capability keys, errors and the conformance suite.

Out of scope: the studio-mode preview scene as the camera's picture (needs a decision amending ADR-0061), audio (a virtual microphone is a separate decision), scaling and colour conversion (the node does them on the GPU), the OS consumers' behaviour (mirroring in self-view is the meeting app's choice), several virtual cameras at once (one camera per computer for now).

## Contract

Media types come from `rbs_core::media` ([media-types.md](media-types.md)). The Rust types in `rbs_vcam` are the source of truth once they exist; the signatures below are kept in sync with them.

### Installation

```rust
#[non_exhaustive]
pub enum InstallState {
    /// This computer cannot have the camera: `reason` is shown to the user as is
    /// ("Needs Windows 11", "This build of RBS is not signed for camera extensions").
    Unsupported { reason: String },
    NotInstalled,
    /// Asked for; the OS waits for the user (macOS: System Settings).
    /// The installed extension or source has another layout version than this app.
    NeedsUpdate { reason: String },
    Installed,
}
```

### Factory

```rust
pub trait VirtualCameraFactory: Send + Sync {
    fn id(&self) -> &'static str;                   // "org.rbs.vcam.cmio", ...
    fn capabilities(&self) -> &CapabilitySet;       // static, no OS call
    /// Cheap probe (no thread, no prompt). May block briefly; control thread only.
    fn status(&self) -> Result<InstallState, VcamError>;
    /// Adds the OS camera. May show OS prompts and block until they are answered;
    /// control thread only, never the UI thread. Idempotent.
    fn install(&self) -> Result<InstallState, VcamError>;
    /// Removes it. Idempotent; `NotInstalled` afterwards, or `NeedsRestart`.
    fn uninstall(&self) -> Result<InstallState, VcamError>;
    /// The one format the camera announces to apps (ADR-0066 decision 7). Control
    /// thread. Applies when no app reads the camera; `Ok(false)` means it is pending.
    fn set_format(&self, format: &CameraFormat) -> Result<bool, VcamError>;
    /// Opens a sending session. Fails with `NotInstalled` unless `status` is `Installed`.
    fn open(&self, config: &VirtualCameraConfig) -> Result<Box<dyn VirtualCameraSession>, VcamError>;
}

/// What the camera announces: size (even, within vcam.max_size), rate (one of
/// vcam.rates), pixel format (Nv12).
#[non_exhaustive]
pub struct CameraFormat { pub width: u32, pub height: u32, pub rate: FrameRate, pub pixel: PixelFormat }
impl CameraFormat { pub fn new(width: u32, height: u32, rate: FrameRate, pixel: PixelFormat) -> Self; }

/// A session: the announced format and the memory kind frames come in
/// (one of vcam.input.memory).
#[non_exhaustive]
pub struct VirtualCameraConfig { pub format: CameraFormat, pub memory: MemoryKind }
impl VirtualCameraConfig { pub fn new(format: CameraFormat, memory: MemoryKind) -> Self; }
```

### Session

```rust
pub trait VirtualCameraSession: Send {
    /// Reserves an empty one of three fixed slots (compare-and-swap `Empty` ->
    /// `Reserved`) for a frame the caller is about to produce. The node calls it before
    /// any GPU work and skips the camera frame on `None` (counted as `consumer_busy`).
    /// A readback still in flight keeps its slot, so several conversions in flight never
    /// compete for one slot. Dropping a `SlotToken` unused (a failed or dropped readback)
    /// returns the slot to `Empty`.
    fn reserve(&self) -> Option<SlotToken>;
    /// Fills the reserved slot with `frame` (slot states `Empty`, `Reserved`,
    /// `Full(seq)`, `Taken`), marks it `Full(seq)` and wakes the sender thread. Never
    /// waits, never fails for lack of room, and never allocates in steady state; the
    /// doorbell wake is the only system call (an exception like ADR-0028's). The sender
    /// takes the newest `Full` slot and drops older `Full` slots itself (`replaced`).
    /// A frame that breaks the config is refused: `InvalidFrame`, the slot returns to
    /// `Empty`, and the frame is dropped by the caller (system frames only: dropping one
    /// recycles its buffer and holds no lease).
    /// Errors only for a frame that breaks the config (size, format, memory kind);
    /// the frame is dropped and counted. Consumer and OS problems never fail `send`:
    /// they show in `stats`.
    fn send(&mut self, slot: SlotToken, frame: VideoFrame) -> Result<(), VcamError>;
    fn stats(&self) -> VcamStats;   // atomics, no lock
}
// Drop stops the sender thread and joins it (on the control thread, never on the
// render thread); the camera shows its still picture within 1 s. Thread count returns to its value before `open`.

/// A snapshot of the session's atomic counters. Read through accessors; new
/// counters are added without breaking callers.
#[non_exhaustive]
#[derive(Default, Clone, Copy)]
pub struct VcamStats { /* private */ }
impl VcamStats {
    pub fn sent(&self) -> u64;            // frames handed to the consumer side
    pub fn replaced(&self) -> u64;        // newer frame came before the sender took this one
    pub fn refused(&self) -> u64;         // send() errors
    pub fn consumer_busy(&self) -> u64;   // no free slot, or surface still in use outside RBS
    pub fn consumer_absent(&self) -> u64; // frames taken while no app reads the camera
    pub fn format_pending(&self) -> u64;  // frames sent while a format change waits for apps to close the camera
    pub fn os_errors(&self) -> u64;
    /// The last OS error, stored as a non-zero `u32` code in an atomic (0 = none).
    pub fn last_os_error(&self) -> Option<u32>;
}
```

Invariants (each a conformance test):

1. `send` returns whatever the consumer does (stalled, absent, crashed): checked with a stalled test consumer under a hang guard, never with a timing assertion (ADR-0032), and it performs no allocation after the first frame of a size (allocation counter).
2. The sender delivers the newest frame: after `send(a); send(b)` with the sender stalled, `b` is delivered, `a` is counted in `replaced` and dropped on the sender thread. With all three slots reserved or full, `reserve` returns `None`; a dropped token frees its slot.
3. Frames are dropped only by the sender thread, except a frame `send` refuses (system memory only).
4. No thread, timer or OS handle exists before `open` or after the session is dropped (P13).
5. `install` and `uninstall` are idempotent; `status` after them reports what they returned.

### Capabilities

| Key | Meaning |
| --- | --- |
| `vcam.input.memory` | Memory kinds `send` takes: `system` (native kinds are added with the adapter that needs them) |
| `vcam.input.format` | Pixel formats: `nv12` |
| `vcam.max_size` | Largest frame size announced to apps |
| `vcam.rates` | Frame rates announced to apps |
| `vcam.persistent` | The camera stays in the OS camera list while RBS is not sending |

The node picks the memory kind from `vcam.input.memory`, never from the platform (P3). States, errors and memory kinds for macOS (approval, restart, camera permission, native IOSurface frames) are added by the macOS ADR; every enum here is `#[non_exhaustive]`, so that is not breaking.

### Shared frame ring (`rbs-vcam-ring`)

An adapter support crate (ADR-0031) for adapters whose consumer lives in another process and takes system memory; a marked low-level module (every `unsafe` block has a `// SAFETY:` comment). Laid out over a byte slice the creator maps (on Windows, the section the media source creates):

- Header: magic `RBSVCAM1`, layout version, generation (written by the creator each time it creates or resets the section), width, height, format, stride, slot count (3), the writer's heartbeat and the reader's heartbeat (milliseconds of a system-wide monotonic clock, written at least every 250 ms while active; the crate reads no clock: every call that needs the time takes `now_ms: u64`, which the Windows adapter and the DLL take from `GetTickCount64` and tests pass themselves), and the index and sequence number of the newest complete slot.
- Each slot: a state word (`Free`, `Writing`, `Ready(seq)`, `Reading`) changed only with compare-and-swap, the frame's timestamp, and the pixels.
- The writer takes a `Free` slot (or the oldest `Ready` slot not being read), writes, then publishes it as newest. It never waits: no slot means the frame is dropped (`consumer_busy`). A slot in `Reading` whose reader heartbeat is older than 1 s is taken back. A changed generation makes the writer start over from the header.
- The reader takes the newest `Ready` slot by CAS to `Reading`, copies it, and frees it. It never sees a slot while it is written.
- The reader never trusts the header: it computes the layout (size, stride, slot count, offsets) from the format it announced, bounds every index by its own values, reads each shared field once per use, and treats any disagreement as "no frames".
- Either side treats a wrong magic or layout version as "no frames"; the app reports `NeedsUpdate`.
- "No frames" on the reader's side always means the source shows its still picture ("RBS Virtual Camera is off"): no writer heartbeat for 1 s, a header that disagrees with the announced format (for example during a pending format change; RBS counts those frames in `format_pending`), or a version mismatch.
- The section is named `Global\org.rbs.vcam.<user SID>`, created by the source, writable only by that SID (ADR-0066 decision 9). The source creates it with `CreateFileMapping`; `ERROR_ALREADY_EXISTS` for a section it did not create itself is accepted only when the section's owner is the Frame Server account and its access list is the one the source sets; otherwise the source shows the still picture and logs it (a squatted name never gets frames read from it). `rbs-vcam-ring` depends on `std` only.

## Errors and failure behaviour

```rust
#[non_exhaustive]
pub enum VcamError {
    Unsupported(String),     // platform or build cannot have the camera
    NotInstalled,
    PermissionDenied(String),// the user or OS refused (UAC cancelled, extension denied)
    InvalidConfig(String),
    InvalidFrame(&'static str),
    Os { code: i64, message: String },
}
```

The node never fails a tick because of the camera; the studio reports the camera's errors in its state and the app shows them under the camera's Start button.

## Performance requirements

`send` is on the render thread's hot path ([hot-paths.md](../architecture/hot-paths.md)): no allocation, lock, system call or wait; one atomic swap into the mailbox and a doorbell. The adapter's sender thread is not real-time but must keep up at 1920 x 1080 at 60 fps. The node's cost (scale, convert, export or readback) is measured per ADR-0066.

## Conformance tests

`rbs_vcam::conformance` runs against every adapter with a test consumer (the adapters provide one: an in-process reader of the ring for system memory):

- `send_never_blocks_with_a_stalled_consumer`
- `full_session_reserves_nothing`
- `dropped_token_frees_its_slot`
- `newest_frame_wins`
- `refuses_frames_that_break_the_config`
- `no_threads_before_open_or_after_drop`
- `replaced_frames_are_dropped_by_the_sender`
- `set_format_applies_when_no_app_reads`
- `install_and_uninstall_are_idempotent` (hardware-gated where the OS prompts: `RBS_VCAM_HW=1`)

Ring tests (in `rbs-vcam-ring`, run everywhere): property tests that a reader never observes a torn slot under any interleaving of one writer and one reader (loom in process), the writer never waits, published sequence numbers strictly increase, a stale reader's slot is taken back, a new generation resets the writer, and a corrupted header gives no frames; a fuzz target for the reader over arbitrary header and slot bytes; a two-process smoke test over a real mapped section on the Windows and Linux CI runners.

Source DLL test (Windows CI, no camera hardware): the test loads `rbs-vcam-mf-source` in process through its COM class factory, starts one stream, calls `RequestSample` against a ring written by a test writer, stops, and checks the samples' pixels, no allocation per sample, and that a panic injected into a COM entry point comes back as an `HRESULT`.
