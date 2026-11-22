# Command path inventory, 2026-11

First-pass classification of every `rbs_studio_host::Command` variant on main `c424931` for [ADR-0163](../adr/0163-one-command-path-for-the-window-and-the-api.md) (one command path, quality plan WP-20). Made by reading each variant's handler arm and the control API method names (`rbs_api::Command::method`), corrected after the ADR's first review. Each namespace step checks its rows again against the code and the control API spec; where a row is wrong, the step's `Command::path` arm is the record, not this table.

Step 0 moved `ResetLoudness` to `Command::Api`; step 1 (scenes and transitions) moved ten scene variants and made Switch scene and Studio mode translations; step 2 (items) moved the Sources row's lock, eye and layer buttons, Add existing source and the timed show; step 3 (sources, first part) moved rename, the per-source slide, keep open, the sync offset, Duplicate source, Copy, Paste and Reset attributes, Delete source, the companion audio, global sources, the capture region and display chrome; step 4 (filters) moved add, remove, move and on/off of a filter; step 5 (audio) moved the monitor mode, solo, device, volume and the device list (the first ticketed answer); step 6 (outputs, first part) moved adding and removing the vertical canvas, then (owner's choice 2026-11-21) the record and stream stops, with `record.stop` ending a file gracefully, and Remove from scene, with `item.remove` sending the undo toast; `command_path.rs` holds the current counts. First-pass counts: 21 Translates (already run through `run_ui_api` or `ApiTarget::run`), 83 Pending, 10 Contract, 20 Missing, 27 WindowOnly (13 View, 9 LocalOnly, 2 Preview, 3 Mirror). Total 161. Corrected after review rounds 1 to 3.

## Known differences between the two paths

Each step decides these for its variants; the window takes the API's behaviour unless a visible text changes (owner's approval).

| Difference | Window path today | API path | Where |
| --- | --- | --- | --- |
| Start (and replay arm) while a profile or collection switch is pending or Test this computer runs | Not refused | `output_refusal` refuses with `conflict` | `control_api.rs` `output_refusal` |
| Refusal logging | Not logged | `failures::log_api_refusal` | `control_api.rs` `handle` |
| Refusal text | `Refusal::text()` | `refused()` wraps it in an `ApiError` | `studio_host.rs` `answer`, `refusal.rs` |
| Panic in a start | `answer_on_panic` sends `NotStarted` | `Internal` error | `failures.rs` |
| Box and crop answers | `BoxPlaced` / `CropPlaced` by `seq` | `Response` with `seq` | `items.rs` |
| `api_*` helpers called directly (duplicate, attributes, reset) | Skips `output_refusal` and containment | Full `run` | `scenes.rs` |
| Vertical canvas box | `set_vertical_box` directly | `item.set_box` with `canvas: vertical` | `items.rs`, `api_item.rs` |
| Drag gestures on crop, style, transform, scroll and source settings | One undo step per gesture | No `gesture` field: each call its own step | `request.rs` (`SetItemCrop`, `SetSourceSettings`) |
| Text source `file` in source settings | Allowed | Refused "whatever the token's scope" | `api_source2.rs` |
| Add source | Asks for OS permission, opens the system picker, then adds | Adds only | `sources.rs` (`prompt_first`, `pick`) |
| Measure sync offset refusals | Shown in the Measure view's phase | `ApiError` | `sync_measure.rs` |
| Record and stream starts | Window sends its own `RecordRequest` / `StreamRequest` | Built from host state | `studio_ui.rs`, `api_item2.rs` |
| Virtual camera start | Window sends size and rate | `vcam.start` takes none | `outputs.rs` |
| T-bar steps and Interact input | Stale ones ignored | Refused (`conflict`) | `api_scene.rs`, `interact.rs` |
| Undo / Redo with nothing to undo | Silent | `conflict` | `history.rs`, `api_item2.rs` |
| Remove from scene | Sends `Update::Deleted` (undo toast) | Sends nothing | `sources.rs`, `api_source2.rs` |
| Forget sign-in | Stops the page first | Does with `restart: true` (protocol 1.101, ADR-0168) | `source_settings.rs`, `api_source2.rs` |
| Filter values on an unchanged release | Restores the stored values | Does not for a client; a window call does (ADR-0168 step 1) | `api_canvas.rs` |
| Collection transition | Sends the whole transition it last saw | A patch of the parts given; the stinger cut point is checked against the file's length | `scenes.rs` `set_transition`; the window now sends only the parts that changed |
| Scroll controls | Published the views after a control | Did not; now publishes too (step 6) | `items.rs` `api_scroll` |
| Where an addition goes when no scene is named (`item.add`, `source.add`) | The scene the window edits: the preview scene in studio mode | The program scene (protocol 1.6); a window call now uses the edited scene (step 6 fix: `item.add` from the window went on air in studio mode since step 2) | `scenes.rs` `placing_into`, `studio_host.rs` `api_command` |
| Canvas change | Always sends `Update::Canvas`, also for a refusal or an unchanged canvas | Nothing for those | `canvas.rs`, `window_answers.rs` (a window call gets `Update::Canvas` in every case, ADR-0168) |
| Media play, pause, seek, restart | Forces the next media-state publish | Does not | `source_settings.rs`, `api_source2.rs` |
| Pause, hold, break refusals | Own sentence ("The recording could not pause or resume: ...") | `ApiError` message | `stream.rs` |
| Lock, eye and layer buttons on a row that is gone | Host refused "There is no source ..." | The window sends nothing (it names the row's item) | `studio_ui/item_calls.rs` |
| Stop recording (main and vertical) | `end_recording`: ends the output gracefully (`ending`), stops at once only when that fails; silent when nothing records | `stop_recording` at once; `conflict` "nothing records" | `recording.rs`, `api_record.rs` |
| End stream, stop the virtual camera, disarm replay with nothing running | Silent | `conflict` | `outputs.rs`, `api_record.rs`, `api_vcam.rs` |
| A refused source edit (rename, slide, companion) | Error bar, then a publish | Error bar only; nothing changed, so nothing to publish | `studio_host.rs` `answer` |
| Companion audio for a source that is gone | "There is no source ..." | "no source N" | `api_studio.rs` |

## Variants

| Variant | Path | API methods, or why |
| --- | --- | --- |
| `ListTargets` | Pending | source.targets |
| `AddSourceWithAudio` | Pending | source.add (with_audio: true); split as AddSource |
| `AddSource` | Pending | source.add; split: the OS permission prompt and system picker stay in the window (LocalOnly) |
| `DeleteSource` | Moved to `Command::Api` (step 3) | source.delete |
| `RemoveFromScene` | Moved to `Command::Api` (step 6) | item.remove; must still send Update::Deleted for the undo toast |
| `SetGlobal` | Moved to `Command::Api` (step 3) | global.add, global.remove |
| `RemoveGlobal` | Moved to `Command::Api` (step 3) | global.remove |
| `AddGlobalSource` | Pending | source.add (global: true) |
| `UndoDelete` | Moved to `Command::Api` | edit.undo_delete; the window is quiet on `conflict` (ADR-0164 step 4) |
| `Undo` | Moved to `Command::Api` (step 3) | edit.undo, sent with `ApiReply::QuietIf(conflict)`: nothing to undo says nothing, a refused step (`undo_conflict`) is shown |
| `Redo` | Moved to `Command::Api` (step 3) | edit.redo, sent with `ApiReply::QuietIf(conflict)`: nothing to undo says nothing, a refused step (`undo_conflict`) is shown |
| `MoveSource` | Moved to `Command::Api` (step 2) | item.move |
| `MoveScene` | Moved to `Command::Api` (step 1) | scene.move |
| `MoveSceneTo` | Moved to `Command::Api` (ADR-0164 step 2) | scene.move with `to_scene` |
| `MoveSourceTo` | Moved to `Command::Api` (ADR-0164 step 2) | item.move with `to_item`; the window is quiet on `invalid_params` (a place in another scene changed nothing before) |
| `SetSourceLocked` | Moved to `Command::Api` (step 2) | item.set_locked |
| `SetSourceVolume` | Translates | source.set_volume, scene.audio.set |
| `SetSourceMuted` | Translates | source.set_muted, scene.audio.set |
| `SetMetersUi` | WindowOnly View |  |
| `EnsureEncoders` | WindowOnly View |  |
| `SetSceneOwnLevels` | Translates | scene.audio.own, scene.audio.shared |
| `ResetSceneLevel` | Translates | scene.audio.reset_source |
| `AddVerticalCanvas` | Moved to `Command::Api` (step 6) | canvas.add_vertical |
| `RemoveVerticalCanvas` | Moved to `Command::Api` (step 6) | canvas.remove_vertical |
| `AutoLayoutVertical` | Translates | canvas.auto_layout |
| `SetVerticalSize` | Translates | canvas.set_vertical_size |
| `SetMasterVolume` | Translates | canvas.set_master_volume |
| `SetMasterMuted` | Translates | canvas.set_master_muted |
| `SetLoudnessShown` | Translates | loudness.set_enabled |
| `ResetLoudness` | Moved to `Command::Api` (step 0) | loudness.reset |
| `SetLoudnessSettings` | Translates | settings.set |
| `SetSourceMonitor` | Moved to `Command::Api` (step 5) | source.set_monitor |
| `SetMonitorSolo` | Moved to `Command::Api` (step 5) | monitor.set_solo |
| `SetMonitorDevice` | Moved to `Command::Api` (step 5) | monitor.set_device |
| `SetMonitorVolume` | Moved to `Command::Api` (step 5) | monitor.set_volume |
| `ListMonitorDevices` | Moved to `Command::Api` (step 5) | monitor.devices; the first ticketed question: `Update::ApiAnswer` replaces `Update::MonitorDevices` |
| `SetSourceMixerHidden` | Translates | source.set_mixer_hidden |
| `AddFilter` | Moved to `Command::Api` (step 4) | filter.add |
| `AddVoiceChain` | Moved to `Command::Api` (ADR-0164 step 3) | filter.add_voice_chain |
| `ApplyLook` | Translates | look.apply |
| `AddSample` | Translates | source.add_sample |
| `AddStamp` | Moved to `Command::Api` (ADR-0164 step 3) | source.add kind `color` with `stamp` |
| `AddImage` | Moved to `Command::Api` (ADR-0164 step 3) | source.add kind `image` with `path` |
| `AddMedia` | Moved to `Command::Api` (ADR-0164 step 3) | source.add kind `media` with `path` |
| `SetStingerFile` | Moved to `Command::Api` (ADR-0164 step 3) | collection.set_stinger_file |
| `AddBrowser` | Moved to `Command::Api` (ADR-0164 step 3) | source.add kind `browser` with `url`; without the engine add-on the window opens it on the `failed` answer's `data.reason: needs_addon` |
| `AddText` | Moved to `Command::Api` (step 6) | source.add (kind text) |
| `FitTextBox` | Moved to `Command::Api` (ADR-0164 step 2) | item.fit_text_box |
| `ListTextFonts` | Moved to `Command::Api` (step 3) | text.fonts with `ApiReply::QuietTicket`; on `not_ready` the host still sends `Update::TextFonts` once the list is read, as the window path did |
| `TextTimer` | Moved to `Command::Api` (step 6) | text.timer (a source gone meanwhile stays quiet; the API now answers "not a timer" with the window's sentence) |
| `SetSourceFill` | Moved to `Command::Api` (step 6) | source.set_settings (`color`) |
| `SetSourceFilled` | Moved to `Command::Api` (ADR-0164 step 2) | item.set_filled; the window names the main canvas, as before |
| `RemoveFilter` | Moved to `Command::Api` (step 4) | filter.remove |
| `MoveFilter` | Moved to `Command::Api` (step 4) | filter.move |
| `SetFilterEnabled` | Moved to `Command::Api` (step 4) | filter.set_enabled |
| `SetFilterValues` | Moved to `Command::Api` (ADR-0168 step 1) | filter.set_settings; for a window call an unchanged release puts the stored values back on air |
| `PreviewFilter` | WindowOnly Preview |  |
| `RestoreFilter` | WindowOnly Preview |  |
| `OpenSyncMeasure` | WindowOnly View |  |
| `StartSyncMeasure` | Moved to `Command::Api` (ADR-0168 step 1) | sync.measure_start; for a window call the host shows the answer in the Measure view, a refusal with its reason |
| `CloseSyncMeasure` | Translates (ADR-0168 step 1) | sync.measure_cancel with the id of the run the window started |
| `ApplySyncToDeviceSources` | Moved to `Command::Api` (step 6) | sync.apply_to_device_sources |
| `SetSourceSyncOffset` | Moved to `Command::Api` (step 3) | source.set_sync_offset |
| `RetargetSource` | Moved to `Command::Api` | source.retarget; the host fills the name, the app and the sound from its own list for every caller, and an unlisted target is `not_found` (ADR-0164 decision 6) |
| `GrabThumbnails` | WindowOnly View |  |
| `StopThumbnails` | WindowOnly View |  |
| `OpenSourceSettings` | WindowOnly View |  |
| `CloseSourceSettings` | WindowOnly View |  |
| `SetSourceSettings` | Moved to `Command::Api` (ADR-0164 step 1) | source.set_settings with `gesture`; the text-file refusal is a caller check, so the window passes |
| `ReloadSource` | Moved to `Command::Api` (step 3) | source.reload; the API now publishes after a restart, as the window did |
| `BrowserControl` | Translates (ADR-0168 step 1) | browser.reload or browser.run_script while the page runs; a reload of a page that does not run is source.reload, a script with no page is quiet |
| `ForgetBrowserSignIn` | Moved to `Command::Api` (ADR-0168 step 3) | browser.forget_sign_in with `restart: true` (protocol 1.101): a running page restarts with the new profile, as before |
| `Media` | Moved to `Command::Api` (step 6) | media.play, media.pause, media.seek, media.restart (the API now also forces the next media-state publish) |
| `SetCompanionAudio` | Moved to `Command::Api` (step 3) | source.set_companion_audio |
| `RenameSource` | Moved to `Command::Api` (step 3) | source.rename |
| `SetGlide` | Moved to `Command::Api` (step 1) | collection.set_glide |
| `SetTransition` | Moved to `Command::Api` (step 6) | collection.set_transition, with only the parts that changed |
| `SetSourceActivity` | Moved to `Command::Api` (step 3) | source.set_activity with the linger the row shows; `set_source_activity` now stores a value equal to the type default as absent for both paths |
| `SetSourceGlide` | Moved to `Command::Api` (step 3) | source.set_glide |
| `AddExistingSource` | Moved to `Command::Api` (step 2) | item.add |
| `AddScene` | Moved to `Command::Api` (step 1) | scene.add |
| `DuplicateScene` | Moved to `Command::Api` (step 1) | scene.duplicate |
| `DuplicateSceneWithCopies` | Moved to `Command::Api` (step 1) | scene.duplicate |
| `DuplicateSource` | Moved to `Command::Api` (step 3) | source.duplicate |
| `CopyAttributes` | Moved to `Command::Api` (step 3) | source.copy_attributes |
| `OpenAttributes` | WindowOnly View |  |
| `ResetSource` | Moved to `Command::Api` (step 3) | source.reset |
| `PasteAttributes` | Moved to `Command::Api` (step 3) | source.paste_attributes |
| `RemoveScene` | Moved to `Command::Api` (step 1) | scene.remove |
| `SetSceneTransition` | Moved to `Command::Api` (step 1) | scene.set_transition |
| `RenameScene` | Moved to `Command::Api` (step 1) | scene.rename |
| `SwitchScene` | Translates (step 1) | scene.switch, canvas.set_preview; becomes Translates (branches on the preview) |
| `SetStudioMode` | Translates (step 1) | canvas.set_preview |
| `Transition` | Moved to `Command::Api` (step 1) | canvas.transition |
| `TransitionProgress` | Moved to `Command::Api` (step 1) | canvas.transition_progress; quiet answer (stale T-bar steps are ignored today) |
| `Rehearse` | Moved to `Command::Api` | canvas.rehearse; the window is quiet on `conflict` (ADR-0164 step 4) |
| `SetSwapAfterTransition` | WindowOnly(Mirror) | settings.set `swap_after_transition` (ADR-0164 step 5); the window saves the file it owns |
| `SetSourceActive` | Moved to `Command::Api` (step 3) | source.set_active |
| `SetSourceVisible` | Moved to `Command::Api` (step 2) | item.set_visible |
| `SetSourceBox` | Moved to `Command::Api` (ADR-0168 step 2) | item.set_box with a ticket; its `{seq}` answer replaces `BoxPlaced`, on both canvases |
| `SetSourceCrop` | Moved to `Command::Api` | item.set_crop with `gesture`, answered by ticket; a refusal puts the handles back (ADR-0164 step 6) |
| `TransformSource` | Moved to `Command::Api` (ADR-0166) | the item transform methods and item.set_rotation; the window maps each op |
| `SetSourceRegion` | Moved to `Command::Api` (ADR-0164 step 2) | item.set_shown_region; the window names the main canvas, as before (the vertical canvas's Show buttons are a follow-up) |
| `SetRecordSeparately` | Moved to `Command::Api` (step 6) | source.set_record_separately |
| `SetSourceCaptureRegion` | Moved to `Command::Api` (step 3) | source.set_region |
| `SetDisplayChrome` | Moved to `Command::Api` (step 3) | source.set_chrome |
| `FitDisplayChrome` | Moved to `Command::Api` (step 3) | source.fit_chrome |
| `ResetDisplayChrome` | Moved to `Command::Api` (step 3) | source.reset_chrome |
| `SetItemShowFor` | Moved to `Command::Api` (step 2) | item.set_show_for |
| `ShowItem` | Moved to `Command::Api` (step 2) | item.show |
| `SetSourceStyle` | Moved to `Command::Api` (ADR-0164 step 1) | item.set_style with `gesture` |
| `SetSourceScroll` | Moved to `Command::Api` (ADR-0164 step 1) | item.set_scroll with `gesture` |
| `ScrollLive` | Moved to `Command::Api` (step 6) | item.scroll_pause, item.scroll_resume, item.scroll_restart (the API now publishes the views after a control, as the window path did) |
| `StartRecording` | Moved to `Command::Api` | record.start {profile: "default", canvas} with `iso` absent; refusals carry `data.reason` (ADR-0164 step 6) |
| `StopRecording` | Moved to `Command::Api` (step 6) | record.stop |
| `StartVerticalRecording` | Moved to `Command::Api` | as StartRecording, `canvas: "vertical"` (ADR-0164 step 6) |
| `StopVerticalRecording` | Moved to `Command::Api` (step 6) | record.stop |
| `StartVerticalStream` | Moved to `Command::Api` | as StartStream, `canvas: "vertical"` (ADR-0164 step 6) |
| `EndVerticalStream` | Moved to `Command::Api` (step 6) | stream.stop |
| `SetVerticalProfile` | WindowOnly(Mirror) | settings.set `vertical_profile` (ADR-0164 step 5); the window saves the file it owns |
| `SetVerticalStreamProfile` | WindowOnly(Mirror) | settings.set `vertical_stream` (ADR-0164 step 5); the window saves the file it owns |
| `SetStreamDestinations` | WindowOnly(LocalOnly) | servers and key flags are the local user's; Use on Go live sends `stream.destination.set_enabled` (ADR-0164 step 5) |
| `SetVerticalFlags` | Moved to `Command::Api` (step 6) | output.set_with_vertical, one call per choice that changed (and both at the start) |
| `SetSeparateFiles` | WindowOnly(Mirror) | settings.set `separate_files` (ADR-0164 step 5); the window saves the file it owns |
| `SetVerticalPicture` | WindowOnly View |  |
| `OpenProjector` | WindowOnly View |  |
| `CloseProjector` | WindowOnly View |  |
| `OpenInteract` | WindowOnly View |  |
| `CloseInteract` | WindowOnly View |  |
| `InteractInput` | Pending | browser.input; stays: one wire event per mouse move, the API takes a checked list of its own event form; input to a page not in Interact is ignored, the API refuses it |
| `SetCanvas` | Moved to `Command::Api` (ADR-0168 step 1) | settings.set (canvas); for a window call the host sends `Update::Canvas` when the call sent none (a refusal or an unchanged canvas), so the preview opens again; an unchanged canvas while recording is no longer refused |
| `SetProfile` | WindowOnly Mirror |  |
| `StartStream` | Moved to `Command::Api` | stream.start {canvas}; the host builds from the saved settings, refusals carry `data.reason` (ADR-0164 step 6) |
| `EndStream` | Moved to `Command::Api` (step 6) | stream.stop |
| `StartVirtualCamera` | Moved to `Command::Api` | vcam.start after `SetVirtualCamera`; the window is quiet on `conflict` (ADR-0164 step 6) |
| `StopVirtualCamera` | Moved to `Command::Api` (ADR-0168 step 1) | vcam.stop, quiet on `conflict`; for a refused window call the host sends `VirtualCamera` with the camera's state |
| `SetVirtualCamera` | WindowOnly LocalOnly |  |
| `SetReplay` | WindowOnly(Mirror) | settings.set `replay` (ADR-0164 step 5); the window saves the file it owns |
| `ArmReplay` | Moved to `Command::Api` (step 6) | replay.arm (a refusal because it is already armed stays quiet) |
| `DisarmReplay` | Moved to `Command::Api` (step 6) | replay.disarm (a refusal because it is not armed stays quiet) |
| `SaveReplay` | Moved to `Command::Api` (ADR-0168 step 1) | replay.save, quiet; for a refused window call the host sends the refusal as `ReplaySaved` |
| `AddMarker` | Translates | marker.add |
| `RemoveMarkers` | Translates | marker.remove |
| `PauseRecording` | Translates | record.pause_toggle (own error sentence around the refusal) |
| `HoldStream` | Translates | stream.hold_toggle (own error sentence around the refusal) |
| `Break` | Translates | output.break_toggle (own error sentence around the refusal) |
| `SetStreamProfile` | WindowOnly Mirror |  |
| `SaveStreamKey` | WindowOnly LocalOnly |  |
| `SetKeyAccount` | WindowOnly LocalOnly |  |
| `SaveVerticalStreamKey` | WindowOnly LocalOnly |  |
| `SaveDestinationKey` | WindowOnly LocalOnly |  |
| `StartDestination` | Translates | stream.destination.start |
| `StopDestination` | Translates | stream.destination.stop |
| `RemoveStreamKey` | WindowOnly LocalOnly |  |
| `SaveImportedKey` | WindowOnly LocalOnly |  |
| `SwitchCollection` | WindowOnly Mirror |  |
| `SetLibrary` | Missing | no settings.set field |
| `ReadPermissions` | WindowOnly LocalOnly |  |
| `RequestPermission` | WindowOnly LocalOnly |  |
| `ListAddOns` | Moved to `Command::Api` (ADR-0168 step 2) | addon.list with a ticket; the answer fills Settings › Add-ons |
| `AddOn` | Moved to `Command::Api` | addon.install, addon.update, addon.cancel and addon.remove (ADR-0164 step 4) |
