# ADR-0145: Add-ons: one place to download, verify and install what RBS does not ship

- Status: Proposed
- Date: 2026-11-18
- Deciders: Salih (owner)
- Builds on: [ADR-0132](0132-browser-engine-css-interaction.md), [ADR-0109](0109-browser-source.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0051](0051-user-data-profiles-and-scene-collections.md), [ADR-0052](0052-ai-agent-control-through-mcp.md), [ADR-0006](0006-three-tier-plugin-model.md)
- Supersedes in part: the ADR-0132 clauses listed in decision 12. Everything else in ADR-0132 stands.
- Spec: [add-ons](../specs/add-ons.md)

## Context

ADR-0132 decides that the CEF runtime is fetched on first use and never shipped with the app, and specifies a safe download (pinned SHA-256 and size, HTTPS, a host list, staging, archive path checks, a `.complete` marker). None of it is built. Today the Add Source menu hides Browser unless `rbs-browser-helper` sits next to the app, and that helper is a test double (`crates/rbs-app/src/studio_host.rs`, the `browser_tree.is_some()` filter). A user sees no browser source and no reason.

The owner (2026-11-18) wants that download to be a general mechanism, not browser code:

- Settings gets one section that lists what can be downloaded, shows what is installed, and where the user downloads or removes it. The browser engine is not installed by default. Browser stays in the Add Source menu; while its engine is missing, choosing it opens that section on the browser engine's row.
- RBS manages the list of known items. Only the browser engine is in it for now. A dynamic list may come later.
- The same mechanism must later serve other things: plugins, and files users share with each other, such as output profiles, workspaces (layouts), scene collections and themes. What is downloaded may stop being "an extension" and become plain data, and the design must still hold.

Forces:

- **Different trust.** A runtime or plugin is code that runs with the user's rights; a profile is data that RBS parses. They need different rules for where they may come from.
- **One safe path.** Download and unpack safety is easy to get subtly wrong. It must exist once and be tested once, without a network.
- **Zero cost (P13).** A user who never opens the section or adds a browser source must pay nothing beyond a small built-in list.
- **Layers (P1).** Core and port crates never see HTTP, archives or file systems of this kind.

## Decision

1. **An add-on has three independent parts.** Each add-on is described by an *origin* (where it comes from), a *transfer* (how its bytes arrive and are verified) and a *kind* (what it is and how it is installed). A new kind adds one installer and must not change the transfer code; a new origin must not change any installer.

2. **Kinds.** `runtime` is the only kind this ADR builds: an archive unpacked into a versioned directory that one consumer loads (the CEF runtime, id `org.rbs.browser-engine`). Planned kinds, each needing its own change: `plugin` (code; ADR-0006 rules, plus a signature) and data kinds (`output_profile`, `workspace`, `scene_collection`, `theme`) that install through the existing `Document` import of ADR-0051, which validates the file, shows the user what it adds and adds it only after confirmation. The kind set is closed per RBS version: an unknown kind in a catalog entry is skipped and logged, never guessed, and a test fails when a compiled-in entry has a kind this build does not know.

3. **Origins and trust.** This ADR has one origin: the built-in catalog, a list compiled into the app. Its entries for code-bearing kinds (`runtime`, later `plugin`) carry per platform the URL, the exact byte size, the SHA-256 and the hosts the download may touch. The browser engine's entry is ADR-0132's pin file `crates/rbs-browser-cef/cef-pin.toml` (not written yet), which gains the entry fields of the spec (`id`, `kind`, `name`, `summary`, `notices`, and per platform `unpacked_size`, `format`, `strip`); ADR-0132's `xtask` check compares its `version` with the `cef` crate in that crate's `Cargo.lock`. Code-bearing kinds must come only from an origin whose entries carry a hash RBS pinned or a signature RBS checks. Later origins (a local file the user picks, a remote catalog) are separate changes; a local file may install only data kinds, and a remote catalog needs its own ADR and a signature over the catalog.

   **Release gate.** Until the runner runs sandboxed (ADR-0132; today it sets `no_sandbox`), release builds do not list the browser engine, do not list Browser in Add Source and show no start-up notice; a browser source in an opened collection is Unavailable with `browser_runtime_missing` and no link to the section. Development builds list everything. The gate applies to the API too: in a release build `addon.list` omits the engine and `addon.install` answers `not_found` for it. On Linux the sandbox is Chromium's namespace sandbox only (no setuid `chrome-sandbox`); where unprivileged user namespaces are blocked, the source is Unavailable with `browser_sandbox_unavailable` and never runs unsandboxed in a release build.

4. **Transfer** (ADR-0132 decision 3 rules, made general and stricter; exact steps in the spec).
   - It runs on one thread created for that install and ended after it, never on the render or audio thread. HTTPS on every hop; redirects only to hosts on the entry's host list; a connect and a stall timeout, so a blocked read cannot keep the thread alive; more bytes than the declared size abort.
   - The archive goes to `<root>/<id>/.staging/`, on the same volume as its target and emptied before each attempt. Its SHA-256 must match before it is opened; the hash and the unpack read the same open file. Free space for the archive plus the unpacked size is checked first.
   - Unpacking checks every entry by its name in the archive alone, never by following anything on disk, with the same platform-independent rules on every OS: no absolute or drive paths, no `..`, no backslash, no `:`, no Windows reserved or trailing-dot names, no two names equal after NFC and `str::to_lowercase`, no device files, setuid, setgid and sticky bits cleared, links created last, and every link target resolved through the archive's own links (depth at most 16) with each step inside the output, and caps on entries and unpacked bytes. The entry says which unpacked directory holds the library and which holds the resources, so upstream archives (`Release/`, `Resources/`) and a flat mirror archive both work. Because CEF loads `icudtl.dat`, `snapshot_blob.bin` and `v8_context_snapshot.bin` from the module directory on Windows and Linux, the installer moves those files from `resources_dir` into `library_dir` when they are not there already, and an install whose `library_dir` lacks the library or these files is `unsafe_archive`.
   - Files and the `.complete` marker are synced to disk before the rename to `<version>`. A directory without the marker is never used. Cancel empties staging and leaves an installed version untouched.
   - Install, clean and remove of one id hold an OS advisory lock on `<root>/<id>/.lock`, so two RBS processes never write the same add-on. After taking the lock, an install whose version is already complete returns at once. Remove deletes the marker first, so a partly deleted directory (Windows keeps loaded DLLs) is never used again. Install never deletes another version.
   - Versions are compared as dotted numbers (`156.6.0 < 156.10.0`); a version that does not parse that way is never compared and never removed by clean.
   - HTTP sits behind a `Fetch` trait, decompression behind an `Unpack` trait, and the OS file operations (lock, no-follow create, sync, directory sync, free space, rename with a bounded retry on Windows sharing violations) behind an `AddOnFs` trait, all implemented in the adapter. `rbs-add-ons` owns every rule; its tests use in-memory versions that can fail, redirect and stall, on an injected clock (ADR-0032). The adapter's own tests cover the timeouts and the OS operations.

5. **Storage.** Add-ons live in the local (not roaming) data directory: `%LOCALAPPDATA%\RBS\add-ons\<id>\<version>\`, `~/Library/Application Support/RBS/add-ons/<id>/<version>/`, `$XDG_DATA_HOME/rbs/add-ons/<id>/<version>/`. Nothing is written there until the user asks. Installing a version never removes another, so an older RBS build next to a newer one keeps working. `clean` removes versions lower than the highest version below the catalog's (keeping the current and the previous one) and never touches a higher one; the desktop Update command runs clean only after an install that succeeded. Data kinds, once built, install into their ADR-0051 libraries, not here.

6. **Resolving.** A consumer asks only `installed(id) -> Option<Installed { version, path }>`, true only for the catalog's own version with its marker present. Resolving is lazy: the app asks only when the open collection has a browser source, when the user picks Browser in Add Source, when the Add-ons section opens, and after each install, cancel or remove. There is no file watcher and no polling.

7. **Statuses.** Each entry is `not_installed`, `downloading {bytes, total}`, `verifying`, `installing`, `installed {version}`, `update_available {installed, available}` (only an older version is on disk; a newer one left by a newer RBS build counts as `not_installed` and is never touched) or `failed {reason}`, where reason is one of `network`, `host_not_allowed`, `too_large`, `hash_mismatch`, `unsafe_archive`, `no_space`, `io`. Cancel returns to the previous status. Progress is published at most four times a second.

8. **Browser engine wiring.**
   - **Shipping.** The runner executable `rbs-browser-cef` ships inside the app (on macOS with its helper bundles, inside the signed `RBS.app`); only the CEF runtime is downloaded (owner decision pending, see Review).
   - **Loading.** The studio passes the resolved runtime directory to the runner tree in the environment variable `RBS_CEF_RUNTIME` (an absolute path), which every CEF child process (renderer, GPU, utility, and the macOS helpers) inherits; the runner never finds CEF through the OS search order. On Windows and macOS every process type, the macOS helper included, loads the library from that path and checks its version (below) before the first CEF call (`api_hash`, `execute_process`). On Linux `libcef.so` is a load-time dependency, so the loader brings it in at exec through `LD_LIBRARY_PATH` (below); a missing or broken library ends the process before `main` (exit status 127), and the studio maps 127, and any exit before the runner's first IPC hello, to `browser_runtime_missing`. `RBS_CEF_RUNTIME` then gives only the resources and locales paths and the version check. Windows: `libcef.dll` is delay-loaded; the runner calls `SetDefaultDllDirectories` and `AddDllDirectory(runtime)` and loads it by absolute path before the first CEF call, so a missing or wrong DLL is an exit code, not a loader dialog, and the studio starts the runner with critical-error dialogs off. Linux: release builds drop `rpath=$ORIGIN`, and the studio sets `LD_LIBRARY_PATH` to the runtime for the runner tree only (the tree runs only itself; a page cannot start processes). macOS: the runner and its helpers load the framework with `cef_load_library` from the absolute path and set `framework_dir_path`, `main_bundle_path` and `browser_subprocess_path`; under the hardened runtime this needs a runtime signed with RBS's own identity (ADR-0132 decision 3) and no library-validation exception; development builds use an unsigned runtime and are not notarized. All platforms set `resources_dir_path` and `locales_dir_path` to the runtime.
   - **Version.** After loading, the runner reads the CEF version from the library and exits with `runtime_mismatch` when it differs from the one it was built against or from the marker. The runner exit codes `runtime_missing` and `runtime_mismatch` get numbers in `rbs-browser-ipc` next to today's codes. A failed load is the source reason `browser_runtime_missing`; a mismatch is the new reason `browser_runtime_mismatch`, and the add-on row then offers Reinstall.
   - **Which runner.** `RBS_BROWSER_HELPER` overrides (development); the test-only fallback `CARGO_BIN_EXE_rbs-browser-helper` stays for tests; otherwise `rbs-browser-cef` next to the app. The checker test double `rbs-browser-helper` next to the app is no longer picked up.
   - **No engine.** A browser source is Unavailable with ADR-0132's reason `browser_runtime_missing`, holds no slot and downloads nothing. Add Source always lists Browser when the type is registered; while the engine is missing, choosing it opens Settings at the Add-ons section with that row selected and adds no source. A collection that already has a browser source opens with the source showing the reason and a way to the Add-ons section; ADR-0132's single start-up notice points there.
   - **After an install.** The browser tree resolves the runtime at each start of a source, not once when the studio is built. When `org.rbs.browser-engine` becomes installed, the app tells the studio, and every source Unavailable with `browser_runtime_missing` is re-evaluated at once (Standby or Active by ADR-0107), with no restart.
   - Removing the engine is refused with `busy` while a browser tree runs.

9. **Settings › Add-ons.** One Settings section lists the catalog: name, one-line description, download size, status, progress while downloading, and the commands Download, Cancel, Update, Retry and Remove. The download size is shown before anything is fetched. The glossary term is "Add-on". Its look follows the UI rules and gets its own proposal and screenshots before it is built (CLAUDE.md rule 16).

10. **Control API** (next free protocol minor at merge; U20 parity). `addon.list` (`read`) returns every entry with its status and sizes; topic `addon.changed`. `addon.install {id}`, `addon.cancel {id}`, `addon.clean {id}` and `addon.remove {id}` use the existing `plugins` scope, which is already loopback and IPC only and is never given to remote tokens; its description becomes "Plugin and add-on management", and a remote call is refused with `scope_denied` as today. `install` is effect `edit`: it never removes or replaces a version with a marker, and deletes only its own staging and an unmarked, never-usable directory of the same version; `docs/specs/control-api.md`'s `edit` definition gains this case in the implementation PR (owner decision with this ADR) and needs the network permission (ADR-0052, `not_permitted` reason `network`); `cancel` is `edit`; `clean` and `remove` are `destructive`, since they delete versions another RBS build may use and getting them back needs the network. Data kinds, once built, keep their own ADR-0051 import methods and scopes. An unknown id is `not_found`; an entry with no artifact for this computer is `invalid_params` with `data.reason` `not_available`. These replace ADR-0132's `browser.runtime.*`, which are never built.

11. **Checks, without a network or CEF.** The spec's conformance list: every hash, size, redirect, timeout, archive-name, link, lock and cancel case leaves no installed add-on and never touches an installed version; an unknown kind in the built-in catalog fails a test; `installed` is `None` for any version but the catalog's; a missing engine makes the browser source `browser_runtime_missing` with no slot, and an install re-evaluates it without a restart; Browser in Add Source with no engine adds no source; `remove` while a tree runs is `busy`; `install`, `clean` and `remove` over a remote connection are `scope_denied`; every process type of the runner tree finds the runtime from `RBS_CEF_RUNTIME`; clean never removes the current, the previous or a higher version; `install` without the network permission is `not_permitted`/`network`. ADR-0132's pin check stays.

12. **What changes in ADR-0132.**

    | ADR-0132 clause | Now |
    | --- | --- |
    | 1, 2, 4, 5, 7, 8, 10, 11 | Stand (2: the pin file gains entry fields and holds one top-level `version` for all platforms instead of one per platform, decision 3) |
    | 3: runtime location `runtimes/cef/<version>/` | Replaced by decision 5 (`add-ons/org.rbs.browser-engine/<version>/`) |
    | 3: download started from the source's Properties or the notice, or `browser.runtime.install` | Replaced by decisions 8 to 10: started in Settings › Add-ons or with `addon.install`; Properties and the notice lead there |
    | 3: transfer, staging, marker and archive rules | Replaced by decision 4 |
    | 3: keep pinned and previous; "Remove browser support" | Replaced by decisions 5, 9 and 10 (install never removes; Update cleans; Remove on the add-on row) |
    | 3: RBS mirror, signed macOS runtime, zero cost until a runtime exists, `browser_runtime_missing` | Stand |
    | 6: Interact window "with the download offer" | The window shows the reason and leads to Settings › Add-ons |
    | 9: `browser.runtime.status`, `.install`, `.remove`, topic `browser.runtime.changed`, statuses `missing`/`ready` | Replaced by decisions 7 and 10 |
    | 12: the hash, size, redirect, escaping-entry, marker and missing-runtime checks | Replaced by decision 11 |
    | 12: the `css`, reload, Interact, profile, pin and `cef-dll-sys` checks | Stand |

## Consequences

- One tested download path serves the browser engine now and plugins and shared files later; a new kind is one installer.
- Browser is visible again, with a clear way to get it, and costs nothing until the user downloads it.
- New crate `rbs-add-ons` (catalog model, statuses, transfer, name and link rules, the `runtime` installer, the `Fetch`, `Unpack` and `AddOnFs` traits; no network or OS-specific code) in a new Add-ons layer next to User data, which may depend only on `rbs-core`, `serde`, `toml`, `sha2` and `unicode-normalization` (NFC for name clashes; case is compared with `str::to_lowercase`); no toolkit, engine or adapter crate. An adapter `rbs-add-ons-sys` implements the three traits (`ureq` as the HTTP/1.1 client on the existing `rustls`, trusting the operating system's roots through `rustls-platform-verifier`, with automatic redirects off so `rbs-add-ons` checks every hop; `bzip2` for upstream CEF archives, the existing `tar` and `flate2`, and OS file locks, `openat2` (on Linux before 5.6, a walk of `openat` with `O_NOFOLLOW` per component), sync and free space through `libc` and `windows`); `xtask` lets it depend on `rbs-add-ons` the way an adapter depends on its port. It is linked only by `rbs-app` and, later, `rbs-cli`; `rbs-mcp` reaches add-ons through the control API. Both join the layer table and the Adapters row in `overview.md` and the `xtask` classes, and the new dependencies are justified in that PR.
- Follow-ups: spec [add-ons](../specs/add-ons.md) (this change); control API spec rows and the `plugins` scope description at implementation; the user-data spec's reserved `runtimes/` path becomes `add-ons/`; the source reasons `browser_runtime_mismatch` and `browser_sandbox_unavailable` in the scene-sources spec; glossary "Add-on"; ADR-0132's status line notes the superseded parts once this is accepted; the Settings section proposal; the sandbox, the RBS mirror and the signed macOS runtime of ADR-0132.
- Harder: the runner now loads a runtime outside its own directory, which needs a delay-loaded DLL on Windows, a changed rpath on Linux and an external framework path for the macOS helpers. A short device check on each OS is part of the wiring PR; it confirms decision 8, it does not replace it.

## Alternatives considered

- **Browser-only download code, as ADR-0132 wrote it.** Simplest now, but the next downloadable thing would copy or refactor the safety code. Rejected by the owner (2026-11-18).
- **One "package" type with no kinds.** Every package unpacked to a directory. Data such as a profile would skip its validated import, and code and data would share one trust rule. Rejected.
- **Download the runner together with CEF as one archive.** Keeps runner and runtime in lockstep, but the runner would be a downloaded executable outside the app's signature and every runner fix would need a new mirror archive. Not chosen; see decision 8.
- **Find CEF through the OS search path (`PATH`, `rpath`).** Simple, but on Windows `PATH` comes last in the DLL search order and a missing DLL fails before `main` with a dialog. Rejected for decision 8's explicit loading.
- **A remote catalog now.** More flexible, but needs catalog signing, hosting and an update policy. The owner chose a built-in list for now.

## Review

Independent review per ADR-0014.

- Round 1, independent AI session, 2026-11-18, verdict: changes requested. Findings and resolution:
  1. Link handling in unpack open to check-then-use races. Resolved: all checks by archive name, links last, targets by name, no following on disk (decision 4, spec steps 6 and 7).
  2. Windows and case-insensitive name rules missing. Resolved: one platform-independent name check (decision 4, spec "Entry names").
  3. Install not safe across processes; durability. Resolved: advisory lock, marker removed first on remove, sync before rename, same volume, bounded rename retry, clean-up failure never fails the install (decision 4, spec).
  4. How the runner finds CEF underspecified. Resolved: absolute path argument, delay-load on Windows, rpath and `LD_LIBRARY_PATH` on Linux, framework path and helpers on macOS, resources and locales paths, version check (decision 8).
  5. Decompression placed in two ways. Resolved: `Unpack` trait, hash and unpack from one handle, entry and size caps (decision 4).
  6. Layering undefined. Resolved: Add-ons layer with its allow list, adapter class, which roots link it (Consequences).
  7. Cancel and stall. Resolved: connect and stall timeouts, HTTPS on every hop, stalling fake, injected clock (decision 4).
  8. Control API effects and scopes. Resolved: `remove` is `destructive`, local only, `install` limited to `admin` kinds, `plugin` gets its own methods (decision 10).
  9. Superseded clauses not exact. Resolved: table in decision 12.
  10. Pin file "unchanged" was wrong. Resolved: decision 3.
  11. No runtime wiring after install. Resolved: decision 8 "After an install" and "Which runner".
  12. Sandbox prerequisite. Resolved: release gate in decision 3.
  13. Resolve lazily. Resolved: decision 6.
  14. Untestable rules. Resolved: compiled-in unknown kind fails a test, `update_available` defined, `InstallError` listed in the spec.
- Round 2, independent AI session, 2026-11-18, verdict: changes requested. Findings and resolution:
  1. CEF child processes never get the runtime path. Resolved: inherited `RBS_CEF_RUNTIME`, every process type loads and checks before the first CEF call (decision 8).
  2. Release gate left users stuck. Resolved: release builds hide Browser and the notice until the sandbox lands (decision 3).
  3. Clean-up removed newer versions; no version order. Resolved: dotted-number order, clean keeps current, previous and higher (decisions 4, 5).
  4. `install` as `edit` deleted files. Resolved: install never deletes; new `addon.clean` is `destructive` (decisions 5, 10).
  5. Per-method remote rule bypassed the scope model. Resolved: the loopback-only `plugins` scope (decision 10).
  6. Link chains checked by name only. Resolved: resolution through the archive's own links, every step inside (decision 4, spec).
  7. Archive layout unspecified. Resolved: `library_dir` and `resources_dir` entry fields (decision 4, spec).
  8. Allow list incomplete; adapter class. Resolved: `AddOnFs` trait in the adapter, `unicode-normalization`, `xtask` rule, Adapters row (Consequences).
  9. Linux sandbox vs setuid stripping. Resolved: namespace sandbox only, `browser_sandbox_unavailable` (decision 3).
  10. Error mapping gaps. Resolved: spec maps every variant; runner exits map to source reasons (decision 8).
  11. Already-installed check too late. Resolved: right after the lock (decision 4).
  12. Timing rules not testable. Resolved: rename retry on the injected clock; adapter tests own the timeouts (decision 4).
  13. Table and follow-ups. Resolved: decision 12 row 1, Consequences, "Which runner".
  14. Setuid wording. Resolved: cleared, in both texts.
- Round 3, independent AI session, 2026-11-18, verdict: approve with changes; nothing blocking. Findings and resolution:
  1. Linux loads `libcef.so` before `main`. Resolved: `LD_LIBRARY_PATH` at exec, exit 127 and an exit before the first hello map to `browser_runtime_missing` (decision 8).
  2. ICU data and V8 snapshots load from the module directory. Resolved: the installer places them in `library_dir` and checks them (decision 4).
  3. `install` deletes an unmarked same-version directory. Resolved: decision 10 wording; `edit` definition extended in the implementation PR.
  4. Release gate missing from the API. Resolved: `addon.list` omits, `addon.install` is `not_found`; error kinds named (decisions 3, 10).
  5. HTTPS client and roots unnamed; old kernels. Resolved: `ureq`, `rustls-platform-verifier`, manual redirects, `openat` walk fallback (Consequences).
  6. Runner exit codes undefined; a test checked the test double. Resolved: codes in `rbs-browser-ipc`, the test checks the studio's mapping (decision 8, spec).
  7. Case folding undefined. Resolved: NFC then `str::to_lowercase`.
  8. Update's clean unconditional. Resolved: only after a successful install.
  9. Decision 12's last row too broad. Resolved: split into replaced and standing checks.
- Owner question open: runner shipped with the app (decision 8) or downloaded with the runtime.
