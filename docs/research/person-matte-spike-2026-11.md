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

## First results (2026-11-24, cloud container, 4 vCPU, no GPU)

- Licences read from the repositories' LICENSE files: RVM is GPL-3.0 (compatible with RBS), MODNet is Apache-2.0. The MediaPipe and Apple Vision rows are still unverified.
- Clips: four 1620x1080 30 fps clips from the owner's Mac camera (6 to 14 s): a still person talking (clip1) and head turns (clip3) are the useful ones; low light and a second person passing were not recorded. They are in the project files folder `person-matte/clips/`.
- RVM MobileNetV3 fp32 through ONNX Runtime on CPU, input 960x640, downsample ratio 0.25: median 30 ms per frame (about 33 fps on 4 vCPU, no GPU). It is a probe for quality, not an RBS measurement.
- Visual result on green: clean edges around hair, beard and shoulders on the still clip; head turns keep the hair edge and ear. A frame with fast motion shows blur on the face and a soft hair edge, which is motion blur in the source. No halo or missing hair seen in the sampled frames. Frames reviewed: 8 of about 1,000, by eye; not a measurement.
- Consequence for ADR-0179: RVM-class quality is reachable at a cost that a GPU can easily absorb; the open question is the engineering of the GPU-native run, not whether a model is good enough.
- Next: the same clips through MODNet and MediaPipe for comparison, a GPU timing on the Windows machine, and a hand-checked frame set for edge error.

## Mac timings (2026-11-24, Apple M4 Pro, macOS 26.7.1, onnxruntime 1.19.2)

First 120 frames of each of the 4 owner clips (1620x1080), first 5 skipped, median of the four per-clip medians; inference only (RVM pre-processing adds about 1.2 ms). The result files stayed on the Mac in `/tmp/matte-spike/out`.

| Engine | Input | Median ms | Worst p90 | Notes |
| --- | --- | --- | --- | --- |
| RVM MobileNetV3 fp32, CPU | 960x640, ratio 0.25 | 14.2 | 15.7 | about 5.8 cores busy |
| RVM, CoreML provider (ALL, GPU, ANE alike) | same | 20.6 to 20.9 | 23.8 | 244 of 303 nodes on CoreML in 48 pieces; the rest on CPU |
| RVM, fixed-shape model, CPU / CoreML | same | 17.9 / about 40 | 49.8 | slower |
| Apple Vision fast, GPU | native | 7.1 | 9.4 | 256x192 mask, blocky edges, jagged shoulder |
| Apple Vision balanced, GPU | native | 19.0 | 21.8 | 512x384 mask, CPU about 16% |
| Apple Vision accurate, GPU | native | 56.6 | 61.3 | 2016x1512 mask, too slow for 30 fps |
| Apple Vision balanced / fast, CPU only | native | 42.5 / 16.0 | 46.2 | |

Findings: ONNX Runtime's CoreML provider does not help RVM (pooling with `ceil_mode=1` and one 1-D Concat are unsupported, so the graph splits and copies cost more than they save); RVM on the Mac GPU or Neural Engine needs our own conversion. RVM on CPU is fast but uses about six cores (against rule 17). Vision balanced on the GPU fits 30 fps and looks close to accurate on these clips; RVM keeps more hair detail and handles motion blur better. Vision on the Neural Engine could not be measured from a sandboxed shell (it failed to build the inference plan), so that run is still open.

## Open tasks

- **Windows measurement (RTX 4090 and AMD iGPU)**, waiting for SC-PC (off on 2026-11-24): RVM through ONNX Runtime with DirectML and CUDA as a baseline, then one GPU-native prototype. Runs of 1 to 2 minutes, one device thread at a time.
- Apple Vision on the Neural Engine: run `vision_probe balanced` from a normal Terminal (a sandboxed shell cannot build the inference plan).
- Low-light and second-person clips (not recorded yet); MODNet and MediaPipe comparison on the same clips.

## Mac round 2: MediaPipe and MODNet (2026-11-24, M4 Pro)

Same four clips, first 120 frames, first 5 skipped, median of per-clip medians, inference only. Licences read from the official repositories: MODNet and MediaPipe Apache-2.0 (the ONNX files used are community conversions whose model cards say Apache-2.0: Xenova/modnet and onnx-community MediaPipe; the MediaPipe conversion was not compared with the official tflite). The clips contain no low-light or second-person scenes, which the owner waived.

| Model / path | ms per frame | Notes |
| --- | --- | --- |
| MediaPipe selfie 256x144, CPU | 1.6 | cheapest; arms sometimes drop out |
| MediaPipe selfie 256x256, CPU | 2.5 (1 thread 3.9, 2 threads 2.9) | also caught a blurred hand |
| MediaPipe 256x256, CoreML | 7.6 | slower than CPU: graph split into 16 + 11 pieces |
| MODNet 384x256, CPU / CoreML | 34.2 / 32.9 | CoreML takes 117 of 242 nodes |
| MODNet 768x512, CPU | 134 | one frame leaked background (white area) |
| RVM, CPU (default / 2 / 1 threads) | 14.2 / 25.6 / 44.0 | about 6 cores at default |
| Apple Vision fast / balanced / accurate, GPU | 9.4 / 18.1 / 55.8 | CPU about 16% |

Flicker: the wall patch test gave exactly 0 for every model (it does not separate them). Whole-frame mean absolute alpha change x1000 on the still-talking clip: RVM 0.23, MediaPipe 0.26, MODNet 0.30, Vision 0.36, all very low; moving clips mix real motion into the number and cannot be compared. Apple Vision on the Neural Engine is still unmeasured (a sandboxed shell cannot build the plan; run `vision_probe` from a normal Terminal). A Windows kit with DirectML probes is on the Mac in `/tmp/matte-spike/windows_kit` for the open Windows measurement.

Suggested quality tiers from these numbers (to confirm after Windows): Fast = MediaPipe on one or two CPU threads (2 to 4 ms); Balanced = Vision balanced on Mac, MODNet 384 elsewhere; High = Vision accurate, or RVM (GPL-3.0 is compatible with RBS, edge quality best, but needs our own GPU path to be lean). Edge quality was judged by eye on contact sheets, not measured.
