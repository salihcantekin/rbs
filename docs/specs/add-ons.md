# Spec: Add-ons

- Status: Draft
- Owner crate(s): `rbs-add-ons` (catalog, statuses, transfer, installers), `rbs-add-ons-http` (HTTPS and decompression adapter), `rbs-app` (built-in catalog, wiring, Settings › Add-ons)
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
```

```rust
pub struct AddOnId(String);           // validated: [a-z0-9.-], 3..=64 bytes, has a dot
pub enum Kind { Runtime }             // later: Plugin, data kinds
pub struct Entry { id, kind, name, summary, version, hosts, notices, platform: Option<Artifact> }
pub struct Artifact { url, size: u64, sha256: [u8; 32], unpacked_size: u64, format, strip: u8 }

pub fn parse_entry(toml: &str, target: &str) -> Result<Option<Entry>, CatalogError>;
```

Invariants:

- An entry whose `kind` is unknown parses to `Ok(None)` (skipped and logged), never an error that hides the other entries.
- `url` must be `https://` and its host must be in `hosts`; otherwise `CatalogError::Invalid`.
- An entry with no artifact for this target is listed with status `not_installed` and its Download command is unavailable, with the reason "Not available for this computer".

### Fetch and Unpack

```rust
pub trait Fetch: Send + Sync {
    /// Starts a GET. Every hop must be https and `allow(host)` must be true.
    /// Connect timeout 15 s; a read that receives no byte for 30 s is `Stalled`.
    fn get(&self, url: &Url, allow: &dyn Fn(&str) -> bool) -> Result<Box<dyn Read + Send>, FetchError>;
}
pub enum FetchError { Network(String), HostNotAllowed(String), NotHttps, Stalled }

pub trait Unpack: Send + Sync {
    /// Decompresses `format` and yields tar entries in archive order; it never writes to disk.
    fn entries<'a>(&self, format: Format, file: &'a mut File) -> Result<Box<dyn Iterator<Item = Result<ArchiveEntry<'a>, UnpackError>> + 'a>, UnpackError>;
}
pub struct ArchiveEntry<'a> { pub name: Vec<u8>, pub kind: EntryKind, pub mode: u32, pub link: Option<Vec<u8>>, pub data: Box<dyn Read + 'a> }
pub enum EntryKind { File, Dir, Symlink, HardLink, Other }
```

`rbs-add-ons-http` implements both. `rbs-add-ons` does every check in this spec; an `Unpack` implementation only decodes. Tests use an in-memory `Fetch` that serves bytes, redirects, failures and stalls, and an in-memory `Unpack`.

### Entry names

One check, the same on every OS, applied to each entry name and each link target after `strip`, on the archive's bytes alone. A name is refused (`unsafe_archive`) when it:

- is not valid UTF-8, is empty, or has more than 255 bytes in one component or 1024 in all;
- is absolute, starts with a drive (`C:`), a `\\?\` or `\\` prefix, or contains `\`;
- has a `.` or `..` component, or a component ending in `.` or a space;
- contains `:`, a control character or one of `*?"<>|`;
- has a component that is a Windows reserved name (`CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9`, `LPT1` to `LPT9`), in any case, with or without an extension;
- equals an earlier name after Unicode NFC and case folding (duplicates are refused too);
- has a parent component that an earlier entry created as a link.

### Transfer and install

`AddOns::install(id, fetch, unpack, progress, cancel, clock) -> Result<Installed, InstallError>` runs on a thread the caller made for that install. In order:

1. Take the advisory lock `<root>/<id>/.lock` (OS file lock, not a lock file's existence). If another process holds it, `busy`.
2. Empty and recreate `<root>/<id>/.staging/`. It is inside `<root>/<id>/`, so it is on the same volume as the target.
3. Check free space for `size + unpacked_size` (`no_space`).
4. Stream the body into `.staging/download`, hashing as it goes; more than `size` bytes is `too_large`; fewer, or a stall, is `network`.
5. Compare the SHA-256 (`hash_mismatch`). Keep the same file handle and seek to 0; the path is never reopened.
6. Read entries through `Unpack` into `.staging/out/`: check each name; files and directories are created with `O_NOFOLLOW`-style opens (Linux `openat2` with `RESOLVE_BENEATH` where available) and modes stripped of setuid, setgid and sticky bits; `Other` entries (devices, FIFOs) are refused; more than 20 000 entries or more bytes than `unpacked_size` is `unsafe_archive`.
7. Create links last. A symlink target, joined to the link's parent and normalised by name, must stay inside `out/`. A hard link may point only to a regular file this archive created earlier. Any violation is `unsafe_archive` and nothing is kept.
8. Sync every file, write `.staging/out/.complete` (version and SHA-256) and sync it, rename `out/` to `<root>/<id>/<version>/`, sync `<root>/<id>/`. On Windows a rename that fails with a sharing violation is retried up to 10 times, 200 ms apart (a scanner holds new DLLs), then `io`. If `<version>/` exists without a marker it is deleted first; with a marker the install is a no-op.
9. Remove versions other than this one and the one before it, then empty `.staging/`. A failure here is logged and does not fail the install.
10. Release the lock.

Cancel is checked between reads and between entries; it empties `.staging/` and returns `Cancelled`, leaving any installed version as it was.

```rust
pub enum InstallError {
    Busy, Cancelled, NoSpace, Network(String), HostNotAllowed(String), NotHttps,
    TooLarge, HashMismatch, UnsafeArchive(String), Io(String), NotAvailable,
}
```

Each maps to one `FailReason` (`Cancelled` maps to none: the status returns to what it was; `NotHttps` maps to `host_not_allowed`; `NotAvailable` means no artifact for this computer).

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

`UpdateAvailable` means only an older version with a marker is on disk. A newer version (left by a newer RBS build) is `NotInstalled` for this build and is never removed by it. Progress updates are published at most four times a second on the injected clock, plus once at each state change.

### Remove

`AddOns::remove(id)` takes the lock, deletes each version's `.complete` first, then the version directories and `.staging/`. A directory that cannot be fully deleted (Windows keeps loaded DLLs) stays without a marker and is never used; the next install or remove deletes it. The app refuses remove with `busy` while a consumer uses the add-on (for the browser engine: while any browser tree runs in this process); another process's use is covered by the marker rule.

### Browser engine consumer

- The runner `rbs-browser-cef` ships with the app (macOS: with its helper bundles inside the signed `RBS.app`). The studio passes the runtime directory as `--runtime <absolute path>`.
- Windows: `libcef.dll` is delay-loaded; before the first CEF call the runner calls `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS)`, `AddDllDirectory(runtime)` and `LoadLibraryExW(<runtime>\libcef.dll)`. The studio starts the runner with critical-error dialogs off. A failed load exits with `runtime_missing`.
- Linux: release builds of the runner have no `rpath=$ORIGIN`; the studio sets `LD_LIBRARY_PATH=<runtime>` for the runner child only.
- macOS: the runner and its helpers call `cef_load_library` with `<runtime>/Chromium Embedded Framework.framework/Chromium Embedded Framework` and set `framework_dir_path`, `main_bundle_path` and `browser_subprocess_path`. Public releases load only an RBS-signed runtime under the hardened runtime, with no library-validation exception.
- Every platform sets `resources_dir_path` and `locales_dir_path` to the runtime. After loading, the runner compares the library's CEF version with its own and with the marker and exits with `runtime_mismatch` on a difference.
- Runner choice: `RBS_BROWSER_HELPER` overrides; otherwise `rbs-browser-cef` next to the app. `rbs-browser-helper` (test double) runs only when named.
- No installed engine: browser sources are Unavailable with `browser_runtime_missing`, hold no slot, start no download. Add Source lists Browser whenever the type is registered; choosing it then opens Settings › Add-ons with `org.rbs.browser-engine` selected and adds no source.
- The browser tree resolves the runtime at each source start. When the engine becomes installed, the app tells the studio and every source Unavailable with `browser_runtime_missing` is re-evaluated at once, with no restart.
- Release builds do not list the browser engine until the runner runs sandboxed (ADR-0132).

### Control API

| Method | Scope | Effect | Notes |
| --- | --- | --- | --- |
| `addon.list` | `read` | `read` | Every entry: `id`, `kind`, `name`, `summary`, `version`, `size`, `unpacked_size`, `available` (artifact for this computer), `status` |
| `addon.install {id}` | `admin`, loopback and IPC only | `edit` | Network permission (ADR-0052, `not_permitted`/`network`). Only kinds allowed under `admin` (`runtime`). Answers at once; progress through `addon.changed`. `busy` while an install of that id runs |
| `addon.cancel {id}` | `admin`, loopback and IPC only | `edit` | No-op when nothing runs |
| `addon.remove {id}` | `admin`, loopback and IPC only | `destructive` | `busy` while a consumer uses it |

Topic `addon.changed {id, status}` (`read`). These replace ADR-0132's `browser.runtime.*`, which are never built. A future `plugin` kind gets its own methods under the `plugins` scope.

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
- A held lock makes a second install and a remove `busy`; remove deletes the marker before the files.
- A successful install keeps this version and the previous one and removes older ones; a failing clean-up still returns `Ok`.
- Progress is published at most four times a second on `TestClock`.
- Every entry of the built-in catalog parses with a known kind (an unknown kind fails this test); an unknown kind in a test entry is skipped and the others still parse.
- `rbs-app`: no engine makes browser sources `browser_runtime_missing` with no slot; an install re-evaluates them without a restart; Add Source Browser with no engine adds no source; `addon.remove` while a tree runs is `busy`; `addon.install` and `addon.remove` from a remote session are refused; `addon.install` without the network permission is `not_permitted`/`network`.

## Open questions

- Whether the runner ships with the app (this spec) or is downloaded with the runtime: owner to confirm (ADR-0145 decision 8).
