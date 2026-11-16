# ADR-0132: The browser source runs CEF fetched on first use, with custom CSS, reload and interaction

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0109](0109-browser-source.md), [ADR-0006](0006-three-tier-plugin-model.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0052](0052-ai-agent-control-through-mcp.md), [ADR-0106](0106-picture-and-sound-on-one-source.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md)

## Context

ADR-0109 fixes the process model of `org.rbs.source.browser`: one sandboxed helper tree per Active source, outside the studio, the studio owns the frame memory, and the page has no API into RBS. It names CEF but leaves open how CEF is built, shipped and updated, and it excludes custom CSS and interaction. Today the helper is a test double that draws a checker ([scene sources](../specs/scene-sources.md#browser-orgrbssourcebrowser-adr-0109)).

Forces:

- **Size and zero cost (P13).** A CEF runtime is about 120 MB compressed and 250 MB unpacked per platform. Most users never add a browser source.
- **License.** RBS is GPL-3.0-or-later. CEF is BSD-3; Chromium is a set of permissive and weak-copyleft licenses; the Rust bindings (`cef` crate, tauri-apps/cef-rs) are MIT or Apache-2.0. All are GPL-compatible. A standard CEF build has no H.264 or AAC decoders inside the page (patent-encumbered codecs are off); VP8, VP9, AV1 and Opus work.
- **Security.** Chromium ships security fixes every few weeks. A pinned runtime ages.
- **Product.** Overlays from StreamElements, Streamlabs and chat widgets need a Chrome-compatible engine, often custom CSS (hide a background, change a font), a reload, and sometimes a click (log in, dismiss a banner). The usual tool has these, and the owner wants them.
- **Layers (P1).** Only adapter crates may depend on CEF. ADR-0109 decision 1 already keeps CEF out of the studio process.

## Decision

1. **Engine.** The runner is CEF, through the `cef` crate, in a new adapter crate `rbs-browser-cef` that builds the executable `rbs-browser-cef` (the tree root of ADR-0109 decision 1). No other crate depends on CEF bindings. `rbs-browser-runner` stays as the test double; tests must not need CEF. The runner uses the standard CEF build (no proprietary codecs). The help text of the source says that H.264 and AAC video inside a page do not play.

2. **The CEF runtime is not shipped with RBS; it is fetched on first use.** The owner chose this on 2026-11-16.
   - One file in `rbs-browser-cef` pins the CEF version, the download URL and the SHA-256 of the archive for each platform. Changing the version is one PR that changes that file.
   - The runtime lives in `<data dir>/runtimes/cef/<version>/`. Nothing is written there until the user asks.
   - When a browser source would start and the pinned runtime is missing, the source is Unavailable with reason `browser_runtime_missing`, and nothing is downloaded. The local user starts the download from the source's Properties (the size is shown first). The download runs off the render thread, can be cancelled, writes to a temporary name, verifies the SHA-256, then unpacks and renames into place. A file that fails the hash is deleted and never loaded.
   - Through the control API, `browser.runtime.status` (read) and `browser.runtime.install` exist for parity (U20). `browser.runtime.install` needs the network permission (ADR-0052); without it the answer is `not_permitted` with reason `network`.
   - When a new RBS version pins a newer CEF, the old runtime is not used. The new one is fetched by the same flow; the old directory is removed after the new one verified. Settings offers "Remove browser support", which deletes the runtime directory when no browser tree runs.
   - Before the runtime exists, the feature costs nothing beyond the type registration (P13).

3. **Custom CSS.** A new setting `css` (string, at most 64 KiB of UTF-8, default empty) is applied by the runner as a stylesheet after each main-frame document load, from the runner side only. It adds no binding the page can call, no RBS object in the page, and no script from the user. A change of `css` while the tree runs is sent over IPC and applied without restarting the tree. The settings help shows the common lines (transparent body, no scrollbars) as an example, not as a default.

4. **Reload.** A `browser.reload` command (control path, never `process`) reloads the page in the running tree, optionally bypassing the cache. It does not restart the process and does not take a new slot. A source without a running tree answers `browser_not_running`. Because a hidden page has no process (ADR-0107), every show already starts from a fresh load, so there is no "reload when shown" setting. A new setting `reload_on_file_change` (bool, default false) applies only to a `file` URL: the runner watches that file and reloads on change. The studio does not watch files.

5. **Interaction.** The desktop app gets an Interact window for one browser source. It shows the frame the studio already holds (no new frame path) and, only while that window has keyboard focus, sends pointer move, button, wheel, key and text input to the runner as IPC messages in page pixels. Nothing is forwarded at any other time, and no input is captured from the OS. While the Interact window is open the source counts as shown in the preview class for ADR-0107 Active and for the ADR-0109 cap. The control API gets `browser.input` with the same event set (`control` scope, `browser_not_running` when there is no tree), so a control surface can press a button on a page. An agent without the network permission may not call it (a page is a network source).

6. **Page audio** goes only to the ADR-0106 pin; the runner turns off CEF's own audio output, so a page never plays to an OS device directly. The Mixer strip is the only volume and mute. No separate audio setting.

7. **Runner protocol.** The IPC in `rbs-browser-ipc` stays private (ADR-0109 decision 1). It gains message kinds for input events, reload, a `css` update and runtime errors; frames and audio move to studio-allocated shared memory per ADR-0109 decisions 3 and 8. When CEF hands out a GPU texture (`OnAcceleratedPaint`), the studio opens the shared handle, copies on the GPU into its own texture, and releases the handle in the same supervisor step; the studio never samples a texture the helper owns. Message layout is specified in the scene-sources spec, not in the plugin SDK.

8. **Saved settings.** `css` and `reload_on_file_change` are added at the next free scene-format version (19 on 2026-11-16, confirmed at merge). An older file reads both as absent (empty, false).

9. **Checks, without CEF.** A hash mismatch never unpacks; a partial download is never loaded; a missing runtime reports `browser_runtime_missing` and starts nothing; `css` over 64 KiB is refused at the API with `invalid_params`; a `css` change does not restart the test-double tree; `browser.reload` keeps the same process; no input is forwarded while the Interact window is unfocused or closed; `browser.runtime.install` and `browser.input` return `not_permitted`/`network` without the network permission; an open Interact window makes a hidden source Active in the preview class and gives up the slot when closed.

## Consequences

- A user who never adds a browser source never downloads or loads CEF. The first browser source costs one download and the CEF start-up time.
- RBS now depends on a third-party download host for one feature. If the host is gone, the source reports `browser_runtime_missing`; an RBS-hosted mirror can replace the URL in the pin file without a contract change.
- CEF version moves are a release chore with a security reason; each one needs a short device run on Windows and macOS.
- macOS needs the CEF helper sub-apps and framework laid out as a bundle under the runtime directory, and signing before a public release (the same blocker as the Mac virtual camera).
- Spec follow-ups in the implementing PRs: scene sources (settings, reasons, IPC kinds), control API (`browser.reload`, `browser.input`, `browser.runtime.*`, the reasons, next free protocol minor), user data (`runtimes/`), glossary (Interact window), UI design system (Interact window, runtime card). The capture port and the plugin SDK are unchanged.
- Hot paths: the shared-memory and GPU-copy consume path needs `hot-path-review` and measurements (ADR-0109 decision 9); the runner itself is not a hot path of the studio.

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio** bundles CEF in every install, runs it in the app process, offers custom CSS, refresh, "shutdown when not visible" (off by default) and an Interact window. A page crash can end the app; pages often keep running hidden.
- **What RBS does better here:** no CEF bytes for users who do not use pages; a page crash does not end the show; a hidden page has no process; interaction and reload are also control API commands, so a control surface can press a page button; CSS changes apply without a restart.
- **Where RBS still falls short:** no H.264/AAC inside pages; a cap of two running pages until measured; no browser docks for the app UI (by ADR-0016, on purpose).

## Alternatives considered

- **Bundle CEF in every release.** Simpler first run, offline install. Rejected by the owner (2026-11-16): it charges every user about 120 to 250 MB. An offline installer can add the runtime later without a contract change.
- **Servo.** Pure Rust, MPL-2.0. Too many overlay sites break today, and page audio is weak. ADR-0109 decision 1 lets it replace the runner later.
- **OS web views (WebView2, WKWebView).** No real offscreen mode and no per-page audio capture.
- **Ultralight.** Proprietary license, not GPL-compatible.
- **A custom CEF build with H.264 and AAC.** A very large build and a patent question for an open source project.
- **Interaction directly in the preview.** Faster to reach, but it mixes editing the scene with driving a page. Kept for a later decision.
- **User JavaScript injection.** Gives the page author's or user's script a path the agent rules cannot bound. Not offered.

## Review

Independent review per ADR-0014. Record: reviewer, date, verdict, concerns and resolutions.
