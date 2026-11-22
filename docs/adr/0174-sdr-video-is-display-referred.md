# ADR-0174: SDR video is display-referred

- Status: Accepted
- Date: 2026-11-22
- Deciders: Salih (owner)

## Context

The [GPU compositor spec](../specs/gpu-compositor.md) linearised a Y'CbCr source tagged `TransferFunction::Bt709` with the inverse BT.709 OETF (camera curve, about gamma 1/0.45 with a linear toe), and the "Video" output conversion wrote linear light with the BT.709 OETF. sRGB sources (screen, window, image, browser, text, and imported media files, which reach the graph as RGBA8 tagged sRGB: `rbs-media-mf` `win.rs`, `rbs-media-avf` `mac.rs`) are linearised with the sRGB EOTF. The preview (`preview.wgsl`), screenshots and still exports (`rgba.wgsl`) and the "Screen" output encode linear light with the sRGB curve.

The two curves differ, so a picture was not passed through unchanged:

- **Preview of a camera** = `srgb_encode(inverse_oetf_709(v))`. It lifts shadows and midtones (8-bit codes: 40 becomes 56, 77 becomes 92, 126 becomes 138). On 2026-11-22 the owner compared the same Cam Link 4K scene in OBS 32.2.2 and RBS on Windows: the RBS preview looked brighter, flatter and more purple. The pixels sampled from the two screenshots match this formula within about two codes; matrix (BT.709) and range (limited) were already right.
- **Recording of an sRGB source in "Video" colour** (screen, window, image, browser, media file) = `oetf_709(srgb_eotf(v))`: code 128 becomes about 115. Those sources were darker in the file, the stream and the virtual camera than on screen and than in OBS.
- **Recording of a camera** round-tripped exactly (inverse OETF, then OETF), so the file was right, but it did not match what the preview showed.

Real-world SDR video is display-referred: a Rec. 709 file is made to look right when its R'G'B' codes are shown on a display (BT.1886 / sRGB-like, about gamma 2.2-2.4). Players, browsers, editors and OBS show the codes as they are. A camera's OETF is applied inside the camera; undoing it with the scene-referred inverse caused the mismatch.

The owner also asked (2026-11-22) for OBS-like per-camera colour space and range choices. That is a capture port change and has its own ADR (ADR-0175); this ADR does not depend on it.

## Decision

1. SDR Y'CbCr sources tagged `TransferFunction::Bt709` (which also stands for BT.601 and SDR BT.2020) must be linearised with the sRGB EOTF, the same curve as sRGB sources: `yuv_decode::params` maps BT.709 to the sRGB transfer code, and the inverse BT.709 OETF and its transfer code 1 are removed from the decode shaders (`composite.wgsl`, `wrap.wgsl`, `yuv_decode.wgsl`). `TransferFunction::Bt709` stays in `rbs-core` as the tag.
2. The "Video" output conversion (`ColorSpace::BT709_LIMITED`, `yuv.wgsl` `oetf` block) must encode linear light with the sRGB OETF, then the BT.709 matrix and limited range. Output files and streams keep their BT.709 tags. "Video" and "Screen" then differ only in range and tag; the Screen help text says so.
3. Tests must check: limited-range grey Y' 126 decodes to the sRGB EOTF of 110/219 (about 0.216 linear, code 128 on screen); the encoder writes the sRGB curve's codes for known colours; encode then decode returns the colour.
4. "Open shadows" stays as specified (a lift in linear light below middle grey, before the Video curve). It is now an extra lift on shadows that are already shown correctly, not a fix for darkening; the owner keeps it as an optional grade.
5. Linear-light composition (blending, scaling, filters) is unchanged; HDR (PQ, HLG) stays out of scope.

## Consequences

- Camera previews match OBS and players; camera recordings are unchanged (exact round trip before and after).
- Screen, window, image, browser and media-file sources in "Video" recordings, streams and the virtual camera (`YuvDownload::scaled_tone*`, `vcam.rs`) get their on-screen brightness. This is a visible, intended change.
- When the media worker's NV12/P010 upload is wired (`rbs-nodes` media worker, today not wired), BT.709-tagged files follow decision 1 automatically.
- Hot path: the decode and encode shaders change one curve for another of the same cost (one `pow` per channel; the decode drops a branch). Measured on cloud lavapipe (`cargo bench -p rbs-gpu --bench compositor`, 20 samples, criterion means), main against this change: `nv12_planes` (1080p Video encode) 15.75 to 14.60 ms, `nv12_decode` 10.80 to 11.42 ms, `nv12_cameras_4_direct` 31.38 to 31.76 ms; all within the run's spread (no change expected: same operation count). The `hot-path-review` checklist is in the commit. The 8-bit decode into `Rgba8UnormSrgb` now linearises with the same curve the store re-encodes; writing R'G'B' to a non-sRGB view would skip both, a possible follow-up.
- Spec updates in the same change: `gpu-compositor.md` (4:2:0 layers, YUV decode colour, conversion kernels, Video, Open shadows, tests), `ui-design-system.md` (Screen help text), and the shader comments. Tests that encoded the old curve change (`rbs-gpu` `tests/yuv.rs`, `tests/upload_download.rs`, `rbs-nodes` `tests/pipeline_gpu.rs`).
- Encoders (VideoToolbox, Media Foundation, NVENC, AMF) take frames converted by `YuvConverter` and only signal the tags, so they need no change.

## Alternatives considered

- **Change only the preview to encode with the BT.709 OETF.** The camera preview would match, but sRGB sources would look wrong in the preview, and "Video" recordings of screens would stay darker. Rejected.
- **Pure gamma 2.2 or BT.1886 (gamma 2.4) instead of the sRGB curve.** Equally display-referred, but sRGB sources and the preview already use the sRGB curve; one curve everywhere keeps every round trip exact. Differences are only near black. Rejected for consistency.
- **Offer the old camera curve as a per-source choice.** It was wrong for every source; a choice would only preserve a bug. OBS's per-device matrix and range choices are ADR-0175; tone is the Colour correction filter's job. Rejected.

## Review

- 2026-11-22, independent AI session (fresh context, ADR + specs + code only), first draft: **changes requested**. Direction confirmed as correct colour science. Fixed: the 40-to-54 example (it is 56); media files are sRGB RGBA8 sources today, not affected as video; Open shadows' purpose restated (decision 4); Screen help text (decision 2); virtual camera listed; test and spec lists completed; transfer code 1 removed (decision 1).
- 2026-11-22, second independent AI session on the revision and the code diff: decisions 1-5 **approve**, with one item: a Video encode-then-decode test. That test exists (`rbs-gpu` `tests/yuv_decode.rs` `nv12_round_trip` / `p010_round_trip`, `ColorSpace::BT709_LIMITED`, six colours) and passes with the new curve. The per-camera choice it also reviewed moved to ADR-0175 with its concerns.
- Owner, 2026-11-22: approved this work's ADRs and visual changes in advance ("Görsel onayları, adr'ların hepsini onaylanmış kabul ediyorum").
