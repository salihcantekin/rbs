# Spec: Media-file decode port

- Status: Draft
- Owner crate(s): `rbs-media-decode` (port), `rbs-media-decode-test` (fake adapter); adapters `rbs-media-anim` (GIF, APNG, WebP; built), `rbs-media-mf`, `rbs-media-avf` (planned)
- Related ADRs: [ADR-0108](../adr/0108-media-file-source.md), [ADR-0106](../adr/0106-picture-and-sound-on-one-source.md), [ADR-0007](../adr/0007-ffmpeg-as-optional-adapter.md)

## Purpose

Demux and decode of one local media file, behind a trait, so the media source can use the platform decoders, a pure-Rust animation decoder, or a later FFmpeg adapter without a scene change. The port returns pictures and sound with container timestamps. It owns no clock, cursor, loop, sleep, or ring; the media source does (ADR-0108 decision 4).

## Scope

In scope: opening a path the host accepted, seeking, per-stream reads, the stated duration, capability `media.decode.pixel`, the frame-duration rule, the size limits, the local-path check. Out of scope: pacing, rings, master-time mapping, resampling (the media source's worker), network URLs (refused), slideshows.

## Contract

```rust
pub trait MediaDecoderFactory: AdapterFactory {
    fn open(&self, path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError>;
}

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
10. Payload. ADR-0108 decision 4 names `rbs_core::media::VideoFrame` as the payload. The port wraps it: a system picture is plain bytes with its layout and colour (the node builds the `VideoFrame` with the master-time stamp it maps, which an adapter cannot know), and a native picture is a `VideoFrame` whose payload must be a native surface (`Picture::native` refuses a texture or system payload). Until the node split lands, the probe refuses a native picture so a zero-copy adapter cannot win and then fail in the node.

## Errors and failure behaviour

`DecodeError::Unsupported(what)` names a layout or a codec the adapter or the platform lacks (for example HEVC without the Store extension); the host tries the next adapter and tells the user what is missing. `DecodeError::NoTimestamp` is a container with neither a timestamp nor a frame duration. `DecodeError::Failed(text)` is any other open or decode failure. When every adapter refuses, the host reports the most specific error: `InvalidSetting` (for example no picture stream, or a size limit), then `Unsupported`, then `Failed` (`rbs-sources`, `probe_media`).

## Performance requirements

`open` and `seek` run on the media source's worker or probe thread, never on a real-time thread. A `cpu-upload` path on a hardware decoder is a measured fallback (P4) with its copies counted.

## Conformance tests

On the fake adapter (`rbs-media-decode-test`), with no platform decoder: per-stream reads and sound chunks in order (`tests/per_stream.rs`), seek moving both streams, a clip without sound, a zero-delay animation frame read as 100 ms; the frame-duration rule and size limits (`rbs-media-decode` unit tests); the local-only path checks (`path.rs`); adapter fallback and the most specific error (`rbs-sources/tests/media_path.rs`). Each real adapter adds a per-platform test with small fixture files; a codec the machine lacks is a skip with a reason.

`rbs-media-anim` (`tests/conformance.rs`, fixtures in `tests/fixtures`): a GIF, an APNG and an animated WebP of three 8x8 frames each give the same pictures, timestamps and frame durations (a stated 0 ms delay reads as 100 ms, and the stated length includes it), compose a partial frame over the one before, seek into a frame by composing forward, and end after a seek past the end; a still PNG is one picture with no frame duration and no length; a file that is no image is refused by content, not by extension.

## Adapter: `rbs-media-anim`

Pure Rust (`gif`, `png`, `image-webp`), on every platform, capability `cpu-upload`. It recognizes the container by its first bytes, so a renamed file still opens and anything else is `Unsupported` for the next adapter. Every picture is the composited canvas as straight-alpha sRGB `Rgba8` (GIF and APNG disposal and blending, WebP through its decoder). There is no keyframe: `seek` reopens the file and composes forward to the frame that covers the cursor. The length is the sum of the delays after the frame-duration rule, read from the frame headers without decoding pixels. A still image is one picture with no frame duration and no length, so the source holds it. There is no sound stream.
