# ADR-0132: The browser source runs CEF fetched on first use, with custom CSS, reload, interaction and an opt-in sign-in

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0109](0109-browser-source.md), [ADR-0006](0006-three-tier-plugin-model.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0052](0052-ai-agent-control-through-mcp.md), [ADR-0106](0106-picture-and-sound-on-one-source.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md), [ADR-0125](0125-projector-windows.md)
- Blocked by: acceptance of ADR-0106, ADR-0107 and ADR-0109

## Context

ADR-0109 fixes the process model of `org.rbs.source.browser`: one sandboxed helper tree per Active source, outside the studio, the studio owns the frame memory, and the page has no API into RBS. It names CEF but leaves open how CEF is built, shipped and updated, and it excludes custom CSS and interaction. Today the helper is a test double that draws a checker ([scene sources](../specs/scene-sources.md#browser-orgrbssourcebrowser-adr-0109)).

Forces:

- **Size and zero cost (P13).** A CEF runtime is about 120 MB compressed and 250 MB unpacked per platform. Most users never add a browser source.
- **License.** RBS is GPL-3.0-or-later. CEF is BSD-3; Chromium is a set of permissive and weak-copyleft licenses (some LGPL parts); the `cef` crate (tauri-apps/cef-rs) is MIT or Apache-2.0. All are GPL-compatible. A standard CEF build has no H.264 or AAC decoders inside the page; VP8, VP9, AV1 and Opus work.
- **Build.** The `cef` crate's `cef-dll-sys` downloads CEF and needs CMake at build time, and the crate version fixes the CEF version.
- **Security.** Chromium ships security fixes every few weeks. A pinned runtime ages. A downloaded executable is a supply-chain target.
- **Product.** Overlays from StreamElements, Streamlabs and chat widgets need a Chrome-compatible engine, often custom CSS, a reload, sometimes a click, and some need the user to sign in once.
- **Layers (P1).** Only adapter crates may depend on CEF.

## Decision

1. **Engine and build.** The runner is CEF through the `cef` crate, in a new adapter crate `rbs-browser-cef` that builds the executable `rbs-browser-cef` (the tree root of ADR-0109 decision 1). No other crate depends on CEF bindings. The crate is a workspace member but not in `default-members`, so `cargo build`, `cargo test` and `cargo clippy` of the workspace never download CEF or need CMake; `cargo xtask cef build` builds it from the same pinned archive the app downloads. `rbs-browser-runner` stays as the test double; tests must not need CEF. The runner uses the standard CEF build. The source's help says H.264 and AAC video inside a page do not play.

2. **One pin.** `crates/rbs-browser-cef/cef-pin.toml` holds, per platform, the CEF version, the archive URL, the exact byte size and the SHA-256. An `xtask` check fails when the pinned version differs from the CEF version of the `cef` crate in `Cargo.lock`. A version move is one PR that changes the pin and the crate version together, and runs a short device check on Windows and macOS. A maintainer takes the SHA-256 from an archive whose upstream checksum (the CEF build index) matched, and records both in the PR. Releases must not pin a CEF older than the newest stable CEF by more than eight weeks.

3. **The runtime is fetched on first use and never shipped with the app** (owner, 2026-11-16).
   - It lives in the local data directory (not roaming): `%LOCALAPPDATA%\RBS\runtimes\cef\<version>\` on Windows, `~/Library/Application Support/RBS/runtimes/cef/<version>/` on macOS, `$XDG_DATA_HOME/rbs/runtimes/cef/<version>/` on Linux. Nothing is written there until the user asks.
   - A browser source whose pinned runtime is missing is Unavailable with reason `browser_runtime_missing`, holds no slot (ADR-0109 decision 7), and starts no download. At app start, if the open scene collection has a browser source and the runtime is missing, the app shows one notice offering the download.
   - The download is started by the local user from the source's Properties or the notice (size shown first), or by `browser.runtime.install`. It runs off the render thread over HTTPS only, follows no redirect to another host, refuses more bytes than the pinned size, and writes into `runtimes/cef/.staging/`, which is emptied before each attempt. The archive must match the pinned SHA-256 before it is opened. Unpacking refuses absolute paths, `..` segments and links that point outside the staging directory. A `.complete` marker is written last, then the staging directory is renamed to `<version>`. A directory without the marker is never loaded.
   - The pinned host is the RBS project's own GitHub release, which carries the CEF and Chromium notices and a pointer to their sources. Until that mirror exists, the pin may point to the upstream CEF build host; the hash rule is the same.
   - The pinned version and the one before it may stay on disk, so an older RBS build next to a newer one keeps working. Older directories are removed after a new runtime verified. "Remove browser support" in Settings deletes every runtime directory when no browser tree runs.
   - macOS: a public release must download a runtime RBS signed (from the RBS mirror); the helper does not get the disable-library-validation entitlement. Unsigned runtimes are for development builds.
   - Until a runtime exists the feature costs only the type registration (P13).

4. **Custom CSS.** A new setting `css` (UTF-8, at most 64 KiB, default empty). The runner applies it with one fixed script into the main frame, passing the CSS only as a JSON-escaped string value; no other script is ever injected and the page gets no binding. The runner sends no frame of a new main-frame document before the CSS is applied, so unstyled frames never reach the studio. Iframes are not styled. A change while the tree runs is applied without a restart. Because `@import` and `url()` reach the network, setting `css` needs the network permission (ADR-0052), like `url`. The help shows common lines (transparent body, no scrollbars) as an example, not as a default.

5. **Reload.** `browser.reload {source, bypass_cache}` reloads the page in the running tree without restarting the process and without a new slot; with no running tree the answer is `browser_not_running`. A hidden page has no process (ADR-0107), so every show already loads fresh; there is no "reload when shown" setting. `reload_on_file_change` (bool, default false) applies only to a `file` URL: the runner watches that file's directory (not recursive) and reloads on any change. The studio does not watch files.

6. **Interaction.** The desktop app gets an Interact window for one browser source. It draws the studio-held frame through the same GPU path as projector windows (ADR-0125), with no per-frame CPU copy. Only while it has keyboard focus it sends pointer, button, wheel, key and text events to the runner over IPC, in page pixels. Nothing is forwarded at any other time, and no OS input is captured. While it is open the source counts as shown in the preview class: this adds one Active cause to ADR-0107 decision 6 and is part of that ADR's acceptance. Under `browser_cap` or `browser_runtime_missing` the window shows that reason (with the download offer for the latter) and forwards nothing. Text events are never logged and never enter the command log.

7. **Page audio** goes only to the ADR-0106 pin; the runner turns off CEF's own audio output. The Mixer strip is the only volume and mute.

8. **Runner protocol.** The IPC in `rbs-browser-ipc` stays private (ADR-0109 decision 1). It gains a heartbeat, input events, reload, `css` update and runtime errors. Frames and audio cross in studio-allocated shared memory or the studio's shared texture (ADR-0109 decision 8): on Windows the helper copies CEF's accelerated-paint texture into the studio's texture inside the callback, under a keyed mutex acquired with a 100 ms timeout; on macOS into the studio's IOSurface with the same rule. If the import of the studio's texture fails at launch, the tree uses the shared-memory path for its life and the metrics count it. The GPU copy and conversion get `risky-review` and `hot-path-review`.

9. **Control API** (next free protocol minor at merge; U20 parity):
   - `browser.reload` and `browser.input` are `live` effects (they change what is on air, and a click can do anything in a page) and also need the network permission. `browser.input` refuses with `browser_not_running` when no tree runs; an agent cannot wake a hidden page.
   - `ui.browser.interact.open` and `.close` are client-scoped like `ui.panel.*`.
   - `browser.runtime.status` (`read`): `missing`, `downloading {bytes, total}`, `verifying`, `ready {version}`, `failed {reason}`. Topic `browser.runtime.changed`.
   - `browser.runtime.install` and `browser.runtime.remove` (`edit`, network permission for install). `remove` refuses with `busy` while a tree runs.
   - `browser.forget_sign_in {source}` (`edit`), see decision 10.

10. **Opt-in sign-in** (owner decision pending; recommended option written here). By default each tree gets a fresh profile, discarded when it stops (ADR-0109 decision 10). A new setting `keep_signed_in` (bool, default false) gives that source one persistent profile under the local data directory, `browser-profiles/<source id>/`, holding cookies and storage only for that source. Permission grants stay denied either way. "Forget sign-in" in Properties and `browser.forget_sign_in` delete that profile (refused with `busy` while the tree runs; the UI stops the tree first). Removing the source deletes its profile.

11. **Saved settings.** `css`, `reload_on_file_change` and `keep_signed_in` are added at the next free scene-format version (19 on 2026-11-16, confirmed at merge). An older file reads them as absent (empty, false, false).

12. **Checks, without CEF.** Wrong hash, wrong size, a redirect to another host, an escaping archive entry or a missing `.complete` marker never produce a loadable runtime; a missing runtime reports `browser_runtime_missing`, holds no slot and downloads nothing; the pin check fails when the pin and `Cargo.lock` disagree; `css` over 64 KiB is `invalid_params`; `css` and `browser.input` without the network permission are `not_permitted`/`network`; a `css` change and `browser.reload` keep the test-double process; no input is forwarded while the Interact window is unfocused or closed; an open Interact window makes a hidden source Active in the preview class and releases the slot when closed; `forget_sign_in` deletes only that source's profile.

## Consequences

- A user who never adds a browser source never downloads or loads CEF. The first one costs a download and the CEF start-up time.
- One feature depends on a download host; the RBS mirror and the pin keep that under the project's control.
- CEF moves are a release chore with a security deadline (decision 2), each with a short device run.
- Public macOS releases need code signing of the runtime, the same blocker as the Mac virtual camera.
- Spec follow-ups in the implementing PRs: scene sources (settings, reasons, IPC kinds), control API (methods, effects, reasons, topic), user data (local data directory, `runtimes/`, `browser-profiles/`), glossary (Interact window), UI design system (Interact window, runtime card and notice). The capture port and plugin SDK are unchanged.

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio** bundles CEF in every install, runs it in the app process, offers custom CSS, refresh, "shutdown when not visible" (off by default) and an Interact window, and keeps one shared browser profile.
- **What RBS does better here:** no CEF bytes for users who do not use pages; a page crash does not end the show; a hidden page has no process; reload and input are control API commands, so a control surface can press a page button; CSS changes apply without a restart and never show unstyled frames; sign-in is per source and can be forgotten.
- **Where RBS still falls short:** no H.264/AAC inside pages; two running pages until measured; iframes are not styled; no browser docks for the app UI (ADR-0016, on purpose).

## Alternatives considered

- **Bundle CEF in every release.** Rejected by the owner (2026-11-16): it charges every user about 120 to 250 MB. An offline installer can add the runtime later without a contract change.
- **Build CEF bindings in the default workspace.** Every build and test run would download CEF and need CMake.
- **Servo.** Too many overlay sites break today, weak page audio. ADR-0109 lets it replace the runner later.
- **OS web views (WebView2, WKWebView).** No real offscreen mode, no per-page audio capture.
- **Ultralight.** Proprietary, not GPL-compatible.
- **A custom CEF build with H.264 and AAC.** Very large build and a patent question.
- **Interaction directly in the preview.** Mixes editing the scene with driving a page. Later decision.
- **User JavaScript.** A path the agent rules cannot bound. Not offered.
- **A shared persistent profile for all pages.** One page could read another's sign-in.

## Review

Round 1: independent AI session, 2026-11-16. Verdict: changes requested. Concerns:
1. The `cef` crate downloads CEF and needs CMake at build time; its version fixes CEF. -> Out of `default-members`, xtask build, pin check against `Cargo.lock` (decisions 1, 2).
2. Download security under-specified. -> Hash provenance, HTTPS, no cross-host redirect, size cap, staging and marker, archive path checks, RBS mirror (decisions 2, 3).
3. macOS signing of a downloaded framework. -> Signed runtime from the RBS mirror for public releases; no library-validation exception (decision 3).
4. `<data dir>` undefined; config dir roams on Windows. -> Local data directory per platform (decision 3).
5. Updates take pages off air; deleting old runtimes breaks side-by-side builds. -> Start-up notice, keep pinned plus previous, pin-age rule (decisions 2, 3).
6. Sign-in contradicts the discarded profile. -> Opt-in per-source `keep_signed_in` with forget (decision 10, owner decision pending); ADR-0109 decision 10 points here.
7. CSS injection is script injection, reaches the network, flashes unstyled frames. -> Fixed script with escaped data, network permission, no frame before CSS, iframes out of scope (decision 4).
8. Control API gaps. -> Effects, `runtime.remove`, status and topic, no API wake of a hidden page, no logging of text, no slot for a missing runtime (decisions 3, 6, 9).
9. Interact window. -> ADR-0107 Active cause stated, ADR-0125 GPU path, behaviour under cap and missing runtime (decision 6).
10. GPU copy sync and failure. -> Copy into the studio's texture inside the callback under a keyed mutex with a timeout; fallback for a failed import; reviews named (decision 8, ADR-0109 decision 8).
11. License notices for a mirror or installer. -> The mirror carries notices and source pointers (decision 3).
Minor: watch only the file. -> Watch its directory (decision 5).

Resolutions (author, 2026-11-16): written into the decisions listed above. A round-2 check is pending.
