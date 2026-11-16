# rbs-denoise

RNNoise speech denoiser for RBS ([ADR-0130](../../docs/adr/0130-voice-filters-noise-suppression-compressor-limiter-eq.md) decision 5).

This crate is a vendored copy of [nnnoiseless](https://github.com/jneem/nnnoiseless) 0.5.2, the Rust port of Xiph's [RNNoise](https://github.com/xiph/rnnoise). It is used under the BSD-3-Clause licence in `LICENSE-RNNOISE`; the files `src/features.rs`, `src/pitch.rs`, `src/rnn.rs`, `src/util.rs` and `src/weights.rnn` come from upstream and carry a header naming their patches. `src/lib.rs` is new.

## Patches against upstream

Keep this list complete, so an upstream fix can be ported by diffing.

1. **FFT.** `easyfft` (lazy thread-local planner: allocates and plans on the first call on a thread, a map lookup on every call) is replaced by `realfft` plans and scratch buffers owned by each `Denoiser` and made in `Denoiser::new`. The spectra are `[Complex32; 481]` arrays. The `InputValues` report of the inverse transform is ignored, as upstream does through easyfft (the DC and Nyquist imaginary parts are zero anyway).
2. **Tables.** The window, DCT table and normalisation (upstream: a `OnceCell` static filled on first use) are built once by the first `Denoiser::new` and shared read-only.
3. **Model.** Only the built-in model. It is read once into owned `Vec<i8>` layers with a safe byte conversion; upstream's `unsafe` `&[u8]` to `&[i8]` cast is gone. `RnnState` no longer borrows the model.
4. **No panics on the frame path.** `assert!`s on frame and feature sizes are replaced by array types; `checked_sub(..).unwrap()` by `saturating_sub` with the invariant stated; the pitch lag is clamped to `PITCH_MAX_PERIOD`; non-finite input samples are read as silence and non-finite output samples are written as silence; an FFT length error (impossible by construction) gives silence.
5. **Gain floor.** `Denoiser::set_max_reduction_db` limits the band gains that are applied (not the ones the network remembers) to a floor; with no floor the output is upstream's.
6. **Reset.** `Denoiser::reset` (and `reset` on the features, pitch finder and RNN state) returns to a silent history without allocating.
7. **Removed.** The C API, the `dasp` signal adapter, the training code, `shift_input`, `Biquad::filter_in_place` and custom-model loading.

## Contract

48 kHz mono, frames of 480 samples in the i16 range. The output of a frame is the frame before (one frame of latency). `process_frame` never allocates, locks, touches a thread-local or panics (tests: `tests/denoise.rs`). Its output agrees with `nnnoiseless` 0.5.2 to below -90 dBFS on the same input.
