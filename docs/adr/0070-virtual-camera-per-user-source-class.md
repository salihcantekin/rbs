# ADR-0070: Virtual camera on Windows: a per-user source class instead of camera properties

- Status: Accepted
- Date: 2026-11-07
- Deciders: Salih (owner)
- Supersedes: ADR-0066 decision 7 (how the format reaches the source, and how a format change applies) and decision 9 (how the source learns which user may write its frames, and what the elevated step does). Every other part of ADR-0066 stands.
- Builds on: ADR-0066 (virtual camera), ADR-0013 (zero cost when unused), ADR-0014 (review policy), ADR-0032 (fast deterministic tests)

## Context

ADR-0066 has the app create the camera from the user's own, non-elevated process with `MFCreateVirtualCamera`, and store two things on it as camera properties with `IMFVirtualCamera::AddProperty`:

- the user's SID, which the media source uses to set the access list of its frame section (decision 9);
- the announced format, which `set_format` changes by removing and creating the camera again "without administrator rights" (decision 7).

Microsoft's reference for `IMFVirtualCamera::AddProperty` and for `IMFVirtualCamera::AddRegistryEntry` says: "Callers must have administrator-level permissions to use this API." Both return `E_ACCESSDENIED` otherwise. So the non-elevated process cannot store either value on the camera. Doing it from the elevated step breaks two promises:

- The camera would belong to whoever typed credentials into the UAC prompt, not to the user who added it.
- Every later format change would need administrator approval again. The owner asked for exactly one approval, with nothing asked afterwards (2026-11-07).

What else is known:

- `MFCreateVirtualCamera` needs an administrator only for `MFVirtualCameraAccess_AllUsers`. With `MFVirtualCameraAccess_CurrentUser` a standard user may call it.
- Its `sourceId` is "the unique CLSID of the custom media source". A camera is keyed by its parameters, so a different CLSID gives a different camera.
- Frame Server loads a media source only from a class registered machine-wide (`HKLM`), never from `HKCU`.
- The source DLL is loaded by several processes: the app that creates the camera, Frame Server (`LocalService`), Frame Server Monitor (`LocalSystem`) and the app that reads the camera (VCamSample).
- The source gets no documented attribute naming the app or user that opened the camera. VCamSample reads an undocumented client process id, for logging only.

## Decision

1. **One source class per user.** The elevated step (the one UAC prompt of ADR-0066 decision 9) registers a COM class for the user who asked, under `HKLM\SOFTWARE\Classes\CLSID\{CLSID}` (64-bit view; the source is 64-bit only).
   - **The CLSID.** A name-based UUID, RFC 9562 version 5 (SHA-1):
     - namespace `6B2F0C41-935E-4B7A-8D13-52E90FA477C8`;
     - name: the UTF-8 bytes of the SID string exactly as `ConvertSidToStringSidW` writes it (`S-1-5-21-...`).

     The GUID is built from the UUID's RFC byte order (big-endian fields), not by copying raw bytes into Windows' `GUID`, whose first three fields are little-endian. Two vectors are tested:

     | SID | CLSID |
     | --- | --- |
     | `S-1-5-21-1004336348-1177238915-682003330-1001` | `{2C18A9A6-54B7-599A-958F-A50EA1BAB8E3}` |
     | `S-1-5-21-1-2-3-1001` | `{AA0E7BC1-FADC-559C-92B3-0272623DF480}` |
   - **The class key holds:**
     - `InprocServer32`: the signed DLL in `Program Files`, `ThreadingModel = Both`;
     - one value `RbsVcamUser`: the SID string.

     Only administrators can write there, so neither this user nor another one can change which user a class belongs to.
   - The app passes that CLSID to `MFCreateVirtualCamera` as `sourceId`. Each user who adds the camera gets their own class through their own UAC prompt. It is the same DLL with a different registration.
2. **The source learns its user from the class it was created as.** `DllGetClassObject` receives the requested CLSID. It reads the class key from `HKLM` explicitly, with `KEY_WOW64_64KEY`, never through `HKCR`: in a user's process, `HKCR` merges that user's `HKCU\Software\Classes` over `HKLM`. It accepts the CLSID only when:
   - `RbsVcamUser` is a user account SID: a local or domain account (`S-1-5-21-` and four numbers) or an Entra ID account (`S-1-12-1-` and four numbers);
   - the CLSID equals the one derived from it.

   Otherwise it returns `CLASS_E_CLASSNOTAVAILABLE`. The check reads the registry and nothing else.

   In addition, a process whose token user is none of the class's user, `LocalService` or `SYSTEM` gets `CLASS_E_CLASSNOTAVAILABLE` too. A reading app loads the source in its own process, so another user's meeting app cannot use this user's class casually. A determined user can still register the same CLSID under their own `HKCU` and load a DLL of their own, which is why the threat in Consequences remains.

   `InprocServer32` naming another file than this DLL (compared case-insensitively after expanding and canonicalising both paths) is reported as a stale or second install. COM loads the file the key names, so this is a consistency check, not a security one.

   Only a running stream opens the ring: the section is created on the stream's start, which happens in Frame Server. The section's name and access list are as ADR-0066 decision 9 says: `Global\org.rbs.vcam.<SID>`, read and write for the source's own account and for that SID, nothing for anyone else.

   An existing section is used only when two things hold:
   - its owner is the source's own account;
   - every entry of its access list allows only that account or the SID.

   The source's account is `LocalService`, which every `LocalService` service shares. A section squatted by another `LocalService` service is accepted. Those are operating system services, and this risk is accepted.
3. **The format is one small file the user can write.**
   - **The file.** The elevated step creates `%ProgramData%\RBS\VirtualCamera\` (owner Administrators, protected access list: administrators and `SYSTEM` full control, everyone else read) and in it `<SID>.fmt`. If either exists already, the step resets its owner and access list.
   - **The file's access list** is protected and grants:
     - administrators and `SYSTEM`: full control;
     - `LocalService`: read;
     - the SID: read and write data only (`FILE_GENERIC_READ | FILE_WRITE_DATA`), so no delete, rename, attribute, access-list or owner change.

     A file only lets the user fill the disk, which they can do anyway. A registry value would let them grow the machine-wide `SOFTWARE` hive, which every user and every start of Windows pays for.
   - **The content.** 16 bytes, little-endian `u32`s: layout version (1), width, height, frames per second. The app writes them in one `WriteFile` call at offset 0, so the source never sees half a change.
   - **Reading it.** The source opens the file with read access only, reads at most 16 bytes, and falls back to 1280 x 720 at 30 fps for:
     - a missing or short file;
     - another version;
     - a size or rate not offered in ADR-0066 decision 11 (1920 x 1080, 1280 x 720 or 640 x 360; 15, 24, 30 or 60 fps).

     It reads the file when it is activated. Nothing security-relevant is ever read from it.
4. **A format change is a file write.**
   - **The change.** `set_format` writes the file from the non-elevated app. The source reads it at its next activation, so the camera is not removed and created again, and a meeting app keeps the camera it picked.
   - **Now or pending.** `set_format` opens the section (the user has access to it) and reads the reader heartbeat. Fresh means an app reads the camera, and `set_format` returns `Ok(false)` (pending). Otherwise, or when the section does not exist, it returns `Ok(true)`.
   - **While an app reads the camera.** The source keeps announcing the old format until that app closes the camera, and writes the format it announced into the ring header (the ring's layout is the announced format). The session reports that format, and RBS keeps sending in it: the node scales the canvas into it, so the meeting never shows the still picture because of a change. `format_pending` counts the frames sent in the old format. The new format applies when the camera is opened again.
   - **If Frame Server caches the media types across activations** (the hardware test checks this), `set_format` also removes the camera and creates it again with the same parameters, which needs no administrator, while no app reads it.
5. **The elevated step is a separate executable**, `rbs-vcam-setup.exe`, installed and signed next to the DLL in `Program Files`. The app starts it with `ShellExecuteEx` and the `runas` verb, passing `register <SID>` or `unregister <SID>`, where the SID is the app's own token user.
   - **It accepts only one SID.** The SID must be a user account (`SidTypeUser`; the forms of decision 2). It must also equal the user signed in to the tool's own session: the session's user from `WTSQuerySessionInformation`, resolved with `LookupAccountName`. This holds even when another administrator typed credentials into the prompt. It refuses a group or well-known SID, any other user and any other argument.
   - **It writes only the class key of decision 1 and the file of decision 3**, or deletes them.
   - **Creating the camera.** After the tool returns, the app creates the camera itself, non-elevated, with these parameters:
     - `MFVirtualCameraType_SoftwareCameraSource`;
     - `MFVirtualCameraLifetime_System`;
     - `MFVirtualCameraAccess_CurrentUser`;
     - friendly name "RBS Virtual Camera";
     - the class of decision 1.

     The camera belongs to the user who added it.
   - **Remembering the camera.** The app stores the camera's symbolic link in `vcam.toml`, read from the camera's attributes after `Start`. If the hardware test shows the link is not there, the app finds the camera by enumerating video camera device interfaces for the friendly name and its source class.
6. **Status, and turning the camera on again.**
   - **What `status` checks** (no thread, no prompt):
     - whether the user's class key and format file exist;
     - whether the camera's device interface exists (`CM_Get_Device_Interface_Property` on the stored symbolic link).
   - **The states it reports:**
     - `NotInstalled`: nothing registered, or registered but the camera is missing (the user turned the setting off). `install` tells the two apart: only the first needs the prompt;
     - `NeedsUpdate { reason }`: the class names another DLL than this install's;
     - `Installed`: registered, and the camera exists.
   - **Turning it on again.** `install` runs the setup tool only when the registration is missing. When only the camera is missing (the user turned the setting off, which removes the camera but keeps the registration), `install` creates it without a prompt.
   - **Privacy setting.** `E_ACCESSDENIED` from `MFCreateVirtualCamera` while Windows' camera privacy setting blocks desktop apps becomes `PermissionDenied`, with the reason "Allow desktop apps to use the camera in Windows Settings" (text checked with the `ui-review` skill and the glossary when the UI is built).
   - **Opening with exact rights.** The app and the source open the class key with `KEY_QUERY_VALUE` only, and the format file with only the rights its access list grants them.
   - **Version mismatch.** A ring layout mismatch between app and DLL is already caught by the ring header's version and shows as `NeedsUpdate` (ADR-0066 decision 9). The class key carries no version of its own.
7. **Updates and removal.**
   - **Updates.** The RBS installer runs elevated anyway. On an update it rewrites every user's class key (`InprocServer32` path) and keeps the format files, so an update asks no user for anything.
   - **Turning the setting off** removes the user's camera only (non-elevated).
   - **"Remove completely"** first removes the user's camera (non-elevated). It then runs the setup tool with `unregister <SID>`: a second UAC prompt that deletes this user's class and format file.
   - **The uninstaller** deletes every user's class and format file. It removes other users' virtual camera devices through the device installation API: the software devices whose source class is an RBS class. Which device property carries the source class is found in the hardware test. If the hardware test shows it cannot, an orphaned "RBS Virtual Camera" that no longer activates is documented in the uninstall notes.

## Consequences

- **Prompts.** One UAC prompt per user, once. Format changes, turning the camera on and off, and RBS updates ask nothing afterwards, as the owner asked.
- **Threat: another account on the computer can view the camera while it sends, silently.** Any other account that can run a program on this computer while this user sends can create a camera of their own on this user's class, because the class id is derived from the public SID. That includes another signed-in user, a Remote Desktop session, a scheduled task, or another account's background process. That camera then shows what RBS sends.
  - The sender is not told, and the other account can record what it sees.
  - The source cannot tell which app or user opened it, so it cannot refuse for sure. The in-process check of decision 2 stops casual misuse only.
  - Writing frames stays this user's alone: the section's access list allows only them.
  - The attack is inferred from the documentation, which does not restrict which class a `CurrentUser` camera may use. The hardware test tries it with a second account.
  - The setting's dialog says: "Other accounts on this computer could also see what RBS sends while it sends, and you would not be told."
  - **The owner's decision (2026-11-07):** accepted, with the in-process check of decision 2. The other options were accepting without the check and waiting for a documented way to identify the reader.
- **A second executable in `Program Files`.** The setup tool is signed with the DLL, takes exactly one argument pair and checks it as decision 5 says. It is reviewed with the `risky-review` skill.
- **Registry and files.** `HKLM` grows by one class key per user who added the camera, and `%ProgramData%` by one 16-byte file. "Remove completely" and the uninstaller remove both.
- **Cost (P13).** The source reads the registry and the file once per activation and never per frame. Off, nothing changes.
- **Threading.** The source's objects have no thread affinity: their state is behind locks. The DLL may be loaded into a reading app on a single-threaded apartment thread while Media Foundation calls it from work-queue threads. The in-process COM test calls it from several threads and records which of `IAgileObject` and `IMarshal` the objects answer to.
- **Tests (in addition to ADR-0066's):**
  - The CLSID derivation and the format parsing are pure functions with the vectors above.
  - The class check takes a root key as input. Tests run it against a per-test private hive (`RegLoadAppKey`), so parallel test processes never share `HKLM` state. They check that a key without a SID, with a group SID, with a mismatched SID or with another DLL path is refused or reported, and that `S-1-5-21` and `S-1-12-1` user SIDs pass.
  - The in-process DLL test redirects `HKEY_LOCAL_MACHINE` with `RegOverridePredefKey` to such a hive.
  - The setup tool refuses malformed arguments, group SIDs and SIDs other than the session user's.
- **Hardware test on SC-PC.** One UAC prompt. Then, each without a prompt: a format change (and whether the camera has to be removed and created again for it), turning the camera off and on, an RBS update, and a second Windows user. A second account also tries to create a camera on the first account's class. After that, "Remove completely" and the uninstaller leave no camera behind.
- **The spec changes in the same change.** `docs/specs/virtual-camera.md` gets the install, status and `set_format` meanings, the setup tool, the registry and file layout and access lists, the new reasons and the test names. Its line saying that a pending format change shows the still picture changes to decision 4's behaviour.

## Alternatives considered

- **Do everything in the elevated step**, including creating the camera and storing its properties. Rejected: the camera would belong to the administrator who typed credentials, and every format change would need the prompt again.
- **One fixed class, with the SID and format in a per-user `HKCU` key.** Rejected: the source runs as `LocalService` and has no documented way to know which user opened the camera, so it would not know whose `HKCU` to read.
- **The client process id that Frame Server passes in an undocumented activation attribute.** Rejected: undocumented, and a process id is not a trustworthy identity.
- **A section any interactive user may write.** Rejected: on a shared computer another user could put their own frames into someone else's camera.
- **A random, secret class id.** Rejected: the names of class keys under `HKLM\SOFTWARE\Classes\CLSID` are visible to every user, so the secret would not stay secret.
- **Four separate registry values for the format.** Rejected: the source could read a half-written change, such as 1920 x 720.
- **One registry value in a user-writable `HKLM` key.** Rejected after review: `KEY_SET_VALUE` cannot be limited to one value, so a user could grow the machine-wide `SOFTWARE` hive.

## Review

- 2026-11-07, independent AI session (ADR-0014, `architecture-review` skill; given only the proposal, ADR-0066, the virtual camera spec, the principles and CLAUDE.md). Verdict: changes requested (1 blocker, 5 major, 8 minor). Addressed in this revision:
  1. (blocker) Another signed-in user can create a camera on this user's class and view what it sends. Stated as a consequence with the dialog text; it is left to the owner to accept. The false claim that the binding protects reading is removed.
  2. (major) The setup tool trusted any SID. It now accepts only a user SID equal to the session's signed-in user (decision 5).
  3. (major) Updates brought the prompt back. The installer rewrites every class key, and the layout version stays in the ring header only (decisions 6 and 7).
  4. (major) Uninstall and "Remove completely" left cameras behind. The order is stated, other users' devices are removed by the uninstaller, and both are in the hardware test (decision 7).
  5. (major) The user-writable key was loosely defined. It now has an owner, a protected access list with exact rights, a reset of pre-existing keys, a single fixed-size typed value, and the accepted hive-growth risk is stated (decision 3).
  6. (major) The DLL runs in several processes. The check is side-effect free, only the stream start (in Frame Server) opens the ring, and the shared-`LocalService` risk is stated (decision 2).
  7. (minor) `HKLM` is read explicitly in the 64-bit view, never `HKCR` (decision 2).
  8. (minor) The path check is a consistency check, with canonical comparison (decision 2).
  9. (minor) The derivation is fully specified, with two vectors (decision 1).
  10. (minor) One atomic `Format` value, and only the offered sizes and rates (decision 3).
  11. (minor) A format change is a registry write. Recreating the camera is a fallback only if the hardware test shows caching. The ring header tells the app which format is active (decision 4).
  12. (minor) Threading stated and tested.
  13. (minor) The privacy-setting refusal, the lifetime's name and the status detection are stated (decisions 5 and 6).
  14. (minor) The spec update and the private-hive tests are stated.
- 2026-11-07, re-check by the same session: findings 2-3 and 5-11 resolved; 1, 4, 12 and 13 partly. New: N1 (major) Entra ID accounts (`S-1-12-1`) refused; N2 (minor) a mid-meeting format change blanked the meeting; N3 (minor) how `set_format` decides now or pending. Addressed in this revision:
  - The threat statement now covers every account, says the sender is not told, adds the attack to the hardware test, and offers the in-process check with its limit (decision 2) as an option for the owner.
  - The uninstaller's device match is left to the hardware test (decision 7).
  - The format moved from a registry value to a 16-byte file, so a user cannot grow the `SOFTWARE` hive (decision 3). Exact open rights are stated (decision 6).
  - The GUID byte order is stated (decision 1). Threading is worded as what the test shows. "Registered but no camera" is `NotInstalled` with no prompt, and there is a fallback for finding the symbolic link.
  - N1: `S-1-12-1` user SIDs are accepted (decisions 2 and 5). N2: RBS keeps sending in the announced format until the camera is opened again (decision 4). N3: the reader heartbeat decides (decision 4).
  - Remaining before Accepted: the owner's decision on the threat, recorded here.
- 2026-11-07, owner decision: Salih accepted the cross-account viewing risk with the in-process check (decision 2). Status set to Accepted.
