# Command path inventory, 2026-11

First-pass classification of every `rbs_studio_host::Command` variant on main `c424931` for [ADR-0163](../adr/0163-one-command-path-for-the-window-and-the-api.md) (one command path, quality plan WP-20). Made by reading each variant's handler arm and the control API method names (`rbs_api::Command::method`), corrected after the ADR's first review. Each namespace step checks its rows again against the code and the control API spec; where a row is wrong, the step's `Command::path` arm is the record, not this table.

Step 0 moved `ResetLoudness` to `Command::Api` (Translates 21 to 20); `command_path.rs` holds the current counts. First-pass counts: 21 Translates (already run through `run_ui_api` or `ApiTarget::run`), 83 Pending, 10 Contract, 20 Missing, 27 WindowOnly (13 View, 9 LocalOnly, 2 Preview, 3 Mirror). Total 161. Corrected after review rounds 1 to 3.

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
| Forget sign-in | Stops the page first | Does not | `source_settings.rs`, `api_source2.rs` |
| Filter values on an unchanged release | Restores the stored values | Does not | `source_settings.rs`, `api_canvas.rs` |
| Media play, pause, seek, restart | Forces the next media-state publish | Does not | `source_settings.rs`, `api_source2.rs` |
| Pause, hold, break refusals | Own sentence ("The recording could not pause or resume: ...") | `ApiError` message | `stream.rs` |

## Variants

| Variant | Path | API methods, or why |
| --- | --- | --- |
| `ListTargets` | Pending | source.targets |
| `AddSourceWithAudio` | Pending | source.add (with_audio: true); split as AddSource |
| `AddSource` | Pending | source.add; split: the OS permission prompt and system picker stay in the window (LocalOnly) |
| `DeleteSource` | Pending | source.delete |
| `RemoveFromScene` | Pending | item.remove; must still send Update::Deleted for the undo toast |
| `SetGlobal` | Pending | global.add, global.remove |
| `RemoveGlobal` | Pending | global.remove |
| `AddGlobalSource` | Pending | source.add (global: true) |
| `UndoDelete` | Missing | edit.undo has no only-if-a-delete condition |
| `Undo` | Pending | edit.undo; quiet when there is nothing to undo |
| `Redo` | Pending | edit.redo; as Undo |
| `MoveSource` | Pending | item.move |
| `MoveScene` | Pending | scene.move |
| `MoveSceneTo` | Missing | scene.move has no target scene |
| `MoveSourceTo` | Missing | item.move has no target item |
| `SetSourceLocked` | Pending | item.set_locked |
| `SetSourceVolume` | Translates | source.set_volume, scene.audio.set |
| `SetSourceMuted` | Translates | source.set_muted, scene.audio.set |
| `SetMetersUi` | WindowOnly View |  |
| `EnsureEncoders` | WindowOnly View |  |
| `SetSceneOwnLevels` | Translates | scene.audio.own, scene.audio.shared |
| `ResetSceneLevel` | Translates | scene.audio.reset_source |
| `AddVerticalCanvas` | Pending | canvas.add_vertical |
| `RemoveVerticalCanvas` | Pending | canvas.remove_vertical |
| `AutoLayoutVertical` | Translates | canvas.auto_layout |
| `SetVerticalSize` | Translates | canvas.set_vertical_size |
| `SetMasterVolume` | Translates | canvas.set_master_volume |
| `SetMasterMuted` | Translates | canvas.set_master_muted |
| `SetLoudnessShown` | Translates | loudness.set_enabled |
| `ResetLoudness` | Moved to `Command::Api` (step 0) | loudness.reset |
| `SetLoudnessSettings` | Translates | settings.set |
| `SetSourceMonitor` | Pending | source.set_monitor |
| `SetMonitorSolo` | Pending | monitor.set_solo |
| `SetMonitorDevice` | Pending | monitor.set_device |
| `SetMonitorVolume` | Pending | monitor.set_volume |
| `ListMonitorDevices` | Pending | monitor.devices |
| `SetSourceMixerHidden` | Translates | source.set_mixer_hidden |
| `AddFilter` | Pending | filter.add |
| `AddVoiceChain` | Missing | no voice chain method |
| `ApplyLook` | Translates | look.apply |
| `AddSample` | Translates | source.add_sample |
| `AddStamp` | Missing | no shape source in source.add |
| `AddImage` | Missing | source.add has no file path, and the box is sized from the image |
| `AddMedia` | Missing | source.add has no file path |
| `SetStingerFile` | Missing | no stinger file method |
| `AddBrowser` | Missing | source.add has no URL |
| `AddText` | Pending | source.add |
| `FitTextBox` | Missing | no fit-box method |
| `ListTextFonts` | Pending | text.fonts |
| `TextTimer` | Pending | text.timer |
| `SetSourceFill` | Pending | source.set_settings |
| `SetSourceFilled` | Missing | also adds a border to the item style in one undo step; no method |
| `RemoveFilter` | Pending | filter.remove |
| `MoveFilter` | Pending | filter.move |
| `SetFilterEnabled` | Pending | filter.set_enabled |
| `SetFilterValues` | Pending | filter.set_settings; window then restores stored values on an unchanged release |
| `PreviewFilter` | WindowOnly Preview |  |
| `RestoreFilter` | WindowOnly Preview |  |
| `OpenSyncMeasure` | WindowOnly View |  |
| `StartSyncMeasure` | Pending | sync.measure_start; refusals go to the Measure view, needs a ticketed answer |
| `CloseSyncMeasure` | Pending | sync.measure_cancel; as StartSyncMeasure |
| `ApplySyncToDeviceSources` | Pending | sync.apply_to_device_sources |
| `SetSourceSyncOffset` | Pending | source.set_sync_offset |
| `RetargetSource` | Pending | source.retarget |
| `GrabThumbnails` | WindowOnly View |  |
| `StopThumbnails` | WindowOnly View |  |
| `OpenSourceSettings` | WindowOnly View |  |
| `CloseSourceSettings` | WindowOnly View |  |
| `SetSourceSettings` | Contract | source.set_settings needs gesture; its text-file refusal moves to the caller checks |
| `ReloadSource` | Pending | source.reload |
| `BrowserControl` | Pending | browser.reload, browser.run_script |
| `ForgetBrowserSignIn` | Pending | browser.forget_sign_in; window stops the page first, API does not |
| `Media` | Pending | media.play, media.pause, media.seek, media.restart; window forces the next media-state publish |
| `SetCompanionAudio` | Pending | source.set_companion_audio |
| `RenameSource` | Pending | source.rename |
| `SetGlide` | Pending | collection.set_glide |
| `SetTransition` | Pending | collection.set_transition |
| `SetSourceActivity` | Pending | source.set_activity |
| `SetSourceGlide` | Pending | source.set_glide |
| `AddExistingSource` | Pending | item.add |
| `AddScene` | Pending | scene.add |
| `DuplicateScene` | Pending | scene.duplicate |
| `DuplicateSceneWithCopies` | Pending | scene.duplicate |
| `DuplicateSource` | Pending | source.duplicate |
| `CopyAttributes` | Pending | source.copy_attributes |
| `OpenAttributes` | WindowOnly View |  |
| `ResetSource` | Pending | source.reset |
| `PasteAttributes` | Pending | source.paste_attributes |
| `RemoveScene` | Pending | scene.remove |
| `SetSceneTransition` | Pending | scene.set_transition |
| `RenameScene` | Pending | scene.rename |
| `SwitchScene` | Pending | scene.switch, canvas.set_preview; becomes Translates (branches on the preview) |
| `SetStudioMode` | Pending | canvas.set_preview |
| `Transition` | Pending | canvas.transition |
| `TransitionProgress` | Pending | canvas.transition_progress; quiet answer (stale T-bar steps are ignored today) |
| `Rehearse` | Missing | starts a rehearsal on the engine; no method (ui.transition.rehearse is the registry entry) |
| `SetSwapAfterTransition` | Missing | no settings.set field |
| `SetSourceActive` | Pending | source.set_active |
| `SetSourceVisible` | Pending | item.set_visible |
| `SetSourceBox` | Pending | item.set_box |
| `SetSourceCrop` | Contract | item.set_crop needs gesture |
| `TransformSource` | Contract | item.flip/rotate/fit/align/reset_transform need gesture |
| `SetSourceRegion` | Missing | no method for the shown region (ADR-0058 Show buttons); source.set_region is the capture region |
| `SetRecordSeparately` | Pending | source.set_record_separately |
| `SetSourceCaptureRegion` | Pending | source.set_region |
| `SetDisplayChrome` | Pending | source.set_chrome |
| `FitDisplayChrome` | Pending | source.fit_chrome |
| `ResetDisplayChrome` | Pending | source.reset_chrome |
| `SetItemShowFor` | Pending | item.set_show_for |
| `ShowItem` | Pending | item.show |
| `SetSourceStyle` | Contract | item.set_style needs gesture |
| `SetSourceScroll` | Contract | item.set_scroll needs gesture |
| `ScrollLive` | Pending | item.scroll_pause, item.scroll_resume, item.scroll_restart, item.scroll_speed |
| `StartRecording` | Contract | record.start builds its request from host state; the window sends its own RecordRequest |
| `StopRecording` | Pending | record.stop |
| `StartVerticalRecording` | Contract | as StartRecording |
| `StopVerticalRecording` | Pending | record.stop |
| `StartVerticalStream` | Contract | as StartStream |
| `EndVerticalStream` | Pending | stream.stop |
| `SetVerticalProfile` | Missing | no settings.set field |
| `SetVerticalStreamProfile` | Missing | no settings.set field |
| `SetStreamDestinations` | Missing | no settings.set field |
| `SetVerticalFlags` | Pending | output.set_with_vertical |
| `SetSeparateFiles` | Missing | no settings.set field |
| `SetVerticalPicture` | WindowOnly View |  |
| `OpenProjector` | WindowOnly View |  |
| `CloseProjector` | WindowOnly View |  |
| `OpenInteract` | WindowOnly View |  |
| `CloseInteract` | WindowOnly View |  |
| `InteractInput` | Pending | browser.input; quiet answer (input to a closed page is ignored today) |
| `SetCanvas` | Pending | settings.set (canvas) |
| `SetProfile` | WindowOnly Mirror |  |
| `StartStream` | Contract | stream.start takes no request; the window sends its own StreamRequest |
| `EndStream` | Pending | stream.stop |
| `StartVirtualCamera` | Contract | vcam.start takes no size or rate |
| `StopVirtualCamera` | Pending | vcam.stop |
| `SetVirtualCamera` | WindowOnly LocalOnly |  |
| `SetReplay` | Missing | no settings.set field |
| `ArmReplay` | Pending | replay.arm |
| `DisarmReplay` | Pending | replay.disarm |
| `SaveReplay` | Pending | replay.save |
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
| `ListAddOns` | Pending | addon.list |
| `AddOn` | Pending | addon.install, addon.cancel, addon.clean, addon.remove |
