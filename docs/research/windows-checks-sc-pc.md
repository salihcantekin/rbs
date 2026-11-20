# Windows checks on SC-PC

Run these on the Windows desktop (SC-PC) from a checkout of `main` after `git pull`. They check code that is already committed. They do not add features.

Stay logged in at the console, or in a Remote Desktop session that stays connected. Disconnecting the session locks the desktop. Windows Graphics Capture, the camera, and the GPU encoders then fail or change behaviour. While the capture test runs, keep something moving on a display (a playing video). WGC does not deliver frames of a still screen.

Do not change encoder `keeps_input`, the Media Foundation camera upload, or the AMF 1 ms sleep from these results. Those waits need a separate bit-exact or driver-wait check. A green run here only says the current path still works.

## Build on this machine

```text
cargo test -p rbs-capture-wgc --lib
cargo test -p rbs-media-mf --lib
cargo test -p rbs-vcam-mf --lib
```

Why: ScreenCaptureKit's clock change has a Windows twin in `rbs-capture-wgc`. The Media Foundation swizzle spare and the virtual-camera slot write were not executed on Windows; this Mac only cross-compiled the capture crate.

## Display capture

```text
set RBS_CAPTURE_HW=1
cargo test -p rbs-capture-wgc --test conformance hardware_session hardware_thumbnails -- --nocapture
```

Why: the shared video clock now stamps WGC frames. The suite checks that a real display session still delivers frames, stays monotonic, and that thumbnails are pictures. `RBS_CAPTURE_HW` unset skips the hardware tests without failing.

## Camera

```text
set RBS_CAPTURE_HW=1
cargo test -p rbs-capture-mf --test conformance hardware_session_passes_conformance -- --nocapture
```

Why: the audit's camera copy (locked Media Foundation buffer, then a CPU upload) is still the shipping path. This run checks that a camera desktop apps may use still passes conformance. It does not remove that copy.

## Microphone and loopback

```text
set RBS_AUDIO_HW=1
cargo test -p rbs-audio-io-wasapi --test conformance -- --ignored --nocapture
```

Why: the one-channel push into the sample ring is in the tree. The ignored suite reads the default microphone, the default output's loopback, and an application playing a short tone. It takes about 15 seconds and needs real endpoints.

## Encoders already on this PC

```text
set RBS_REQUIRE_NVENC=1
cargo test -p rbs-codec-nvenc --test conformance --test hardware -- --nocapture

set RBS_REQUIRE_AMF=1
cargo test -p rbs-codec-amf -- --nocapture

cargo test -p rbs-codec-mf --test conformance media_foundation_passes_the_audio_suite -- --nocapture
```

Why: NVENC (RTX 4090), AMF (the AMD iGPU), and Media Foundation AAC were signed off on 2026-11-03. Re-run them so a later clock or readback change is not blamed on a broken encoder. `RBS_REQUIRE_NVENC=1` fails the run when H.264 NVENC is missing. `RBS_REQUIRE_AMF=1` fails when AMF probes to nothing. Do not turn `keeps_input` on from a pass.

## What this machine cannot decide

Linux DMA-BUF, QSV, and VAAPI need a Linux GPU machine. This PC does not replace that.
