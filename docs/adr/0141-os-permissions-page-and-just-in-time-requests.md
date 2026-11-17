# ADR-0141: OS permissions: ask when used, show them in Settings, explain a blocked source

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner)
- Owner direction: 2026-11-17, project thread: a permission check that runs at first start and can be run again from Settings, works on every OS, and shows which permissions RBS has. Trigger: a development build started from another worktree had no camera access and the camera source showed nothing, with no way to ask for access.
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0077](0077-source-health-retarget-and-settings-window.md), [ADR-0113](0113-experience-level-and-first-run.md)
- Amends: [capture-port](../specs/capture-port.md) Media Foundation notes (`permission` reads the privacy consent instead of always `Granted`); [audio-io-port](../specs/audio-io-port.md) WASAPI notes (the same for microphones); ADR-0113 decision 5 (the first-run page gets an "Access" step)
- Spec: [capture-port](../specs/capture-port.md), [audio-io-port](../specs/audio-io-port.md), [scene-sources](../specs/scene-sources.md), [ui-design-system](../specs/ui-design-system.md) (with the UI proposal)

## Context

The ports already model OS permissions. `CaptureFactory` and `AudioBackend` have `permission(kind)` (fast, never prompts) and `request_permission(kind)` (shows the OS prompt, blocks, control thread only). The macOS adapters implement both: ScreenCaptureKit (`CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`), AVFoundation cameras and CoreAudio microphones (`AVAuthorizationStatus`, `requestAccessForMediaType`). `open` and `start` refuse with `PermissionNotDetermined` or `PermissionDenied` and never prompt, by design (capture-port.md). The nodes turn both into `SourceFault::PermissionDenied`, and the source row says "Camera access denied" (ADR-0077).

What is missing is the caller. Nothing in `rbs-app` calls `request_permission`. On macOS a camera whose permission was never asked therefore stays `NotDetermined` forever: the OS prompt never appears, the source shows "Camera access denied", and the only fix is to know the right pane in System Settings, where an app that never asked is not even listed. That is what the owner hit.

The way each OS keys a grant makes this frequent during development and confusing in general:

- **macOS (TCC).** A grant belongs to the *responsible* app. A bare executable started from a terminal (how `cargo run` and most dev runs start RBS) is attributed to the terminal, or to whichever app launched it (Terminal, iTerm, VS Code, the Claude app). A bundled `RBS.app` (`crates/rbs-app/macos/bundle.sh`) is keyed by its bundle id and code signature; the bundle is signed ad hoc by the linker, so the grant is tied to that exact build's code hash and a rebuild may lose it. Two worktrees, or the same build started two ways, are different apps to TCC. Camera and microphone grants apply at once; a screen recording grant applies only after the app restarts. There is no public query for the system audio recording permission (process taps, macOS 14.4+).
- **Windows.** Camera and microphone have per-user privacy switches ("Camera access", "Let desktop apps access your camera", and the same for the microphone), stored under `HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{webcam,microphone}` (`Value` = `Allow` / `Deny`, the desktop-app switch in the `NonPackaged` subkey) and overridable by policy under `HKLM`. Desktop apps get no prompt; a blocked device fails with `E_ACCESSDENIED`. Today the Media Foundation and WASAPI adapters return `Granted` from `permission` and only learn the truth on `open` (capture-port.md). Screen and window capture (WGC) needs no permission for desktop apps.
- **Linux.** Screens and windows are chosen in the desktop portal's dialog for each pick (with a restore token afterwards); cameras through PipeWire need no permission outside a sandbox. A Flatpak build would go through the camera portal, which RBS does not ship yet.

Forces: P13 (nothing runs while unused: no polling, no thread); P3 (no branching on vendor or OS names in engine code; the composition root may know the OS); the user must be able to see, fix and understand permissions without knowing where each OS hides them; Apple's guidance is to ask at the moment of use, not in bulk at start.

## Decision

Terms (glossary, with the UI proposal): the page is **Permissions**; a row's status is **Allowed**, **Not asked yet**, **Blocked**, **Managed by your organization**, **Asked each time** or **No permission needed**; the actions are **Allow** and **Open system settings**.

1. **Four permission kinds, one status.** The app talks about *Camera*, *Microphone*, *Screen and windows* and *System and app sound*. `rbs-sources` gains a toolkit-free `permissions` module that maps them to port kinds (`TargetKind::Camera`; `EndpointKind::Input`; `TargetKind::Display` and `Window`; `EndpointKind::OutputLoopback` and `Application`) and folds the answers of every installed adapter for that kind into one `PermissionStatus`, in this order of precedence:
   - `Managed`: some adapter says `Restricted` (decision 2);
   - `Blocked`: some adapter says `Denied`;
   - `NotAsked`: some adapter says `NotDetermined`;
   - `Unknown`: some adapter says `Unknown`;
   - `AskedEachTime`: every adapter serving the kind has the per-choice consent capability (decision 3) and none says otherwise;
   - `Allowed`: every adapter says `Granted` and at least one has the permission capability;
   - `NotNeeded`: no adapter serving the kind has a permission capability;
   - `Unavailable`: no installed adapter serves the kind (the row is not shown).
   `SourceRegistry::permission_status(kind)` reads it (calls `permission` only: fast, never prompts) and `SourceRegistry::request_permission(kind)` calls `request_permission` on the adapters that say `NotDetermined`. Nothing is cached, polled or started. The fold is pure and gets a property test (`permission_fold_follows_precedence`) over every combination of answers and capabilities.

2. **Port changes (minor, both ports).** `Permission` (capture and audio-io, both `#[non_exhaustive]`) gains `Restricted` (the OS or an administrator forbids it and the user cannot change it: macOS `AVAuthorizationStatusRestricted`, a Windows policy that forces deny) and `Unknown` (the OS has a permission but no way to read it before use: macOS system audio taps). `request_permission` may run on any thread that is neither the UI thread nor a real-time thread (it was "control thread only"). Both specs and their conformance suites change with it; existing adapters keep compiling and keep their answers, except as decisions 5 and 6 say and the `audio.input.permission` value on WASAPI (decision 3).

3. **Capabilities, not OS names (P3).** Per kind, an adapter declares `Flag` capabilities:
   - `capture.permission.<camera|display|window>` / `audio.permission.<input|loopback|application>`: the OS has a user permission for this kind. The existing `audio.input.permission` becomes a deprecated alias of `audio.permission.input`: its value turns true on WASAPI (it was "macOS microphone privacy" only), PipeWire and the test adapter keep false, and the audio-io-port.md capability table changes with it;
   - `capture.permission.<kind>.restart`: a grant takes effect only after the app restarts (macOS screen recording);
   - `capture.pick.consent`: the OS asks the user on every pick, in its own dialog (the desktop portal);
   - `capture.permission.<kind>.prompt` / `audio.permission.<kind>.prompt`: `request_permission` can show a prompt in this process now (decision 6).
   These decide `NotNeeded`, `AskedEachTime`, the restart line and whether **Allow** is offered.

4. **Ask at the moment of use.** When the user adds a source, or picks a target for one, whose kind is `NotAsked` and can prompt, the app calls `request_permission` for that kind before `open`, so the OS prompt appears right then. The call blocks until the user answers, so it runs on a one-shot worker thread that posts the answer back to the UI thread. At most one request per kind is in flight; a second caller waits for the same answer. Before the request, the app makes itself the active app so the prompt is in front. On quit, a waiting worker is detached and the process exits without waiting for it. The source row shows "Waiting for permission" meanwhile. Granted: the source opens. Refused: the source is added and shows its fault with the fix action (decision 8). A source loaded from a saved scene does not prompt at start-up; it shows the fault and the action.

5. **macOS adapters.** AVFoundation and CoreAudio map `Restricted` to `Restricted`. CoreAudio says `Unknown` for loopback and application kinds and declares their permission capability. ScreenCaptureKit cannot tell "not asked" from "denied" (`CGPreflightScreenCaptureAccess` is a bool), and `CGRequestScreenCaptureAccess` shows its alert only once per app: after one request in this process returns false, the adapter says `Denied` for the rest of the process; before that it says `NotDetermined`. So the first **Allow** shows the alert (or, if it was refused in an earlier run, does nothing and turns the row to Blocked with **Open system settings**). A prompt can end the process when the responsible app's `Info.plist` lacks the usage key, so the adapters declare the `.prompt` capability only when this process is the app TCC asks about (no other `.app` above its bundle in the ancestor walk of decision 10; a pure function over an injected ancestor list, tested) and the main bundle's info dictionary has `NSCameraUsageDescription` / `NSMicrophoneUsageDescription` / `NSScreenCaptureUsageDescription` / `NSAudioCaptureUsageDescription`; `request_permission` without it returns the current status and does not prompt. `crates/rbs-app/macos/Info.plist` gains `NSAudioCaptureUsageDescription`.

6. **Windows adapters read the consent store.** `rbs-capture-mf` (`webcam`) and `rbs-audio-io-wasapi` (`microphone`) implement `permission` from three registry reads, no prompt: the policy `HKLM\SOFTWARE\Policies\Microsoft\Windows\AppPrivacy` (`LetAppsAccessCamera` / `LetAppsAccessMicrophone` = 2 is `Restricted`), the device-wide switch `HKLM\...\CapabilityAccessManager\ConsentStore\<kind>` and the user switches `HKCU\...\ConsentStore\<kind>` and its `NonPackaged` subkey (`Deny` at any of them is `Denied`); otherwise `Granted`. The layout is undocumented, so a missing or unreadable value reads as `Granted` and `E_ACCESSDENIED` on `open` stays `PermissionDenied`, as today. The parse is a pure function over the read values, tested with fixtures. They declare the permission capability and no `.prompt` (Windows has no prompt for desktop apps). This changes `permission` from always `Granted` to the real value: a minor, behavioural change stated in both specs.

7. **Settings › Permissions.** A new section in the App group, after Shortcuts, shown at every experience level. One row per permission kind that is not `Unavailable`, each with its status in words and at most one action: `NotAsked` with the prompt capability **Allow**; `NotAsked` without it, `Blocked` and `Unknown` **Open system settings**; `Managed`, `Allowed`, `AskedEachTime` and `NotNeeded` none. `NotAsked` without the prompt capability says "Start RBS from RBS.app to be asked" (decision 10). The page reads the statuses when it is shown and when the Settings window gets focus back (the user returns from System Settings), and has a **Check again** button; there is no timer. A kind with the restart capability whose row is `Blocked` or `NotAsked` when the Settings window gets focus back adds "If you just allowed it, restart RBS" with **Restart RBS** (the running process keeps reading the old answer until it restarts, so nothing needs to be remembered). The page also says which app the system gives the permissions to (decision 10).

8. **A blocked source says how to fix it.** A source whose fault is `PermissionDenied` keeps its ADR-0077 text ("Camera access denied") and gets one action in its unavailable row and in Properties: **Allow** when its kind is `NotAsked` and can prompt, otherwise **Fix permission**, which opens Settings › Permissions at that kind's row. When a request or a return to the page turns a kind `Allowed`, sources of that kind with a `PermissionDenied` fault are reopened once, except for kinds with the restart capability. This is a user action, so the spec's "permission errors are never retried automatically" still holds for background retries.

9. **Opening the OS pane is the composition root's job.** `rbs-app` gets a small `privacy` module next to `reveal.rs` that opens the OS pane for a kind with the OS's own opener, nothing linked: macOS `x-apple.systempreferences:com.apple.preference.security?Privacy_Camera` (`Privacy_Microphone`, `Privacy_ScreenCapture`, `Privacy_AudioCapture`), Windows `ms-settings:privacy-webcam` / `ms-settings:privacy-microphone`, Linux none (no button). If the opener fails, the row says where to go in words.

10. **Who holds the grant.** `rbs-app` describes the app identity the OS uses, read once when the page is first shown. It is text only.
    - macOS with a bundle (`NSBundle.mainBundle.bundleIdentifier` set): the bundle id and this copy's path; for an ad hoc signed bundle it adds "This build is signed for this computer only, so macOS may ask again after a rebuild."
    - macOS without a bundle: "RBS runs as a plain program, so macOS gives permissions to the app that started it, likely `<name>`." The name comes from walking the parent processes (`proc_pidinfo` for the parent pid, `proc_pidpath`) up to the first one inside a `.app`, at most 16 levels; without one, "the app that started it".
    - Windows and Linux: the executable path; on Windows the camera and microphone switches apply to all desktop apps, and the line says so.

11. **First run.** The first-run page (ADR-0113 decision 5) gets a step after the experience level: the Camera, Microphone and Screen and windows rows of decision 7 with their actions, and **Later**. Nothing is requested without a press. The step is skipped when no kind is `NotAsked` or `Blocked`.

12. **Zero cost.** Installed: nothing runs; the page and the first-run step exist only as UI. Reading a status is a few OS calls on the UI thread when the page is shown or regains focus. A request starts one short-lived thread. No polling, no background thread, no per-frame work, no hot path touched.

13. **Control API, later.** A read-only `permissions.status` (the kinds and their statuses) comes with a later control API minor, in its own change, so an agent can learn why a camera is black. Requesting stays a UI action, because an OS prompt must answer to a person's press.

14. **UI is approved separately.** The page, the first-run step, the source action and the texts are a visible change: they follow the UI proposal with dark and light screenshots and the owner's approval before implementation (CLAUDE.md rules 13, 16 and 18), use library components and tokens only, and add the glossary terms above.

## Consequences

- The owner's case is fixed at its cause: adding a camera to a build that never asked shows the macOS prompt, when it can be shown safely; a blocked camera says how to unblock it; the page says which app the grant belongs to, so a worktree build that is a different app to macOS is visible as one.
- Windows gets a real status for cameras and microphones before `open` fails.
- Port crates change in a minor way (`Permission` variants, threading note, capability keys); adapters `rbs-capture-sck`, `rbs-capture-avf`, `rbs-audio-io-coreaudio`, `rbs-capture-mf`, `rbs-audio-io-wasapi` and `rbs-capture-pipewire` (the consent flag) change; `rbs-sources` gets `permissions`; `rbs-app` gets the page, the first-run step, the source action, `privacy` and the identity line.
- Tests: the fold (property test), the consent-store parse (fixtures), the registry calls with the test adapters' scripted permissions, the single-flight worker with a fake that answers on a channel (no sleeps, ADR-0032), the SCK "denied after a refused request in this process" rule with an injected request function.
- Hardware checks, each short: macOS camera prompt from `RBS.app`, refused screen recording turning Blocked, the restart line; Windows camera switch off and on.
- A stable signing identity for development bundles, so grants survive rebuilds, is a separate follow-up in `bundle.sh`.

## Alternatives considered

- **Ask for everything at first start.** Prompts with no context get refused, and a refused macOS prompt is not shown again; Apple's guidance is to ask at the moment of use. First run offers the requests instead (decision 11).
- **Let `open` prompt.** It runs on the control path with a time budget, and a prompt from a scene load at start-up would surprise the user. The ports keep "open never prompts".
- **Poll the statuses to keep the page live.** A timer for a page rarely open (P13); focus return and Check again cover the real case.
- **A new `SourceFault::PermissionNotAsked`.** It would change the control API to tell apart two states the action can read from the registry when it is pressed.
- **Read the macOS TCC database.** Private, behind Full Disk Access, and changes between releases.
- **WinRT `AppCapability::CheckAccess` on Windows.** Documented, but its behaviour for unpackaged desktop apps is not, and it pulls WinRT activation into two adapters that do not use it today. The registry reads are cheap and fall back to today's behaviour when the layout changes. To be revisited if the layout changes.
- **Prompt from a bare executable anyway.** AVFoundation ends the process when the responsible app lacks the usage key; decision 5 prevents that instead.

## Review

Reviewer: independent AI session
Date: 2026-11-17
Verdict: changes requested (round 1)
Concerns:
1. ScreenCaptureKit cannot report Denied, so a refused screen permission stayed "Not asked" and Allow did nothing -> decision 5: Denied after a refused request in this process.
2. `Unknown` could not come from port data without an OS check (P3) -> decision 2: `Permission::Unknown`; decision 3: per-kind keys, `audio.input.permission` kept as an alias.
3. A prompt can end a bare executable whose responsible app lacks the usage key; `NSAudioCaptureUsageDescription` missing -> decision 5: `.prompt` capability only with the key in the main bundle; key added.
4. Worker thread versus "control thread only", no single flight, quit while waiting, activation -> decisions 2 and 4.
5. Windows facts (HKLM is the device switch; policy is under AppPrivacy), undocumented layout -> decision 6; WinRT alternative recorded.
6. Restricted and policy blocks are not user-fixable -> `Managed` status, no action.
7. Linux portal wording -> `capture.pick.consent`, "Asked each time".
8. Parent-process guess too shallow under cargo -> ancestor walk to the first `.app`, 16 levels.
9. Reopen after a screen recording grant fails until restart -> restart capability; no reopen.
10. UI outside the ADR, mixed terms -> one term set; decision 14.
11. Semver and test names -> decisions 1, 2, 6 and Consequences.
Round 2 (same reviewer, 2026-11-17): changes requested, minor; all round-1 concerns resolved.
1. The restart line could never show (the process keeps reading the old answer) -> decision 7: shown for restart kinds that read Blocked or Not asked on focus return.
2. `.prompt` checked the wrong app when RBS.app's binary is started from a terminal -> decision 5: also requires no other `.app` above in the ancestor walk.
3. The alias changes WASAPI's `audio.input.permission` value -> decisions 2 and 3: deprecated alias, table updated, values stated.
Status: all concerns resolved in the text; owner approval pending.
