# Person matte spike (ADR-0179 step 1): desk research and measurement plan

Date: 2026-11-24. Status: desk research done; measurements need the reference machines. Facts marked (verify) come from web search results, not from the model repositories.

## Candidates

| Candidate | What it is | Licence | Size | Notes |
| --- | --- | --- | --- | --- |
| Robust Video Matting (RVM), MobileNetV3 | Video matting with recurrent state; alpha plus foreground; best known for hair edges and temporal stability | GPL-3.0 (verify in the repo; RBS is GPL-3.0-or-later, so compatible) | ONNX 15 MB fp32, 7.5 MB fp16; about 4 M parameters (estimate from file size) | Needs 4 recurrent tensors carried between frames; downsample ratio set per input size |
| RVM, ResNet50 | Same, bigger backbone | GPL-3.0 | 107 MB fp32 | Likely too heavy for a live filter |
| MODNet | Trimap-free portrait matting, single image (no temporal state) | Apache-2.0 per recent pages; an old fork README says CC BY-NC-SA 4.0 (verify the official repo before any use) | about 6.5 M parameters, about 31 GFLOPs at 512x512 (webcam variant) | Needs temporal smoothing added by us |
| MediaPipe selfie segmentation | Tiny coarse person mask | Apache-2.0 (verify) | well under 1 MB | Fast; coarse edges; the model OBS users run |
| Apple Vision person segmentation | OS API, quality levels fast/balanced/accurate | OS | none | macOS only; CPU or GPU texture in, buffer out (verify what it accepts) |

## How to run on the GPU in Rust (decision 3a vs 3c of ADR-0179)

- The studio uses wgpu 30 (`rbs-gpu`), D3D12 first on Windows. A GPU-native backend means our own compute shaders for a fixed network: for MobileNetV3-class nets about 30 to 40 layer types (conv, depthwise conv, hard-swish, add, resize, GRU-style gates in RVM). Doable but real work, and tuning conv kernels for speed on a 4090, an AMD iGPU and an M4 Pro is the cost driver.
- `burn` (WGPU backend, ONNX import to Rust code) is the most active Rust option found; `wonnx` targets exactly "ONNX on wgpu" but its maintenance is unverified; `tract` is CPU-first. Each would be a new dependency and needs the same checks as ADR-0179 decision 3 (frames cross wgpu buffers, no readback). Whether any can share our `wgpu::Device` and take our textures must be tested, not assumed.
- ONNX Runtime with DirectML (Windows) or CoreML (macOS) is the fallback; it is proven but normally takes CPU tensors, which brings the small readback exception (decision 3).

## Measurement plan (needs the owner's machines)

1. Test set: about 6 clips of 20 s, 1080p30, from Salih's own camera: talking still, waving hands, fast head turns, fine hair against a busy background, low light, a second person passing. Plus a ground-truth set of about 30 hand-checked frames.
2. Quality: edge error on the hand-checked frames (mean absolute alpha error in a 16 px band around the outline), temporal flicker (frame-to-frame alpha change on static regions), visual check on hair and fingers.
3. Cost per candidate at 1080p30 on Windows (RTX 4090 and the AMD iGPU) and Mac (M4 Pro): GPU ms per frame, CPU use, memory, power if available, and added latency.
4. Backend path: for each candidate, the cheapest way found to run it (ONNX Runtime with DirectML or CoreML as the quick baseline, then one GPU-native prototype of the best model).
5. Output: a table appended to ADR-0179 and the chosen model, with its licence verified from the repository.

## What can start without the owner's machines

- Licence verification in the model repositories (not just web search).
- A throwaway harness (outside the workspace, not merged) that runs ONNX candidates with ONNX Runtime on a clip and writes alpha PNGs, so quality can be compared on screen.
- Prototype conv, depthwise and GRU compute kernels in WGSL against the existing `rbs-gpu` test context (golden values from the ONNX run).

## What needs the owner

- Camera clips and permission to run the harness on SC-PC and the Mac (one device thread at a time, runs of 1 to 2 minutes, project rules).
