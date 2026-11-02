# Roadmap

Durations are rough estimates for a small experienced team assisted by AI agents; they will be recalibrated after Phase 0.

| Phase | Goal | Exit gate |
| --- | --- | --- |
| 0. Vertical slice (6-8 weeks) | Windows: Graphics Capture -> wgpu compositor -> hardware encoder -> RTMP, all on the GPU. Small prototypes on macOS and Linux to validate capture and interop | 1080p60 stream with resource use at or below OBS on the same machine; ADRs updated with findings |
| 1. Core (4-6 months) | `rbs-core`, media graph, master clock, audio graph, scene document and command log, fMP4/MKV recording, RTMP and SRT, headless engine with control API | All ports defined with conformance suites; 4-hour soak test without RBS-caused drops |
| 2. Usable alpha (3-4 months) | Desktop UI, cameras and audio devices, basic filters, transitions, multiple canvases, OBS scene collection importer | External testers can replace OBS for a simple stream |
| 3. Plugin SDK and beta (3-4 months) | Native, WASM and process plugin tiers, browser source, WHIP, virtual camera, first third-party plugins | SDK 1.0 frozen |
| 4. 1.0 (2-3 months) | Stability, performance, HDR, accessibility, docs, packaging and auto-update | Success criteria in [vision.md](vision.md) met |
| Post-1.0 | Remote guests (WebRTC, SFU, roles and moderation), NDI, Decklink, Teams/Zoom integrations | Delivered as plugins |

## Decisions needed before Phase 0 ends

- License ([ADR-0010](adr/0010-project-license.md)).
- Tier-1 platforms and reference machines.
- Desktop UI toolkit (Slint vs Tauri) after a small comparison prototype.
- Control API schema technology.
