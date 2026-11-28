# Vendored libopus

- Upstream: Xiph.Org libopus 1.5.2, <https://downloads.xiph.org/releases/opus/opus-1.5.2.tar.gz>
- Tarball SHA-256: `65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1`
- Licence: BSD-3-Clause (`libopus/COPYING`), compatible with GPL-3.0-or-later (ADR-0010).
- Files are unmodified copies. Included: `celt/` and `silk/` (top level and `silk/float`),
  and from `src/` only `opus.c`, `opus_decoder.c`, `opus_encoder.c`, `repacketizer.c`,
  `extensions.c`, `analysis.c`, `mlp.c`, `mlp_data.c` plus the headers.
- Left out: SIMD directories (`x86`, `arm`, `mips`), the fixed-point SILK sources, the multistream
  and projection APIs, the DNN code (deep PLC, DRED, OSCE) and all demo and test programs.
- To update: unpack a new tarball, copy the same file set, change the version in `build.rs`
  and the SHA above, and run the crate's tests including the slow RFC 8251 vector test.
