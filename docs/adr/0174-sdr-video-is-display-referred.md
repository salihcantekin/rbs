# ADR-0174: Decode and encode SDR video with the sRGB curve (display-referred)

- Status: Proposed
- Date: 2026-11-22
- Deciders: Salih (owner)

## Context

The [GPU compositor spec](../specs/gpu-compositor.md) linearises a Y'CbCr source tagged `TransferFunction::Bt709` with the inverse BT.709 OETF (camera curve, about gamma 1/0.45 with a linear toe), and the "Video" output conversion writes linear light with the BT.709 OETF. sRGB sources (screen, window, image, browser, text) are linearised with the sRGB EOTF. The preview (`preview.wgsl`), screenshots and still exports (`rgba.wgsl`) and the "Screen" output encode linear light with the sRGB curve.

The two curves differ, so a picture is not passed through unchanged:

- **Preview of a camera or video file** = `srgb_encode(inverse_oetf_709(v))`. It lifts shadows and midtones (code 40 becomes 54, 77 becomes 92, 126 becomes 138 in 8-bit full-scale terms). On 2026-11-22 the owner compared the same Cam Link 4K scene in OBS 32.2.2 and RBS on Windows: the RBS preview looked brighter, flatter and more purple. Every pixel sampled from the two screenshots matches this formula to within one code; matrix (BT.709) and range (limited) were already right.
- **Recording of an sRGB source in "Video" colour** = `oetf_709(srgb_eotf(v))`: code 128 becomes about 115. Screen captures, images and browser pages are darker in the file than on screen and than in OBS.
- **Recording of a camera** round-trips exactly (inverse OETF, then OETF), so the file is right, but it does not match what the preview showed.

Real-world SDR video is display-referred: a Rec. 709 file is made to look right when its R'G'B' codes are shown on a display (BT.1886 / sRGB-like, about gamma 2.2-2.4). Players, browsers, editors and OBS show the codes as they are. A camera's own OETF is applied inside the camera; undoing it with the scene-referred inverse is the cause of the mismatch, not a property of the source.

## Decision

1. SDR Y'CbCr sources tagged `TransferFunction::Bt709` (which also stands for BT.601 and SDR BT.2020, `rbs-core` `video.rs`) must be linearised with the sRGB EOTF, the same curve used for sRGB sources. The decode transfer code for BT.709 maps to sRGB (`yuv_decode::params`); the inverse BT.709 OETF is removed from the decode shaders (`composite.wgsl`, `wrap.wgsl`, `yuv_decode.wgsl`).
2. The "Video" output conversion (`ColorSpace::BT709_LIMITED`, `yuv.wgsl` `oetf` block) must encode linear light with the sRGB OETF, then the BT.709 matrix and limited range. "Video" and "Screen" then differ only in range. "Open shadows" stays as specified (a lift in linear light before that curve).
3. Output files keep their tags: "Video" stays BT.709 primaries, BT.709 transfer, BT.709 matrix, limited range. The tag describes the intended display (BT.1886), which is how the codes are produced.
4. Result that tests must check: an 8-bit Y'CbCr source code decoded and encoded back with the same matrix and range comes back unchanged (within rounding), and an sRGB source pixel written in "Video" colour has the same R'G'B' code it had on screen. The preview of a BT.709 source equals its R'G'B' codes (within rounding).
5. Linear-light composition (blending, scaling, filters) is unchanged; only the curve to and from linear changes. HDR (PQ, HLG) stays out of scope.

## Consequences

- Camera and video-file previews match OBS and media players; camera recordings are unchanged (exact round trip before and after).
- Screen, window, image and browser sources in "Video" recordings get their on-screen brightness (they were darker); this is a visible change in existing workflows, intended.
- Hot path: the decode and encode shaders change one curve for another of the same cost (one `pow` per channel). Needs before/after numbers on the encode conversion and compositor benches, the `hot-path-review` checklist and the owner's approval (P15).
- Spec updates in the same change: `gpu-compositor.md` (4:2:0 layers, YUV decode colour, output conversion "Video"), and the comments in `yuv.wgsl`, `composite.wgsl`, `wrap.wgsl`, `yuv_decode.wgsl`. Tests that encode the old curve (`rbs-gpu` `tests/yuv.rs`, `tests/upload_download.rs`, `rbs-nodes` `tests/pipeline_gpu.rs`) change to the sRGB curve.
- The macOS VideoToolbox and other encoders take frames already converted by `YuvConverter`, so they need no change. Imported media decoded by AVFoundation or FFmpeg with a BT.709 tag follow rule 1 automatically.
- `TransferFunction::Bt709` stays in `rbs-core` as the tag value; it no longer selects a different curve on the GPU.

## Alternatives considered

- **Change only the preview to encode with the BT.709 OETF.** The camera preview would match, but sRGB sources would then look wrong in the preview, and "Video" recordings of screens would stay darker. Rejected.
- **Pure gamma 2.2 or BT.1886 (gamma 2.4) instead of the sRGB curve.** Equally display-referred, but sRGB sources and the existing preview/screenshot code already use the sRGB curve; one curve everywhere keeps every round trip exact. Differences are only near black. Rejected for consistency.
- **Keep the scene-referred decode and add a per-source "camera curve" option.** Adds a setting to fix a default that is wrong for nearly all sources. Rejected.

## Review

Pending: independent review per ADR-0014.
