# Spec: Media-file decode port

- Status: Draft
- Owner crate(s): `rbs-media-decode` (port), `rbs-media-decode-test` (fake adapter); adapters `rbs-media-anim` (GIF, APNG, WebP), `rbs-media-mf` (Windows), `rbs-media-avf` (macOS); the last two are `zero-copy` with a `cpu-upload` factory for measurements
- Related ADRs: [ADR-0108](../adr/0108-media-file-source.md), [ADR-0106](../adr/0106-picture-and-sound-on-one-source.md), [ADR-0007](../adr/0007-ffmpeg-as-optional-adapter.md)

## Purpose

Demux and decode of one local media file, behind a trait, so the media source can use the platform decoders, a pure-Rust animation decoder, or a later FFmpeg adapter without a scene change. The port returns pictures and sound with container timestamps. It owns no clock, cursor, loop, sleep, or ring; the media source does (ADR-0108 decision 4).

## Scope

In scope: opening a path the host accepted, seeking, per-stream reads, the stated duration, capability `media.decode.pixel`, the frame-duration rule, the size limits, the local-path check. Out of scope: pacing, rings, master-time mapping, resampling (the media source's worker), network URLs (refused), slideshows.

## Contract

```rust
pub trait MediaDecoderFactory: AdapterFactory {
    fn open(&self, path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError>;
    // Default: ignores the target and calls `open`.
    fn open_for(&self, path: &Path, target: &DecodeTarget) -> Result<Box<dyn MediaDecoder>, DecodeError>;
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeTarget { pub adapter_luid: Option<u64> } // DecodeTarget::none(), DecodeTarget::adapter(luid)

pub trait MediaDecoder: Send {
    fn seek(&mut self, cursor: Duration) -> Result<(), DecodeError>;
    fn next_picture(&mut self) -> Result<Option<PictureSample>, DecodeError>;
    fn next_sound(&mut self) -> Result<Option<SoundSample>, DecodeError>;
    fn has_audio(&self) -> bool;
    fn duration(&self) -> Option<Duration> { None }
}

pub struct PictureSample { pub pts: Option<Duration>, pub frame_duration: Option<Duration>, pub picture: Picture }
pub struct SoundSample { pub pts: Option<Duration>, pub pcm: InterleavedPcm }
pub struct Picture { pub width: u32, pub height: u32, data: PictureData }
impl Picture {
    pub fn system(width: u32, height: u32, layout: PictureLayout, color: SampleColor, bytes: Vec<u8>) -> Self;
    pub fn native(frame: VideoFrame) -> Option<Self>; // None unless the payload is a native surface
    pub fn data(&self) -> &PictureData;
    pub fn into_data(self) -> PictureData; // a native surface moves on with its lease
}
#[non_exhaustive]
pub enum PictureData {
    System { layout: PictureLayout, color: SampleColor, bytes: Vec<u8> },
    Native(rbs_core::media::VideoFrame),
}
```

Invariants:

1. `open` is called only with a path `accept_local_media_path` accepted (canonical, local, regular file). The port never opens a URL, a UNC path, a device, a pipe, or a directory.
2. Streams are read separately. `next_picture` returns the picture stream in presentation order; `next_sound` returns PCM of the first audio stream as interleaved `f32`. Other audio streams are ignored. An adapter whose demuxer reads in container order keeps a skew buffer of compressed packets, at most 1 s and 8 MiB, and fails with `Failed` beyond it.
3. `seek` moves both streams. The next picture is the first at or after the cursor, or an earlier keyframe; the caller skips pictures stamped before the cursor.
4. Frame duration rule (`frame_duration_rule`): an animated picture's delay of 10 ms or less reads as 100 ms; a video frame of zero duration continues by the stream's nominal duration, or, when the stream states none, is treated as unstated and continues by the previous frame's duration. The adapter applies it before returning.
5. `System` pictures are NV12 or P010 (`check_color` accepts the colour) or straight-alpha sRGB `Rgba8`. Every other layout is converted by the adapter, or `open` fails. `Native` pictures follow the capture port's native surface rules (lease returned after the importer retires it, a handle kept open 10 s after its last reference, LUID encoding); a source holds at most 18 native surfaces (filled picture slots plus 2) against the importer's `MAX_SURFACES` of 64.
6. A side over 8192 or a system frame over 128 MiB is refused (`refuse_picture`) before the caller allocates any buffer.
7. `duration` is the container's stated length, read once after open, or `None`.
8. From this version on, new trait methods have defaults and the public enums are `#[non_exhaustive]`, so neither breaks an adapter. `next_sound` and `has_audio` are required, so an adapter cannot claim sound and forget to deliver it.
9. Capability `media.decode.pixel` is a set of `zero-copy` and `cpu-upload`. The host picks the adapter named in the source's `adapter` setting, or else `zero-copy` before `cpu-upload`, then registration order, among adapters that open the file.
10. Payload. ADR-0108 decision 4 names `rbs_core::media::VideoFrame` as the payload. The port wraps it: a system picture is plain bytes with its layout and colour (the node builds the `VideoFrame` with the master-time stamp it maps, which an adapter cannot know), and a native picture is a `VideoFrame` whose payload must be a native surface (`Picture::native` refuses a texture or system payload). The picture node imports a native picture once, when it is first shown, through the importer the capture node uses; the imported texture repeats while the clock repeats the picture, and stays as the held picture in the end hold. A source keeps at most `NATIVE_WAITING` (15) native pictures decoded and not yet imported, in the worker and on the ring; with the one the worker has just decoded, the shown texture and the held texture that is the budget of 18. A picture over the budget is dropped and counted (`worker_dropped_pictures`). A failed import drops the picture, counts it (`import_failures`), and repeats the last texture. Native pictures the clock passed go back to the worker, which drops them (and their lease) off the render thread. `rbs-media-decode-test` has `ScriptFactory::native` (fake surfaces with counted leases) for the budget and lease tests (`rbs-nodes/tests/media_native.rs`).
11. Import device. The worker opens with `open_for` and the `DecodeTarget` the picture node recorded in its `prepare` from the render device: the adapter LUID on Direct3D 12 (`GpuContext::adapter_luid`, as the capture node's `import_preferences`), `none` elsewhere. A run that starts before the picture node was ever prepared (the sound node started it), and the probe, open with `DecodeTarget::none()`. An adapter whose native pictures must be on the import device (Media Foundation's shared D3D11 texture) gives system pictures without a LUID, and never a native picture on another adapter; it does not change its stated capability. A run keeps the pixel path it opened with until its next open.

## Errors and failure behaviour

`DecodeError::Unsupported(what)` names a layout or a codec the adapter or the platform lacks (for example HEVC without the Store extension); the host tries the next adapter and tells the user what is missing. `DecodeError::NoTimestamp` is a container with neither a timestamp nor a frame duration. `DecodeError::Failed(text)` is any other open or decode failure. When every adapter refuses, the host reports the most specific error: `InvalidSetting` (for example no picture stream, or a size limit), then `Unsupported`, then `Failed` (`rbs-sources`, `probe_media`).

## Performance requirements

`open` and `seek` run on the media source's worker or probe thread, never on a real-time thread. A `cpu-upload` path on a hardware decoder is a measured fallback (P4) with its copies counted.

## Conformance tests

On the fake adapter (`rbs-media-decode-test`), with no platform decoder: per-stream reads and sound chunks in order (`tests/per_stream.rs`), seek moving both streams, a clip without sound, a zero-delay animation frame read as 100 ms; the frame-duration rule and size limits (`rbs-media-decode` unit tests); the local-only path checks (`path.rs`); adapter fallback and the most specific error (`rbs-sources/tests/media_path.rs`). Each real adapter adds a per-platform test with small fixture files; a codec the machine lacks is a skip with a reason.

`rbs-media-anim` (`tests/conformance.rs`, fixtures in `tests/fixtures`): a GIF, an APNG and an animated WebP of three 8x8 frames each give the same pictures, timestamps and frame durations (a stated 0 ms delay reads as 100 ms, and the stated length includes it), compose a partial frame over the one before, seek into a frame by composing forward, and end after a seek past the end; a still PNG is one picture with no frame duration and no length; a file that is no image is refused by content, not by extension.

## Adapter: `rbs-media-anim`

Pure Rust (`gif`, `png`, `image-webp`), on every platform, capability `cpu-upload`. It recognizes the container by its first bytes, so a renamed file still opens and anything else is `Unsupported` for the next adapter. Every picture is the composited canvas as straight-alpha sRGB `Rgba8` (GIF and APNG disposal and blending, WebP through its decoder). There is no keyframe: `seek` reopens the file and composes forward to the frame that covers the cursor. The length is the sum of the delays after the frame-duration rule, read from the frame headers without decoding pixels. A still image is one picture with no frame duration and no length, so the source holds it. There is no sound stream.

## Adapters: `rbs-media-mf` and `rbs-media-avf`

Platform video files: Media Foundation on Windows (`rbs-media-mf`, synchronous `IMFSourceReader`) and AVFoundation on macOS (`rbs-media-avf`, `AVAssetReader`). Both work the same way:

- Each stream has its own reader on the file, with only that stream selected, so reading one stream never queues samples of the other (invariant 2 without a skew buffer). The sound reader is created on the first `next_sound`; a source that is never heard opens no second reader (P13).
- Native pictures (`zero-copy`, the factories' `new`):
  - AVFoundation asks for IOSurface-backed `420v` (NV12, video range, the colour from the buffer's attachments) and hands out the decoded `CVPixelBuffer`'s `IOSurface`, kept in one of 24 slots while a lease names it. The Metal importer's texture retains the surface, so its address cannot be reused while the importer caches it; the adapter therefore needs no handle grace, and frees a free slot's buffer on the worker and a dropped decoder's pool on its janitor once no lease names it.
  - Media Foundation, opened with `open_for` and the render adapter's LUID, decodes on a D3D11 device of that adapter (`IMFDXGIDeviceManager`, hardware transforms) and asks the video processor for `ARGB32` (`B8G8R8A8`). Each picture is copied on the GPU, cropped to the display aperture, into one of 24 shared NT-handle textures; the worker waits on a D3D11 fence, so the picture is `Ready`. A replaced texture and a dropped decoder's pool keep their handles open for the 10 s grace on the adapter's janitor. A `B8G8R8A8` picture is 4 bytes a pixel where NV12 is 1.5; importing NV12 shared textures on Direct3D 12 is a later step.
  - A picture the platform gives in memory, a file whose device or type cannot be set, and a picture when every slot is referenced are copied as below.
- Copied pictures (`cpu-upload`, the factories' `cpu_upload`, and the fallback): requested as 8-bit BGRA/BGRX (Media Foundation's advanced video processing converts and deinterlaces; VideoToolbox decodes) and copied once into `Rgba8`, cropped to the display aperture and turned upright when the rows are bottom-up. Sound is requested as interleaved 32-bit float at the track's own rate and channel count.
- `seek`: Media Foundation moves both readers (`SetCurrentPosition`). An `AVAssetReader` cannot move once it reads, so the AVFoundation adapter replaces the picture reader with one whose time range starts at the cursor, and recreates the sound reader on the next `next_sound`.
- A missing decoder is `Unsupported` and names it (for example: HEVC needs the HEVC Video Extensions from the Microsoft Store). A file with no picture stream is `Unsupported`, so the next adapter can try it.
- A frame of unstated or zero duration continues by the track's nominal rate, then by the previous frame (invariant 4).
- Not handled yet: a track's rotation (`preferredTransform`, the display matrix), colour beyond sRGB conversion by the platform, HDR. Media Foundation also returns the AAC encoder priming and end padding untrimmed (about 21 ms of sound before the picture at the start); trimming by the track's edit list comes with the play range work.

Capability: `zero-copy` for `new`, `cpu-upload` for `cpu_upload` (the measured fallback of ADR-0108 decision 4). Each adapter's PR records its numbers from `examples/decode_bench.rs` (`copy` as the third argument measures the copy): time per picture and real-time factor at 1080p60 and 4K60, and the process CPU and memory measured outside the process.

Conformance (`tests/conformance.rs`, run on its own platform; on other platforms the adapter only checks that it refuses): `fixtures/clip.mp4` (H.264 Main 160x90, coded 160x96, 10 fps, 5 frames, red with a green top-left quarter, and 0.5 s of 48 kHz stereo AAC). The tests check upright, cropped `Rgba8` pictures with 100 ms stamps and durations; a stated length near 0.5 s; a seek that moves both streams; interleaved float sound covering 0.5 s give or take one AAC frame; and the refusal of a file that is not media. The native tests check NV12 IOSurfaces (macOS) or `B8G8R8A8` shared textures on the first adapter, read back (Windows), with the same stamps, and a seek after the pictures were released. A machine without the decoder, or one that decodes the fixture off the GPU, skips with a reason.
