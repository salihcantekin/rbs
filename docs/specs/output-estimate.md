# Output cost estimate

Status: Draft. Decision: [ADR-0087](../adr/0087-output-cost-estimate-and-hardware-limits.md) (Accepted). This spec covers the estimate function (step A), the disk probe and the `calibration.toml` file (step B), the plan of a start and `record.estimate` (step C, control API 1.53), the start gate (control API 1.54) and the calibration run *Test this computer* (step D1, control API 1.55); the export measurement and the UI are later steps, listed at the end.

## Purpose

Say before a start what it will cost on this computer, which resource sets the limit, and how many separate video and audio tracks fit, so a take does not fail or fall to a CPU encoder halfway through. An unknown limit is reported as unknown, never guessed.

## Parts and layers

| Part | Crate | Notes |
| --- | --- | --- |
| `estimate()`, `EstimatePlan`, `Limits`, `Estimate` | `rbs-record` (`estimate`) | Pure: no clock read, no OS call, no thread, no adapter. Tested with synthetic plans |
| `Studio::start_plan`, `Studio::separate_video_plan` | `rbs-record` (`studio::estimate_plan`) | The plan of a start next to what runs, and the adapters' declared capabilities; reads state only, opens nothing |
| `record.estimate` and the host's limits | `rbs-app` (`studio_host`) | Control API 1.53 (the gate 1.54); `calibration.toml` and the disk probe keyed on the profile's recording folder |
| `separate_video_bound`, `gate` (`GateRequest`, `GatePolicy`, `GateDecision`, `GateRefusal`, `LimitCause`) | `rbs-record` (`estimate::gate`) | Pure: the start gate of control API 1.54 over a plan and limits |
| The start gate in the host | `rbs-app` (`studio_host`) | Decided once before the take folder is made; the desktop buttons are older requests until step F |
| `DiskProbe` (free space per folder, short-lived thread, cache) | `rbs-record` (`estimate`) | Takes the caller's master-clock time and a `ReadFreeSpace` function (`rbs-system-probe` in the app, a fake in tests) |
| `calibration.toml` (`CalibrationFile`, `load`, `save`, `for_estimate`, `prune`) | `rbs-app` (`calibration_file`) | Beside `app.toml`, outside the `UserData` library |
| `calibration::run_encoders`, `measure_write_speed`, `CalibrationPlan` | `rbs-record` (`calibration`) | The calibration run (ADR-0087 decision 4, step D1): plain functions over injected `Lanes`, `Clock`, `Pacer`, `WriteSink` and a stop flag; threads belong to the caller |
| `Studio::calibration_plan` | `rbs-record` (`studio::estimate_plan`) | What a run measures: the classes of a start's plan and each class's candidates |
| The run in the host (`calibration.run`, worker and session threads, merge and save) | `rbs-app` (`studio_host::calibration`) | Control API 1.55 |

## Inputs

- **`EstimatePlan`**: what the start would run, outputs already running included.
  - `video`: one `VideoEncode` per encoder session (encoders shared by several outputs, ADR-0047, are listed once): who uses it (`Recording`, `Stream` or `Iso(track)`), the adapter id the selection would pick and whether it is hardware, codec, width, height, frames per second, rate-control class (`cbr`, `vbr`, `cq`), bytes per second written, whether it falls to a CPU encoder (`falls_to_cpu`, ADR-0087 decision 2), the ISO encode worker it runs on and the pooled export buffers it holds.
  - `audio`: one `AudioEncode` per audio encoder, with its user and bytes per second.
  - `images`: image tracks (ADR-0117), listed and not counted against encoders.
  - `active_sources` (count, informational) and `inactive`: marked sources that cannot be kept active.
  - `video_template` and `audio_template`: one more separate track of each kind, with `used_by` an `Iso` track, for the "at most N" count. `None` when the recording has no picture or no sound.
  - `iso_workers`, `tick` (the canvas tick interval), `give_way_share` (ADR-0086 decision 9, `DEFAULT_GIVE_WAY_SHARE`) and `planned_duration` (default `DEFAULT_PLANNED_DURATION`, 1 hour).
- **`Limits`**: what is known about this computer.
  - `adapters`: per adapter id, `adapter.hardware`, `codec.sessions.max` and `codec.video.pixel_rate.max` as declared.
  - `calibration`: the last measurement, already filtered by the reader to adapters that still probe with the same `adapter.version`. Encoder entries are keyed by adapter, codec, size class, frame-rate class and rate-control class, each with the highest session count, the highest pixel rate and the encode time of one frame; export times per size class; the folder's write speed.
  - `free_space`: the cached probe's free space of the recording folder, `None` when unknown.
  - `previous_tick`: the median of the render partition's last previous-tick times, `None` until it is published.

## Classes

- Size class: the smallest of 720, 1080, 1440 and 2160 lines at or above the frame height, else `above-2160`.
- Frame-rate class: 30, 60 or `above-60`, at or below.
- Rate-control class: `cbr`, `vbr`, `cq`.

## Rows

Every row has a resource, an optional scope (the adapter id, or `video`/`audio` for the ceilings), a need, a limit (`None` when unknown or unlimited), a unit, a level (`None` on informational rows) and whether the limit was measured on this computer.

| Resource | Scope | Unit | Need | Limit | Kind |
| --- | --- | --- | --- | --- | --- |
| `encoder_sessions` | adapter | `sessions` | sessions on the adapter | measured, read by `stopped_by` ("Measured encoder entries"), else declared `codec.sessions.max`; absent on hardware is unknown, on software unlimited (level `fits`) | count |
| `encoder_throughput` | hardware adapter | `pixels_per_second` | sum of width × height × fps | measured pixel rate per class (a limit or a lower bound, "Measured encoder entries"), else declared `codec.video.pixel_rate.max`; mixed classes give the limit at which the same share is used | rate |
| `cpu_load` | none | `pixels_per_second` | software video encoders' pixel rate | measured pixel rate of the software encoders; unknown without a calibration | rate |
| `iso_workers` | none | `ratio` | the busiest ISO encode worker: sum of frame time × fps of its tracks | 1.0; unknown without a calibration of every track's class | rate |
| `iso_render_share` | none | `ratio` | (previous tick + export time per track of its size class) / tick | `give_way_share`; unknown until both the previous tick and the export times are known | rate |
| `memory` | none | `bytes` | width × height × 4 × export buffers per ISO video track | none | informational |
| `disk_speed` | none | `bytes_per_second` | sum of all encoders' bytes per second | measured write speed | rate |
| `disk_space` | none | `bytes` | bytes per second × planned duration | free space | `fits` or `heavy` only |
| `iso_ceiling` | `video`, `audio` | `tracks` | separate tracks of that kind | `MAX_ISO_VIDEO` 16, `MAX_ISO_AUDIO` 32 | count |
| `active_sources` | none | `sources` | sources kept active | none | informational |

`iso_workers`, `iso_render_share` and `memory` are present only when the plan has separate video tracks; `encoder_throughput` only for hardware adapters; `cpu_load` only when a video encoder is software.

## Measured encoder entries

Each `[[encoders]]` entry of `calibration.toml` (an `EncoderMeasure`) carries `stopped_by` (`StopReason`), which says what its `sessions` and `pixels_per_second` mean:

| `stopped_by` | Cause | `sessions` means |
| --- | --- | --- |
| `refused` | an open of session `n ≥ 2` refused (`ResourceExhausted` or `Unsupported`) | a session cap: exactly `n - 1` |
| `late` | step `n` failed (a late round or a lost frame) | a throughput limit: `n - 1` keep up |
| `ceiling` | step 20 passed (`SESSION_CEILING`, `MAX_OUTPUTS + MAX_ISO_VIDEO`) | no session cap for any plan |
| `limit` | the series' share or the run's time ended it during step `n ≥ 2` | a lower bound: at least `n - 1` |
| `cancelled` | a cancel during step `n ≥ 2` | a lower bound |
| `error` | an open or encode error after step 1 | a lower bound |

- **`encoder_sessions`, hardware adapter.** The cap is the lowest `sessions` of the adapter's `refused` entries among the plan's classes; the row is measured against it. Without one: when every class the adapter serves in the plan has a `ceiling` entry, the row `fits` (measured, no cap); otherwise the largest `sessions` among the adapter's entries is a lower bound (a session cap belongs to the adapter): a need up to it `fits` (measured), above it the declared `codec.sessions.max` decides, else the row is unknown (`sessions_unknown`).
- **`encoder_sessions`, software adapter**: unlimited unless `codec.sessions.max` is declared.
- **`encoder_throughput` and `cpu_load`** combine the classes by share. A `late` entry's `pixels_per_second` is a limit, any other entry's a lower bound. When the combined share is at most 1 the row is measured; above 1 it is `over` only when every entry in the sum is `late`, else unknown (hardware falls back to a declared `pixel_rate_max` first).
- **A measured 0** (`late` with `sessions = 0`: one session of the class does not keep up) makes the throughput or CPU row `over` with limit 0, never unknown. A refusal at step 1 writes no entry, so `encoder_sessions` is never measured 0.
- An entry written before 1.55 or by hand without `stopped_by` reads as `limit` when `partial` is true and as `late` otherwise.

## Levels

- `HEAVY_SHARE` = 0.7.
- Count: `fits` when need ≤ limit, `over` when need > limit.
- Rate: `fits` when need < 0.7 × limit, `heavy` when 0.7 × limit ≤ need ≤ limit, `over` when need > limit.
- Disk space: as a rate, but `over` is reported as `heavy`; a disk with no room for the start itself is found when files open.
- Unknown limit: level `unknown`, never `over`.
- `Estimate::gate_level` is the same level computed without the `disk_space` row: what a start's gate judges (ADR-0087 decision 2: disk space never refuses a start).
- The plan's level is the worst of `fits`, `heavy` and `over`; the resources with an unknown limit are listed beside it in `unknown`. A track that falls to a CPU encoder raises the plan to at least `heavy`. A marked source that cannot be kept active makes it `over`.

## Reasons

Closed codes, as ADR-0087 decision 7: `unknown`, `no_adapter`, `open_failed`, `sessions`, `sessions_unknown`, `throughput`, `software_fallback`, `cpu_load`, `workers`, `render_share`, `disk_unknown`, `disk_full`, `disk_slow`, `ceiling`, `source_inactive`.

- A row at `heavy` or `over` adds its code (`sessions`, `throughput`, `cpu_load`, `workers`, `render_share`, `disk_slow`, `disk_full`, `ceiling`).
- An unknown row adds `sessions_unknown` (hardware sessions), `disk_unknown` (disk rows) or `unknown` (the rest).
- `Resource::calibration_measures` says whether *Test this computer* can measure the resource (`encoder_sessions`, `encoder_throughput`, `iso_workers`, `cpu_load`, `disk_speed`), so the advice is given only there. `no_adapter` and `open_failed` come from the start itself, not from this function.

## At most N

`max_video` and `max_audio` count the total separate tracks of that kind (the plan's own included) that fit before the first resource that grows with them becomes `over`.

- The plan is evaluated with n tracks of the kind, n from 0 to the ceiling: the plan's own tracks first, then copies of the template (video copies take workers in turn).
- Resources that grow with video tracks: `encoder_sessions`, `encoder_throughput`, `iso_workers`, `iso_render_share`, `cpu_load`, `disk_speed` and the video ceiling. With audio tracks: `disk_speed` and the audio ceiling.
- `count` is n − 1 at the first n with such a resource `over`, and `limit` is that resource. When none is `over` up to the ceiling, `count` is the ceiling and `limit` is `iso_ceiling`.
- `partial` is true, and `unknown` lists them, when a growing resource is unknown: the count is then an upper bound. The unknown rows of the step where a known row is `over` are left out of `unknown`: they cannot change the count.
- `known_count` is the largest n such that at every step up to n neither the `encoder_sessions` nor the `cpu_load` row (the two that `separate_video_limit` checks) was unknown; a row the plan does not have counts as known, and other rows do not matter (`iso_render_share` stays unknown until the export measurement). `None` when no step beyond 0 was known.
- `count` and `limit` are `None` when no growing resource other than the ceiling is known.
- No template (no picture or no sound): `count` 0.

## Track costs

Each separate track lists what it adds per resource: a video track one session, its pixel rate, its bytes per second, its export memory and, when measured, its export time over the tick; an audio track its bytes per second.

## The studio's plan

`Studio::start_plan(adapters, collection, request)` builds the `EstimatePlan` of a start (`StartPlanRequest`) next to what runs now, and the `DeclaredAdapter` list of the adapters it names. `Studio::separate_video_plan(adapters, collection, start)` is the same with only the separate video files of `start` added. It reads the studio's state and the registries' probe results only; nothing opens.

- Every running video encoder chain is one `VideoEncode` (`Recording`), at the size it was opened with and the canvas frame rate; every running audio chain one `AudioEncode`. Chains used only by the outputs in `request.replacing` are left out. The running separate files are listed as they opened (their encoder, size and worker), unless `request.replacing_separate`.
- Each added output (`PlannedOutput`: its `OutputSettings` and the encoder the user picked, if any) adds nothing when the start would share a running chain (same canvas, settings and resolved size, an adapter that is a candidate, ADR-0047 decision 7); otherwise one `VideoEncode` (`Recording`) on the picked encoder, else the likely one, and one `AudioEncode` at its bit rate (PCM at its bit depth for a lossless one).
- With `request.sound_files`, each source marked for sound is one `AudioEncode` (`Iso`) of 24-bit PCM at the graph's format, and the audio template is one more.
- The files' encoder is the one the selection would most likely use for the start's settings at the canvas size: the first candidate in registry order that a running chain already uses (it is known to open), else the first candidate. The real selection passes over an encoder that refuses to open, so this is a best guess.
- Each source marked for video (`iso::plan`) is one `VideoEncode` (`Iso`) at its picture size, or the canvas size before it has a picture, on the shared engine workers in turn, holding `DOWNLOAD_BUFFERS` export buffers. Image tracks are listed. The template is one more such file at the canvas size.
- Bytes per second: the bit rate over 8 (VBR: the target); constant quality is planned at 0.1 bit per pixel per frame.
- `adapter.hardware` and `codec.sessions.max` come from the registry's probe; `codec.video.pixel_rate.max` is not a port key yet and is left out.

## Separate video limit

`separate_video_limit(estimate)` is the number of separate video files a start opens; marked sources past it are skipped with the reason `limit`, in source id order.

- It is `max_video.count` when what an encoder can take is known.
- While `encoder_sessions` or `cpu_load` is among `max_video.unknown` (a hardware encoder whose session limit is neither measured nor declared, or a software encoder whose load is not measured), it is `min(count, max(known_count, 3))` (`MaxTracks::unknown_capacity_cap`, with `UNKNOWN_SESSIONS_VIDEO_LIMIT` = 3), and 3 when the count is `None`: the owner's choice of 2026-11-14 (ADR-0087 Review). A measured lower bound raises it above 3 up to what was measured; with no measurement it stays 3. Three 1080p60 files kept the master whole on an M4 Pro with its hardware encoder; four did not.
- A measured or declared session limit decides, above or below 3.
- `separate_video_bound(plan, estimate)` returns the same number with its cause: `Encoder` (the cap of 3, or `encoder_sessions`, `encoder_throughput`, `cpu_load`), `Resource` (another growing resource) or `NoEncoder` (the plan has no video template: no encoder can take a file). The start gate (control API 1.54) refuses or skips by cause.

## The start gate

`gate(plan, limits, marked, request)` decides which marked video sources (source id order) open a file, once, before the take folder is made (control-api "Protocol 1.54", ADR-0087 decisions 5 to 7). It never looks at `disk_space` (`Estimate::gate_level`).

- `GateRequest::Asked { acknowledge_heavy, policy }`: extra files with cause `Encoder` are skipped under `Skip`, refused under `Ask` (`HardwareLimit`); cause `Resource` the same with `OverLimit`; cause `NoEncoder` is refused (`OverLimit`) under both. The kept plan then refuses at gate level `over` (`OverLimit`) and at `heavy` without `acknowledge_heavy` (`Heavy`). `keep` is the exact list of sources that may open.
- `GateRequest::Older`: never refused. Extra files are skipped; with `NoEncoder` every file. Then, per row (resource, scope) that grows with the files, while it is `heavy` or `over` with the kept files and was neither with none, the last kept file is skipped. `count` is how many files open, a later source taking the slot of one without a picture.
- The decision carries the estimate of the kept plan, and for a refusal the plan that was judged and the files concerned.

- Until *Test this computer* runs (step D1) no encoder entry is measured, and no adapter in the tree declares `codec.sessions.max`, so a computer gets 3; after a run it gets the measured number, which may be more or fewer than 3. The Settings card still shows the fixed 3 (`SEPARATE_VIDEO_LIMIT`); the live number comes with the UI step.

## Disk probe

- `DiskProbe::free_space(folder, now)` never waits. It answers `Known(bytes)` when the folder was read less than `CACHE_FOR` (60 s) ago on the caller's clock, `Unknown(reason)` when the last read failed or timed out, and `Pending` otherwise. A missing or stale reading starts a read for the next estimate.
- A read runs on its own short-lived thread, which ends after the one read and is never joined, so a hung network share never holds the control thread. One read runs at a time: while one is running (also after it timed out), no other starts.
- A read that has not answered `PROBE_TIMEOUT` (2 s) after it started makes the folder `Unknown` with a reason (`poll`), until the late answer or a later read replaces it. A failed read is cached like a value, so a missing folder is not read again for 60 s.
- The estimate takes `FreeSpace::bytes()` as `Limits::free_space`; `Pending` and `Unknown` are an unknown disk space row (`disk_unknown`), never `over`.
- Nothing runs between requests: no thread exists unless a read is running (P13).

## calibration.toml

A per-computer file beside `app.toml` in the config directory, outside the `UserData` library: never part of a profile or collection, never exported, imported or synced (`user-data.md`). Read once at app start, with `app.toml`, before the control thread serves anything (`HostConfig::calibration`), and kept until a calibration rewrites it. The estimate uses the entries of the profile's recording folder, never the take folder, which is new on every start; the disk probe reads the same folder.

```toml
format = 1
kind = "calibration"
rbs_version = "0.1.0"        # the RBS version that last wrote it

[[encoders]]
adapter = "nvenc"
version = "560.94"           # the adapter's adapter.version; absent when it reports none
codec = "h264"
size = "1080"                # 720, 1080, 1440, 2160, above-2160
fps = "60"                   # 30, 60, above-60
rate = "cbr"                 # cbr, vbr, cq
sessions = 3
pixels_per_second = 373248000
frame_time_us = 2100
measured_at = 1763150000     # seconds since the Unix epoch
partial = false              # stopped_by is limit, cancelled or error
stopped_by = "late"          # refused, late, ceiling, limit, cancelled, error

[[exports]]
size = "1080"
time_us = 900
measured_at = 1763150000

[[folders]]
path = "D:/Recordings"       # kept in this local file only
write_speed = 412000000      # bytes per second
measured_at = 1763150000
partial = false              # the write stopped early
```

- **Read** (`load`): a missing file is empty. A file that cannot be read, of another `kind` or another `format` is taken as empty with a message, and replaced when a calibration next writes.
- **For the estimate** (`for_estimate(this computer, folder)`): an encoder entry holds when its adapter still probes with the same `version`; an entry without a version holds only while `rbs_version` equals the running version. Export entries hold while `rbs_version` is unchanged. The write speed is the entry of the start's recording folder. Entries with a class name this build does not know are skipped.
- **Before a write** (`prune`): entries that no longer hold are dropped, folder entries whose folder no longer exists are dropped, the newest `MAX_ENTRIES` (64) of each kind are kept, and `rbs_version` is stamped.
- **After a run** (`merge_run`): the file is pruned, then each encoder entry replaces the one with the same key (adapter, version, codec, size, fps, rate), except that an entry with a lower-bound reason (`limit`, `cancelled`, `error`) replaces an old one only when its `sessions` is higher, so a cancelled run never lowers a full measurement. A complete folder entry replaces the old one; a partial one only a partial one. Then the file is pruned again and written.
- Written in one step (a temporary file, then a rename).

## Calibration run

*Test this computer* (ADR-0087 decision 4, step D1, control API 1.55 `calibration.run`) measures what the plan of a start with a recording profile can use on this computer, and merges it into `calibration.toml`.

- **What.** `Studio::calibration_plan` takes the plan of `record.estimate` for the profile and canvas: the master and every separate video file of a marked source. The classes are the distinct (codec, size class, frame-rate class, rate-control class) of its video encodes, the master's first; each class uses the profile's video settings at the canvas frame rate, NV12 in system memory, at the largest planned size of the class. The candidates of a class are the encoders the selection would consider for it (hardware first, `picture_outside` applied; only the picked one when the profile picks one); the one the plan uses is `likely`. Stream encodes are not measured. No graph and no GPU work: frames are pre-filled system-memory frames, the default recording path (ADR-0038).
- **Layout.** As a take: session 1 on its own lane (the master's partition), session `i ≥ 2` on lane `1 + (i - 2) % 2`, the `SHARED_ENGINE_PARTITIONS` (2) shared workers. When ADR-0119 steps (c) and (d) give files partitions of their own, the layout follows. Stream outputs on their own partitions are not modelled, which is conservative.
- **Frames.** A ring of `RING_FRAMES` (8) frames of a moving picture (`moving_ring`: a diagonal gradient that shifts each frame and a bright block moving across it), used in order. It costs an encoder about what a screen or a camera does; noise cost VideoToolbox about 40 % more than any real picture on an M4 Pro (2026-11-15 measurement, the owner's choice in ADR-0087's Review). Every frame differs; timestamps increase per session for the life of the series. Sessions stay open across steps: step `n + 1` opens one more. Frame memory is per class and freed before the next.
- **A step** `n` (1 to `SESSION_CEILING`, 20) opens session `n`, runs a warm-up (unjudged rounds until no session's backlog has grown for `WARMUP_STABLE` (10) rounds, at least `WARMUP_MIN` 10, at most `WARMUP_MAX` 120, each bounded by `WARMUP_ROUND_LIMIT` 1 s; a backlog still growing at the end fails the step as late), then `JUDGED_SECONDS` × fps (2 s of video) judged rounds back to back. Each session's backlog at the end of warm-up is its `L`.
- **A late frame** (decision 4's definition). With period `P = 1 / fps`, the round's duration `d_k` (all waiting inside it included) and `e_{-1} = t_start`: `e_k = max(e_{k-1}, t_start + k·P) + d_k`. A judged round is late when (a) `e_k > t_start + (k + Q)·P` with `Q` = `JITTER_DEPTH` (3), so slack is never banked and a single round of up to `Q·P` passes; or (b) any session holds more than `L + Q` frames not yet drained. The step also fails when (a') the mean `d_k` without the `Q` longest is above `P` (a steady shortfall), or (c) a judged frame older than the newest drained one minus `LOST_WINDOW` (16) frames has no packet (a lost frame, checked again at the next step and after the flush). A step stops at its first late round. Two runs on one machine can differ by one step.
- **`Busy`.** A refused submit drains and retries, yielding, until the frame is accepted, the round can no longer be on time (late), or the stop flag is set. The method runs unpaced, so an encoder that clocks down when idle may measure higher than it runs (decision 10).
- **The entry** of a series: `sessions` the highest passing step, `pixels_per_second` = `sessions × width × height × fps`, `stopped_by` ("Measured encoder entries"), and `frame_time_us`: after step 1, session 1 runs `FRAME_TIME_SETTLE + FRAME_TIME_ROUNDS` (5 + 30) rounds paced at `P` and the mean submit-plus-drain call time of the last 30 is the worker's own cost per frame. A series that does not finish step 1 for a reason other than `late` or `refused` writes no entry; it is `skipped` (`busy` or `unsupported` for a refused first open, `open_failed`, `encode_failed`, `time_limit`, `cancelled`). A lost device ends the whole run `failed` (`device_lost`) and nothing is written.
- **Time.** `CALIBRATION_LIMIT` 60 s for the run; `WRITE_RESERVE` 15 s is kept for the write, which runs last. Pass 1 measures the likely candidate of each class (`FIRST_PASS` 30 s shared equally), pass 2 the other candidates with the rest up to 45 s; unused time passes to the next series. A series whose share is below its estimated step-1 cost (`(WARMUP_MIN + 2 × fps) × frame_time` of the class's first measured series, or 1 s) is skipped with `time_limit`.
- **The folder write** (`measure_write_speed`). Skipped (`no_room`) when the probe's free space is known and below twice `CALIBRATION_WRITE_BYTES`. Left-over `.rbs-calibration-<pid>.tmp` files of other processes older than `LEFTOVER_AGE` (10 min) are removed. Then `.rbs-calibration-<this pid>.tmp` gets 256 MiB of incompressible data in 8 MiB chunks, is synced (`sync_all`, `F_FULLFSYNC` on macOS) and removed in every case; the speed is bytes over the time to the end of the sync. A stop between chunks syncs what was written and records it as `partial` from `PARTIAL_MIN_BYTES` (32 MiB); an unsynced rate is never recorded.
- **In the app.** A worker thread and two session threads, each body inside `rbs_crash::contain`, talk over channels; a panicked or disconnected lane ends the run `failed` (`panicked`). The worker reports to the control thread through its message channel. The host ends a run that has not ended by its deadline (start + 65 s, or 5 s after a cancel) itself, with what it received, and detaches the threads; sessions a hung worker holds stay open until the app restarts. Nothing runs while no test runs (P13), and the run never touches the render loop or a running encoder: it is refused while any output runs and every start is refused while it runs.

## Cost

Nothing runs unless the caller asks. The function is called by the control thread when the UI opens the Separate files card or the Record separately row, when one of their settings changes, and when Record or Go live is pressed (later steps). It touches no hot path.

## Later steps

- C: the desktop app's and the API's picture files take their limit from the estimate (C1, done); `calibration.toml` read at app start, the disk probe in the host, the plan of a whole start and `record.estimate` (C2a, control API 1.53, done). The start gate (`acknowledge_heavy`, `on_hardware_limit`), the profile setting `recording.iso.on_hardware_limit`, the per-file open rules and the manifest's `[[skipped]]` (C2b, control API 1.54, done).
- D1: *Test this computer* (`calibration.run`, `calibration.cancel`, `calibration.status`, topic `calibration.state`, control API 1.55): the encoder sessions and the folder write speed (done; see "Calibration run").
- D2: the export time per size class in a throwaway GPU context and the native-surface path (ADR-0087 decisions 4 and 9); until then `[[exports]]` stays empty and `iso_render_share` unknown after a run.
- E: the render partition's previous-tick time published for the estimate (hot path: measurements and sign-off).
- F: the meter, the sentence, the button and the start confirmation in the desktop app.
