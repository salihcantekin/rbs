# Spec: Virtual camera port

- Status: Draft
- Owner crate(s): `rbs-vcam` (port, shared frame ring, conformance), `rbs-vcam-test` (test adapter), adapters `rbs-vcam-cmio` (macOS), `rbs-vcam-mf` (Windows); companion binaries `rbs-vcam-cmio-extension`, `rbs-vcam-mf-source`; the node in `rbs-nodes`
- Related ADRs: [0066](../adr/0066-virtual-camera.md), [0003](../adr/0003-ports-and-adapters.md), [0013](../adr/0013-zero-cost-when-unused.md), [0025](../adr/0025-encoder-surface-export.md), [0038](../adr/0038-io-graph-nodes.md)
- Related specs: [backend-ports.md](backend-ports.md), [media-types.md](media-types.md), [io-nodes.md](io-nodes.md), [recording.md](recording.md)

## Purpose

The port through which RBS offers its picture to other apps as a camera. Every OS mechanism (CoreMediaIO Camera Extension on macOS, `MFCreateVirtualCamera` on Windows, `v4l2loopback` on Linux later) is an adapter behind this one contract; the engine sends frames and never sees the OS API.

## Scope

In scope: installation state, adding and removing the OS camera, opening a sending session, the non-blocking frame hand-off, the shared-memory frame ring for consumers in another process, capability keys, errors and the conformance suite.

Out of scope: audio (a virtual microphone is a separate decision), scaling and colour conversion (the node does them on the GPU), the OS consumers' behaviour (mirroring in self-view is the meeting app's choice), several virtual cameras at once (one camera per computer for now).

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
    NeedsApproval,
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
    /// Removes it. Idempotent; `NotInstalled` afterwards.
    fn uninstall(&self) -> Result<InstallState, VcamError>;
    /// Opens a sending session. Fails with `NotInstalled` unless `status` is `Installed`.
    fn open(&self, config: &VirtualCameraConfig) -> Result<Box<dyn VirtualCameraSession>, VcamError>;
}

#[non_exhaustive]
pub struct VirtualCameraConfig {
    pub width: u32,          // even, within vcam.max_size
    pub height: u32,
    pub rate: FrameRate,     // one of vcam.rates
    pub format: PixelFormat, // Nv12
    pub memory: MemoryKind,  // one of vcam.input.memory
}
```

### Session

```rust
pub trait VirtualCameraSession: Send {
    /// Hands `frame` to the adapter's sender thread and returns at once. Never blocks,
    /// never allocates in steady state, never calls the OS. The mailbox holds one frame:
    /// a frame not yet taken by the sender is replaced and counted (`replaced`).
    /// Errors only for a frame that breaks the config (size, format, memory kind);
    /// the frame is dropped and counted. Consumer and OS problems never fail `send`:
    /// they show in `stats`.
    fn send(&mut self, frame: VideoFrame) -> Result<(), VcamError>;
    fn stats(&self) -> VcamStats;   // atomics, no lock
}
// Drop stops the sender thread and joins it; the camera shows its still picture
// within 1 s. Thread count returns to its value before `open`.

#[derive(Default, Clone, Copy)]
pub struct VcamStats {
    pub sent: u64,         // frames handed to the consumer side
    pub replaced: u64,     // newer frame arrived before the sender took this one
    pub refused: u64,      // send() errors
    pub consumer_busy: u64,// no free slot / surface still in use on the consumer side
    pub consumer_absent: u64, // frames taken while no app reads the camera
    pub os_errors: u64,
    pub last_error: Option<ErrorCode>,
}
```

Invariants (each a conformance test):

1. `send` returns within the hand-off budget whatever the consumer does (stalled, absent, crashed), and performs no allocation after the first frame of a size.
2. The sender delivers the newest frame: after `send(a); send(b)` with the sender stalled, `b` is delivered and `a` is counted in `replaced`.
3. A native frame is the adapter's single consumer's (ADR-0025): it is dropped (its lease released) once the consumer side no longer reads its surface, never earlier and never on the render thread.
4. No thread, timer or OS handle exists before `open` or after the session is dropped (P13).
5. `install` and `uninstall` are idempotent; `status` after them reports what they returned.

### Capabilities

| Key | Meaning |
| --- | --- |
| `vcam.input.memory` | Memory kinds `send` takes: `metal` (IOSurface native frames), `system` |
| `vcam.input.format` | Pixel formats: `nv12` |
| `vcam.max_size` | Largest frame size announced to apps |
| `vcam.rates` | Frame rates announced to apps |
| `vcam.persistent` | The camera stays in the OS camera list while RBS is not sending |

The node picks the memory kind from `vcam.input.memory` (`metal` before `system` when the GPU can export), never from the platform (P3).

### Shared frame ring (`rbs_vcam::ring`)

For adapters whose consumer lives in another process and takes system memory. Laid out over a byte slice the adapter maps (a named section on Windows):

- Header: magic `RBSVCAM1`, layout version, width, height, format, stride, slot count (3), the writer's heartbeat (a monotonic tick in milliseconds, written at least every 250 ms while sending), and the index and sequence number of the newest complete slot.
- Each slot: a state word (`Free`, `Writing`, `Ready(seq)`, `Reading`) changed only with compare-and-swap, the frame's timestamp, and the pixels.
- The writer takes a `Free` slot (or the oldest `Ready` slot not being read), writes, then publishes it as newest. It never waits: no slot means the frame is dropped (`consumer_busy`).
- The reader takes the newest `Ready` slot by CAS to `Reading`, copies or wraps it, and frees it. It never sees a slot while it is written.
- Either side treats a wrong magic, version or size as "no camera" and opens nothing.

## Errors and failure behaviour

```rust
#[non_exhaustive]
pub enum VcamError {
    Unsupported(String),     // platform or build cannot have the camera
    NotInstalled,
    ApprovalPending,         // macOS: waiting for the user in System Settings
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

`rbs_vcam::conformance` runs against every adapter with a test consumer (the adapters provide one: an in-process reader of the ring for system memory, a surface-use probe for IOSurfaces):

- `send_never_blocks_with_a_stalled_consumer`
- `newest_frame_wins`
- `refuses_frames_that_break_the_config`
- `no_threads_before_open_or_after_drop`
- `native_frames_are_released_off_the_render_thread`
- `install_and_uninstall_are_idempotent` (hardware-gated where the OS prompts: `RBS_VCAM_HW=1`)

Ring property tests (in `rbs-vcam`, run everywhere): a reader never observes a torn slot under any interleaving of one writer and one reader (loom or exhaustive small-step model), the writer never waits, published sequence numbers strictly increase, and a corrupted header opens nothing.
