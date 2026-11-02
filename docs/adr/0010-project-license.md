# ADR-0010: Project license

- Status: Proposed
- Date: 2026-11-02

## Context

The license affects which libraries can be bundled (x264 and GPL FFmpeg are GPL), whether closed-source plugins and commercial use are possible, and how attractive the project is to contributors and companies.

## Options

| Option | Bundled GPL encoders (x264, GPL FFmpeg) | Closed-source plugins | Notes |
| --- | --- | --- | --- |
| GPL-3.0-or-later | Yes | Only across process or WASM boundaries, subject to legal review | Same family as OBS |
| MPL-2.0 | No, only as user-installed optional adapters | Yes | File-level copyleft |
| Apache-2.0 OR MIT | No, only as user-installed optional adapters | Yes | Most permissive, common in Rust |

AAC and HEVC patent questions apply regardless of the license.

## Decision

Open. To be decided by the project owner, ideally with legal advice, before the first public release. Until then the repository is all rights reserved.

## Consequences

ADR-0007 already keeps FFmpeg dynamically linked and optional, so every option above remains possible.
