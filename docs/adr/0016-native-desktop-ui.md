# ADR-0016: Native desktop UI, no web technology

- Status: Accepted
- Date: 2026-11-02
- Supersedes: the UI toolkit candidates named in ADR-0008 ("Slint or Tauri"); the rest of ADR-0008 stands

## Context

ADR-0008 left the desktop toolkit open between Slint and Tauri (a web UI in a system web view). The project owner has decided that the RBS desktop UI must be native, not web-based. Reasons that support this: lower memory and idle CPU (ADR-0013, U5), direct rendering of the engine's GPU surface for preview and program without a browser compositor in between, no dependency on per-OS web view versions, and a consistent look across platforms.

## Decision

1. The RBS desktop application and every panel inside it are rendered by a native Rust UI toolkit. The desktop app must not embed a web view or HTML/CSS/JavaScript runtime for its UI.
2. Plugin panels are declarative: a plugin (sandbox or process tier) describes its panel as a tree of design-system components bound to its state, and the host renders it natively. Plugins do not ship HTML, CSS or JavaScript for UI.
3. The browser source (CEF in a separate process, ADR-0006) is unaffected: it renders web content as video, not as application UI.
4. Remote clients of the control API (ADR-0008) may be built with any technology, including web, outside the desktop app. They are separate products and not part of the desktop UI.
5. Toolkit choice among native Rust toolkits is made with a comparison prototype in Phase 2 against the requirements in the [UI design system spec](../specs/ui-design-system.md#requirements-on-the-ui-toolkit). Candidates:

Facts below are as understood on 2026-11-02 and must be verified in the prototype.

| Toolkit | Strengths | Concerns |
| --- | --- | --- |
| Slint (front-runner) | Pure Rust runtime, declarative `.slint` markup, small footprint, own widget renderer with platform styles, accessibility work in progress | Licence (GPLv3, royalty-free or commercial) must fit ADR-0010. wgpu texture import sits behind unstable, version-pinned features, which couples RBS's `wgpu` version (ADR-0005) to Slint's. Right-to-left and bidirectional text support to be verified against requirement 4 |
| iced | Pure Rust, MIT licence, wgpu-based renderer, redraws only on change in recent releases | Screen-reader accessibility not in released versions as far as known; many custom components needed |
| Qt through `cxx-qt` | Most mature accessibility, bidi text and widget set | LGPLv3 or commercial licence; C++ toolchain and Qt version churn; zero-copy preview needs per-platform native texture sharing with `wgpu` |

Immediate-mode toolkits (egui) are acceptable for internal debug tools only, because their accessibility and fit with a token-based component library do not meet the UI rules.

## Enforcement

- `deny.toml` bans web view and web UI crates (`wry`, `tauri`, `webview2-com`, `webkit2gtk`) in the whole workspace, and allows the CEF bindings only as a dependency of the browser-source adapter. CI runs `cargo deny check bans`.
- Remote clients live outside this repository's desktop app crates and are not covered by the ban.

## Consequences

- Lower resource use and tighter GPU integration; no web view security surface in the app.
- The component library must be built natively; no reuse of web component ecosystems.
- Plugin panels are limited to what the declarative panel format can express ([plugin SDK spec](../specs/plugin-sdk.md#panels)); charts and live previews are host-provided components.

## Alternatives considered

- Tauri / web view UI: fastest UI development and shared code with web panels; rejected by the owner, and it adds memory, idle cost and a second rendering stack.
- Electron: same as above with a larger footprint.

## Review

- Reviewer: independent AI session (fresh context; given only this ADR, related ADRs, UI rules and specs)
- Date: 2026-11-02
- Verdict: approve after changes
- Concerns: (1) panel format had no contract -> Panels section with a versioned WIT world in the plugin SDK spec; (2)-(4), (9) toolkit facts inaccurate or unsupported -> table reworded with licence, accessibility, wgpu coupling and bidi caveats, marked for verification; (5) ADR-0008 had no pointer -> status line notes the superseded clause; (6) remote clients vs token rules -> remote clients declared outside U6, CSS token export kept for them only; (7) no enforcement -> cargo-deny ban in CI; (8) commit hygiene -> status flips split from this ADR.
- Owner approval: Salih, 2026-11-02 (project thread: "native olmalı")
