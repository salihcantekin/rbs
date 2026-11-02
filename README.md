# RBS

**Rust Broadcasting Software**: an open, modular live production, streaming and recording engine written in Rust.

RBS aims to be a long-lived alternative to OBS Studio, built on a foundation that is:

- **Solid**: a crash in a source or plugin never takes down a live stream or a recording.
- **Swappable**: every external library, protocol and hardware API sits behind a stable interface. FFmpeg, NVENC, PipeWire or RTMP can be replaced without touching the core.
- **Efficient**: frames stay on the GPU from capture to encoder, the audio path never allocates, and every change is measured against CPU, GPU and memory budgets.
- **Open to contributors**: clear specs, recorded decisions and conformance tests, so anyone (human or AI) can add a backend or plugin with confidence.

> Status: pre-alpha. This repository currently holds the foundation documents and the workspace skeleton. No usable application yet.

## Where to start

| Read this | To learn |
| --- | --- |
| [docs/vision.md](docs/vision.md) | What we build, for whom, and what we will not build |
| [docs/architecture/overview.md](docs/architecture/overview.md) | How the system fits together |
| [docs/architecture/principles.md](docs/architecture/principles.md) | The rules every change must respect |
| [docs/adr/](docs/adr/) | Why each major decision was made |
| [docs/ui/principles.md](docs/ui/principles.md) | Binding UI rules |
| [docs/specs/](docs/specs/) | Contracts: backend ports, media graph, plugins, performance budgets |
| [docs/roadmap.md](docs/roadmap.md) | Phases and exit gates |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to contribute |
| [CLAUDE.md](CLAUDE.md) | Instructions for AI coding agents working in this repo |

## Building

```sh
cargo build --workspace
cargo test --workspace
```

The minimum supported Rust version is pinned in [`rust-toolchain.toml`](rust-toolchain.toml).

## License

RBS is free software under the [GNU General Public License v3.0 or later](LICENSE). See [ADR-0010](docs/adr/0010-project-license.md) for the reasoning.

Copyright (C) 2026 Salih Cantekin and contributors.
