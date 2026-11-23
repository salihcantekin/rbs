# RBS

**Produce live. Record with more control. Build what comes next.**

Rust Broadcasting Software is an open live production, streaming and recording
project built in Rust. It brings familiar studio workflows together with
production tools built into the application — and a modular foundation designed
to make new capabilities easier to add.

The goal is more than an OBS alternative: a studio that can grow beyond parity,
with less reliance on assembling a production workflow from separate add-ons.

> **Development status: pre-alpha.** The desktop application and core media
> paths are implemented, but features and platform support are incomplete.
> RBS is not yet a drop-in replacement for OBS or a production-stability
> guarantee. See [current status](docs/status.md) for what is implemented.

## A studio built for the whole workflow

**Go live with familiar tools.** Arrange scenes and sources, apply transitions
and transforms, mix audio, and record and stream together. Capture, image,
text, media-file and browser sources are part of the studio; availability
depends on the platform and installed components.

**Keep more of your production built in.**

- **Sound:** source and master faders, meters and headphone monitoring, plus
  gain, noise gate, ducking and master loudness metering.
- **Edit after the show:** record marked sources separately alongside the
  master, add recording markers, and export supported editor project formats.
- **Make sources work harder:** live text sources include clocks, countdowns
  and timers; browser sources support page interaction, CSS and scripts.
- **Bring your setup:** the OBS import wizard and CLI import supported scene
  collections and profiles, with a report of approximated or skipped items.
- **Automate:** a partial control API and MCP integration expose studio
  operations to remote clients and AI tools.

Some of these workflows are still being built out. For example, the engine
supports multiple audio buses and recording tracks, but bus editing and
track-list UI are not yet available. See [current status](docs/status.md) for
the scope and limitations of each feature.

## Designed to grow

The desktop studio sits on a headless media engine. Capture, audio, encoding
and transport use ports and replaceable adapters, so new implementations can
be added without tying the core to a particular vendor. The media graph,
capability model and parameter schemas provide a foundation for new sources,
filters and other extensions. These are design principles, not a promise that
every future integration will be effortless.

Native GPU encoder input is preferred when the device and encoder support it,
with system-memory fallback otherwise. The plugin host can discover manifests,
but loading plugin code is not implemented yet; crash isolation is partial.
Linux support is incomplete and has not been tested on a GPU-equipped Linux
machine; video-file decoding is not yet implemented on Linux. See the
[architecture](docs/architecture/overview.md),
[status](docs/status.md) and [specifications](docs/specs/) for details.

## Try a development build

Install the Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml)
and the platform's build prerequisites. The development scripts build the app
with its companion programs; they do not produce an installer.

macOS / Linux:

```sh
tools/dev/run.sh --build --no-browser --fresh-config
```

Windows (PowerShell):

```powershell
.\tools\dev\run.ps1 -Build -NoBrowser -FreshConfig
```

These commands use a fresh temporary configuration and skip the browser
runner, so browser sources are unavailable. For browser support, release
builds, prerequisites and more options, see
[`tools/dev/README.md`](tools/dev/README.md). Back up your existing production
setup and test locally before relying on RBS for a live show.

## Build and contribute

```sh
cargo build --workspace
cargo test --workspace
```

Start with [contribution guidance](CONTRIBUTING.md). The project records
contracts and design decisions alongside the code, and welcomes contributions
that keep backends replaceable and real-time work predictable.

| Explore | Purpose |
| --- | --- |
| [Current status](docs/status.md) | Implemented features and known gaps |
| [Vision](docs/vision.md) | Goals and intended audience |
| [Architecture overview](docs/architecture/overview.md) and [principles](docs/architecture/principles.md) | System layers and design rules |
| [Specifications](docs/specs/) and [decisions](docs/adr/) | Contracts and rationale |
| [UI principles](docs/ui/principles.md) | Rules for the desktop experience |
| [Roadmap](docs/roadmap.md) | Planned phases and exit gates |
| [Agent instructions](CLAUDE.md) | Guidance for AI coding agents |

## License

RBS is free software under the [GNU General Public License v3.0 or later](LICENSE).
See [ADR-0010](docs/adr/0010-project-license.md) for the reasoning.

Copyright (C) 2026 Salih Cantekin and contributors.
