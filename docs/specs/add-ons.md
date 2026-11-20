# Spec: Add-ons

- Status: Draft
- Owner crate(s): `rbs-add-ons` (catalog, statuses, transfer, installers), `rbs-add-ons-sys` (HTTPS, decompression and file system adapter), `rbs-app` (built-in catalog, wiring, Settings › Add-ons)
- Related ADRs: [0145](../adr/0145-add-ons-one-place-to-download-and-install.md), [0132](../adr/0132-browser-engine-css-interaction.md), [0109](../adr/0109-browser-source.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md), [0052](../adr/0052-ai-agent-control-through-mcp.md), [0013](../adr/0013-zero-cost-when-unused.md)

## Purpose

Some things RBS can use are not shipped with it: today the CEF runtime of the browser source, later plugins and files users share. This spec defines how such an add-on is described, fetched, verified, installed, found and removed, so every kind uses one tested path.

## Scope

In scope: the catalog entry, the `Fetch` and `Unpack` traits, the transfer, name and unpack rules, locking, the `runtime` kind, statuses, storage layout, resolving, and the control API methods.

Out of scope, later changes: the `plugin` kind (signature, ADR-0006), data kinds (`output_profile`, `workspace`, `scene_collection`, `theme`) through ADR-0051 import, a local-file origin, a remote catalog (needs its own ADR), the RBS mirror and the signed macOS runtime (ADR-0132 decisions 2 and 3), and the look of Settings › Add-ons (its own UI proposal).

## Contract

### Catalog entry

The built-in catalog is a list of entries compiled into `rbs-app`. An entry for a code-bearing kind is a TOML document; the browser engine's entry is `crates/rbs-browser-cef/cef-pin.toml` (ADR-0132 decision 2, not written yet; ADR-0132's `xtask` check reads its `version`).

```toml
id = "org.rbs.browser-engine"     # reverse-DNS, stable, never reused
kind = "runtime"                  # closed set per RBS version
name = "Browser engine"           # shown in Settings, glossary-checked
summary = "Lets browser sources show web pages."
version = "156.6.0"               # the version consumers load
hosts = ["cdn-host.example"]      # every host a request or redirect may reach
notices = "https://..."           # license notices and sources

[platform.x86_64-pc-windows-msvc]
url = "https://..."
size = 123456789                  # exact archive bytes
sha256 = "..."                    # lowercase hex
unpacked_size = 260000000         # for the free-space check
format = "tar.bz2"                # tar.bz2 | tar.gz
strip = 1                         # leading path parts removed when unpacking
library_dir = "Release"           # holds libcef / the macOS framework, icudtl.dat and the V8 snapshots, after strip
resources_dir = "Resources"       # holds the .pak files and locales/ (may equal library_dir)
required = ["libcef.so", "icudtl.dat", "snapshot_blob.bin", "v8_context_snapshot.bin"]  # must end up in library_dir
```

```rust
pub struct AddOnId(String);           // validated: [a-z0-9.-], 3..=64 bytes, has a dot
pub enum Kind { Runtime }             // later: Plugin, data kinds
pub struct Entry { id, kind, name, summary, version, hosts, notices, platform: Option<Artifact> }
pub struct Artifact { url, size: u64, sha256: [u8; 32], unpacked_size: u64, format, strip: u8, library_dir: RelPath, resources_dir: RelPath, required: Vec<RelPath> }
pub struct Version(Vec<u32>);         // dotted numbers; 156.6.0 < 156.10.0; anything else does not parse

pub fn parse_entry(text: &str, target: &str) -> Result<Option<Entry>, CatalogError>;
```

Invariants:

- An entry whose `kind` is unknown parses to `Ok(None)` (skipped and logged), never an error that hides the other entries.
- `url` must be `https://` and its host must be in `hosts`; `version` must parse as a `Version`; `library_dir` and `resources_dir` must pass the entry-name rules; otherwise `CatalogError::Invalid`.
- An entry with no artifact for this target is listed with status `not_installed` and its Download command is unavailable, with the reason "Not available for this computer".

### Fetch and Unpack

```rust
pub enum Response { Redirect(String), Body(Box<dyn Read + Send>) }
pub trait Fetch: Send + Sync {
    /// One GET. Never follows a redirect: it returns it, and `rbs-add-ons` checks
    /// `https` and the host list before the next hop (at most 5 hops).
    /// Connect timeout 15 s; a body read that receives no byte for 30 s fails with
    /// an io error whose kind is `TimedOut` or whose inner error is `FetchError::Stalled`.
    fn get(&self, url: &str) -> Result<Response, FetchError>;
}
pub enum FetchError { Network(String), Stalled }

pub trait Unpack: Send + Sync {
    /// Decodes `format` and calls `visit` for each entry in archive order; it never
    /// writes to disk and checks nothing. It stops when `visit` breaks.
    fn unpack(&self, format: Format, input: &mut dyn Read,
              visit: &mut dyn FnMut(ArchiveEntry<'_>) -> ControlFlow<()>) -> Result<(), UnpackError>;
}
pub struct ArchiveEntry<'a> { pub name: Vec<u8>, pub kind: EntryKind, pub mode: u32, pub link: Option<Vec<u8>>, pub data: &'a mut dyn Read }
pub enum EntryKind { File, Dir, Symlink, HardLink, Other }
```

```rust
pub trait AddOnFs: Send + Sync {
    fn lock(&self, path: &Path) -> Result<Box<dyn LockGuard>, FsError>;          // OS advisory lock; Busy if held
    fn create_file_beneath(&self, root: &Path, rel: &RelPath, mode: u32) -> Result<File, FsError>; // never follows links
    fn create_dir_beneath(&self, root: &Path, rel: &RelPath) -> Result<(), FsError>;
    fn symlink_beneath(&self, root: &Path, rel: &RelPath, target: &str) -> Result<(), FsError>;
    fn hard_link_beneath(&self, root: &Path, rel: &RelPath, existing: &RelPath) -> Result<(), FsError>;
    fn open_read(&self, path: &Path) -> Result<File, FsError>;
    fn sync_file(&self, file: &File) -> Result<(), FsError>;
    fn sync_dir(&self, dir: &Path) -> Result<(), FsError>;
    fn free_space(&self, dir: &Path) -> Result<u64, FsError>;
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError>;              // SharingViolation is its own error
    fn remove_file(&self, path: &Path) -> Result<(), FsError>;                   // NotFound when absent
    fn remove_all(&self, dir: &Path) -> Result<(), FsError>;                     // never follows links; Ok when absent
    fn list_dir(&self, dir: &Path) -> Result<Vec<String>, FsError>;               // empty when absent
    fn exists(&self, path: &Path) -> bool;                                       // does not follow a final link
    fn pause(&self, duration: Duration);                                         // the rename retry; tests advance a TestClock
}
pub enum FsError { Busy, SharingViolation, NotFound, Link, Io(String) }
```

`rbs-add-ons-sys` implements all three traits: `Fetch` with `ureq` (HTTP/1.1, automatic redirects off) on `rustls` with `rustls-platform-verifier` (the OS trust roots); `*_beneath` on Linux and macOS with a walk of `openat` with `O_NOFOLLOW | O_DIRECTORY` per component from `root` (`openat2` with `RESOLVE_BENEATH` is an allowed alternative on Linux 5.6 and later), `FILE_FLAG_OPEN_REPARSE_POINT` checks on Windows. `rbs-add-ons` holds every rule in this spec; the implementations only decode, transfer and perform. `rbs-add-ons` tests use in-memory versions: a `Fetch` that serves bytes, redirects, failures and stalls, an `Unpack` over listed entries, and an `AddOnFs` over a temporary directory that can hold a lock for "another process", fail a rename with a sharing violation or report little space. `rbs-add-ons-sys` tests cover the timeouts against a local test server and the OS operations in a temporary directory.

### Entry names

One check, the same on every OS, applied to each entry name and each link target after `strip`, on the archive's bytes alone. A name is refused (`unsafe_archive`) when it:

- is not valid UTF-8, is empty, or has more than 255 bytes in one component or 1024 in all;
- is absolute, starts with a drive (`C:`), a `\\?\` or `\\` prefix, or contains `\`;
- has a `.` or `..` component, or a component ending in `.` or a space;
- contains `:`, a control character or one of `*?"<>|`;
- has a component that is a Windows reserved name (`CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9`, `LPT1` to `LPT9`), in any case, with or without an extension;
- equals an earlier name after Unicode NFC followed by `str::to_lowercase` (exact duplicates are refused too);
- has a parent component that an earlier entry created as a link.

`library_dir` and `resources_dir` must name directories the archive created. After unpacking, each name in `required` that is not in `library_dir` but is in `resources_dir` is moved into `library_dir`; one in neither is `unsafe_archive`. The browser engine's entry lists the library, `icudtl.dat` and the V8 snapshots there on Windows and Linux, because CEF loads them from the module directory.

### Transfer and install

`AddOns::install(id, fetch, unpack, progress, cancel, clock) -> Result<Installed, InstallError>` runs on a thread the caller made for that install. In order:

1. Take the advisory lock `<root>/<id>/.lock` (OS file lock, not a lock file's existence). If another process holds it, `busy`. If `<root>/<id>/<version>/.complete` already names this version, release the lock and return `Ok` without fetching.
2. Empty and recreate `<root>/<id>/.staging/`. It is inside `<root>/<id>/`, so it is on the same volume as the target.
3. Check free space for `size + unpacked_size` (`no_space`).
4. Stream the body into `.staging/download`, hashing as it goes; more than `size` bytes is `too_large`; fewer, or a stall, is `network`.
5. Compare the SHA-256 (`hash_mismatch`). Keep the same file handle and seek to 0; the path is never reopened.
6. Read entries through `Unpack` into `.staging/out/`: check each name; files and directories are created with `AddOnFs::*_beneath` and modes with setuid, setgid and sticky bits cleared; `Other` entries (devices, FIFOs) are refused; more than 20 000 entries or more bytes than `unpacked_size` is `unsafe_archive`.
7. Create links last. A symlink target must be relative. It is resolved by name through the archive's own entries: join it to the link's parent, then walk it component by component; when a component is another link of this archive, continue through that link's target (at most 16 links in all, else `unsafe_archive`). Every intermediate path must stay inside `out/`, and the final path must name an entry of the archive. A hard link may point only to a regular file this archive created earlier. Any violation is `unsafe_archive` and nothing is kept. (The macOS framework's `Versions/Current` links pass this rule.)
8. Sync every file, write `.staging/out/.complete` (version, SHA-256, `library_dir`, `resources_dir`) and sync it, rename `out/` to `<root>/<id>/<version>/`, sync `<root>/<id>/`. A rename that fails with a sharing violation is retried up to 10 times, 200 ms apart on the injected clock (a scanner holds new DLLs), then `io`. If `<version>/` exists without a marker (an unfinished, never-usable directory of the same version) it is deleted first.
9. Empty `.staging/` and release the lock. Install never removes or replaces a version with a marker.

Cancel is checked between reads and between entries; it empties `.staging/` and returns `Cancelled`, leaving any installed version as it was.

```rust
pub enum InstallError {
    Busy, Cancelled, NoSpace, Network(String), Stalled, HostNotAllowed(String), NotHttps,
    TooLarge, HashMismatch, UnsafeArchive(String), Io(String), NotAvailable,
}
```

| Error | Status afterwards |
| --- | --- |
| `Busy`, `NotAvailable` | Unchanged (refused before anything starts; the caller gets the error) |
| `Cancelled` | The status before the install |
| `Network`, `Stalled` | `failed {network}` |
| `HostNotAllowed`, `NotHttps` | `failed {host_not_allowed}` |
| `TooLarge` | `failed {too_large}` |
| `HashMismatch` | `failed {hash_mismatch}` |
| `UnsafeArchive` | `failed {unsafe_archive}` |
| `NoSpace` | `failed {no_space}` |
| `Io` | `failed {io}` |

### Clean

`AddOns::clean(id)` takes the lock and removes every version lower than the highest version below the catalog's (it keeps the catalog's version, the one before it, every higher version and every version that does not parse), marker first as in Remove. The desktop Update command runs clean only after an install that succeeded.

### Storage

`<root>` is `%LOCALAPPDATA%\RBS\add-ons`, `~/Library/Application Support/RBS/add-ons` or `$XDG_DATA_HOME/rbs/add-ons` (default `~/.local/share/rbs/add-ons`). The composition root passes it in; `rbs-add-ons` never computes OS paths.

### Resolving

```rust
pub struct Installed { pub version: String, pub path: PathBuf }
impl AddOns { pub fn installed(&self, id: &AddOnId) -> Option<Installed>; }
```

`Some` only when `<root>/<id>/<catalog version>/.complete` exists and names that version. The app calls it lazily: when the open collection has a browser source, when Browser is picked in Add Source, when Settings › Add-ons opens, and after each install, cancel or remove. There is no watcher and no polling.

### Status

```rust
pub enum Status {
    NotInstalled,
    Downloading { bytes: u64, total: u64 },
    Verifying,
    Installing,
    Installed { version: String },
    UpdateAvailable { installed: String, available: String },
    Failed { reason: FailReason },
}
pub enum FailReason { Network, HostNotAllowed, TooLarge, HashMismatch, UnsafeArchive, NoSpace, Io }
```

`UpdateAvailable` means only a lower version with a marker is on disk. A newer version (left by a newer RBS build) is `NotInstalled` for this build and is never removed by it. Progress updates are published at most four times a second on the injected clock, plus once at each state change.

### Remove

`AddOns::remove(id)` takes the lock, deletes each version's `.complete` first, then the version directories and `.staging/`. A directory that cannot be fully deleted (Windows keeps loaded DLLs) stays without a marker and is never used; the next install or remove deletes it. The app refuses remove with `busy` while a consumer uses the add-on (for the browser engine: while any browser tree runs in this process); another process's use is covered by the marker rule.

### Browser engine consumer

- The runner `rbs-browser-cef` ships with the app (macOS: with its helper bundles inside the signed `RBS.app`). The studio sets `RBS_CEF_RUNTIME=<absolute runtime path>` in the runner's environment; every CEF child process inherits it. Every process type (browser, renderer, GPU, utility, and the macOS helper binary) reads it, loads the library and checks the version before `api_hash` or `execute_process` (on Linux the loader has already loaded it, see below). `<lib>` below is `<runtime>/<library_dir>` and `<res>` is `<runtime>/<resources_dir>`, read from the marker.
- Windows: `libcef.dll` is delay-loaded; before the first CEF call each process calls `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS)`, `AddDllDirectory(<lib>)` and `LoadLibraryExW(<lib>\libcef.dll)`. The studio starts the runner with critical-error dialogs off. A failed load exits with `runtime_missing`.
- Linux: `libcef.so` is a load-time dependency, so the loader brings it in at exec. Release builds of the runner have no `rpath=$ORIGIN`; the studio sets `LD_LIBRARY_PATH=<lib>` for the runner child only. A missing or broken library ends the process before `main` with exit status 127; the studio maps 127, and any exit before the runner's first IPC hello, to `browser_runtime_missing`. `RBS_CEF_RUNTIME` gives the resources and locales paths and the version check.
- macOS: each process calls `cef_load_library` with `<lib>/Chromium Embedded Framework.framework/Chromium Embedded Framework`; the browser process sets `framework_dir_path`. `main_bundle_path` and `browser_subprocess_path` keep CEF's defaults: the runner's own bundle still holds its helper apps, so only the framework moves out. Public releases load only an RBS-signed runtime under the hardened runtime, with no library-validation exception.
- Every platform sets `resources_dir_path` to `<res>` and `locales_dir_path` to `<res>/locales`. After loading, the runner compares the library's CEF version (`cef_version_info` entries 0 to 2) with its own and with the marker and exits with `runtime_mismatch` on a difference; the marker is compared first, before anything is loaded. A debug build of the runner started without `RBS_CEF_RUNTIME` uses the engine `cef-dll-sys` copied next to it (developer runs); a release build exits with `runtime_missing`.
- The exit codes are numbered in `rbs-browser-ipc` `exit`: the runner's own 2 to 6, `RUNTIME_MISSING` 7 and `RUNTIME_MISMATCH` 8 (`exit::cause`). Runner exits map to source reasons: `RUNTIME_MISSING`, 127 and an exit before the first hello (the first heartbeat) with a code the runner does not use itself (a loader failure, such as a Windows DLL-not-found crash) to `browser_runtime_missing`, `RUNTIME_MISMATCH` to `browser_runtime_mismatch` (the add-on row then offers Reinstall); any other exit, a kill by the studio or a lost heartbeat is `helper_exit`. The studio checks the installed engine before it spawns (`BrowserTree::unavailable`), so a missing engine never starts a process; a runner that exits for its engine after spawn shows the reason, and the source starts again on the next install or Reload.
- Runner choice (`rbs-app` `browser_tree::resolve_runner`): `RBS_BROWSER_HELPER` overrides; the test-only `CARGO_BIN_EXE_rbs-browser-helper` fallback stays; otherwise `rbs-browser-cef` next to the app (on macOS also `rbs-browser-cef.app/Contents/MacOS/rbs-browser-cef`). `rbs-browser-helper` (test double) next to the app is no longer picked up. Only the shipped runner needs the add-on; an override or the test double gets `RBS_CEF_RUNTIME` when the engine is installed and starts without it otherwise. No runner at all: browser sources are Unavailable with `helper_missing`.
- No installed engine: browser sources are Unavailable with `browser_runtime_missing`, hold no slot, start no download. Add Source lists Browser whenever the type is registered; choosing it then opens Settings › Add-ons with `org.rbs.browser-engine` selected and adds no source.
- The browser tree resolves the runtime at each source start from a slot the host fills (`browser_tree::EngineSlot`): at studio start when the collection has a browser source, when Add Source › Browser is chosen, and after each install or remove. When the engine becomes installed, the app tells the studio (`Studio::browser_engine_changed`), which grants slots again and prepares every browser source Unavailable with an engine reason at once, with no restart. A missing engine fails `prepare` as `NodeError::Permanent`, so it never backs off. `addon.remove` is `busy` while any runner the tree started is alive (`RunnerBrowserTree::in_use`).
- The source's words: "The browser engine is not installed. Download it in Settings › Add-ons." and, for a mismatch, "The browser engine does not match this version of RBS. Install it again in Settings › Add-ons." A missing runner (`helper_missing`) says "The browser runner was not found next to the app." Any other stop says "The browser page stopped. Press Reload to try again." (`rbs-record` `Studio::sources`). A button in Properties that opens the Add-ons section is a later UI change (rule 16).

### Control API

| Method | Scope | Effect | Notes |
| --- | --- | --- | --- |
| `addon.list` | `read` | `read` | Every entry: `id`, `kind`, `name`, `summary`, `version`, `size`, `unpacked_size`, `available` (artifact for this computer), `status` |
| `addon.install {id}` | `plugins` | `edit` | Network permission (ADR-0052, `not_permitted`/`network`). Never removes a version. Answers at once; progress through `addon.changed`. `busy` while an install of that id runs |
| `addon.cancel {id}` | `plugins` | `edit` | No-op when nothing runs |
| `addon.clean {id}` | `plugins` | `destructive` | See Clean. `busy` while a consumer uses a version it would remove |
| `addon.remove {id}` | `plugins` | `destructive` | `busy` while a consumer uses it |

An unknown id is `not_found`; an entry with no artifact for this computer is `invalid_params` with `data.reason` `not_available`. `install`'s `edit` effect: it never removes or replaces a version with a marker; the `edit` definition in the control API spec gains this case at implementation. The `plugins` scope (loopback and IPC only, never on remote tokens) is described as "Plugin and add-on management"; a remote call is `scope_denied`. Topic `addon.changed {id, status}` (`read`). These replace ADR-0132's `browser.runtime.*`, which are never built.

## Errors and failure behaviour

Every failure is a typed `InstallError` mapped to a `FailReason`; nothing panics. A failed or cancelled install leaves staging empty and any installed version untouched. A failed install is shown on its row with the reason and a Retry command.

## Performance requirements

No thread, timer, watcher or file read exists for add-ons until a consumer resolves one or the user opens the section (P13). The install thread ends when the install ends; the stall timeout bounds it. Nothing runs on the render or audio thread.

## Conformance tests

All in `rbs-add-ons` with the in-memory `Fetch` and `Unpack`, no network or CEF:

- Each of `hash_mismatch`, `too_large`, a short body, a stall, a redirect to a host outside `hosts`, an `http://` hop, and a cancel mid-transfer and mid-unpack leaves no installed version and an existing version untouched.
- Each name rule of "Entry names" (absolute, drive, `\\?\`, backslash, `..`, trailing dot or space, `:`, reserved name with and without extension, control character, case-fold and NFC duplicate, exact duplicate, a component under an earlier link) is `unsafe_archive`.
- An escaping symlink, a chain of links that escapes, a write through an earlier link, a hard link to a file not in the archive, a device entry, too many entries and too many bytes are `unsafe_archive`; setuid bits are cleared.
- A directory without `.complete` is never `installed`; one with a marker for another version is not `installed` for this catalog; a newer version is left alone and the status is `not_installed`.
- A held lock makes a second install, a clean and a remove `busy`; an install of an already complete version fetches nothing; remove and clean delete the marker before the files.
- Install never removes a version; clean keeps the catalog's version, the previous one, higher ones and unparsable ones, and removes the rest; `Version` orders `156.6.0 < 156.10.0`.
- A link chain that resolves inside passes (a framework-style `Versions/Current` layout); a chain of 17 links fails; `library_dir` or `resources_dir` naming no created directory fails; ICU data and snapshots only in `resources_dir` are moved to `library_dir`; a `library_dir` without them fails; names differing only in case (`A.pak`, `a.pak`) or only in NFC form clash.
- A rename that fails with a sharing violation is retried on `TestClock` and then `io`.
- Progress is published at most four times a second on `TestClock`.
- `rbs-add-ons-sys` (adapter tests, local server and temporary directory): connect and stall timeouts, a redirect hop to `http`, `*_beneath` refusing an existing link, the lock between two handles.
- Every entry of the built-in catalog parses with a known kind (an unknown kind fails this test); an unknown kind in a test entry is skipped and the others still parse.
- `rbs-app`: no engine makes browser sources `browser_runtime_missing` with no slot; an install re-evaluates them without a restart; Add Source Browser with no engine adds no source; `addon.remove` while a tree runs is `busy`; `addon.install`, `addon.clean` and `addon.remove` from a remote session are `scope_denied`; the studio maps each runner exit code, 127 and an exit before the first hello to the right source reason (tests: `rbs-record` `a_browser_source_waits_for_its_engine_and_starts_after_the_install`, `rbs-app` `add_source_browser_leads_to_the_engine_until_it_is_installed`, `rbs-sources` `no_engine_makes_the_source_runtime_missing_with_no_slot_until_it_is_installed` and `a_runner_exit_maps_to_its_engine_reason`, `rbs-browser-ipc` `runner_exits_map_to_engine_causes`, `rbs-browser-runner` `a_runner_without_its_engine_exits_as_runtime_missing` and the mismatch case; real-runner loading: `rbs-browser-runner` `tests/real_cef.rs`, ignored, run with `RBS_TEST_CEF_RUNNER` and `RBS_TEST_CEF_RUNTIME` on each platform); `addon.install` without the network permission is `not_permitted`/`network`.

## Open questions

None. The runner ships with the app (owner, 2026-11-18).
