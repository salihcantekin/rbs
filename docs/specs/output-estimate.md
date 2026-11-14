# Output cost estimate

Status: Draft. Decision: [ADR-0087](../adr/0087-output-cost-estimate-and-hardware-limits.md) (Accepted). This spec covers the estimate function (step A); the probe, the calibration, the control API and the UI are later steps, listed at the end.

## Purpose

Say before a start what it will cost on this computer, which resource sets the limit, and how many separate video and audio tracks fit, so a take does not fail or fall to a CPU encoder halfway through. An unknown limit is reported as unknown, never guessed.

## Parts and layers

| Part | Crate | Notes |
| --- | --- | --- |
| `estimate()`, `EstimatePlan`, `Limits`, `Estimate` | `rbs-record` (`estimate`) | Pure: no clock read, no OS call, no thread, no adapter. Tested with synthetic plans |
| Plan building from the output profile and the collection | `rbs-app` (later step) | Resolves the adapter each encoder would use and its declared capabilities |
| Disk probe, `calibration.toml`, calibration run | later steps | ADR-0087 decisions 1 and 4 |

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
| `encoder_sessions` | adapter | `sessions` | sessions on the adapter | measured sessions (the lowest of the classes used), else declared `codec.sessions.max`; absent on hardware is unknown, on software unlimited (level `fits`) | count |
| `encoder_throughput` | hardware adapter | `pixels_per_second` | sum of width × height × fps | measured pixel rate per class, else declared `codec.video.pixel_rate.max`; mixed classes give the limit at which the same share is used | rate |
| `cpu_load` | none | `pixels_per_second` | software video encoders' pixel rate | measured pixel rate of the software encoders; unknown without a calibration | rate |
| `iso_workers` | none | `ratio` | the busiest ISO encode worker: sum of frame time × fps of its tracks | 1.0; unknown without a calibration of every track's class | rate |
| `iso_render_share` | none | `ratio` | (previous tick + export time per track of its size class) / tick | `give_way_share`; unknown until both the previous tick and the export times are known | rate |
| `memory` | none | `bytes` | width × height × 4 × export buffers per ISO video track | none | informational |
| `disk_speed` | none | `bytes_per_second` | sum of all encoders' bytes per second | measured write speed | rate |
| `disk_space` | none | `bytes` | bytes per second × planned duration | free space | `fits` or `heavy` only |
| `iso_ceiling` | `video`, `audio` | `tracks` | separate tracks of that kind | `MAX_ISO_VIDEO` 16, `MAX_ISO_AUDIO` 32 | count |
| `active_sources` | none | `sources` | sources kept active | none | informational |

`iso_workers`, `iso_render_share` and `memory` are present only when the plan has separate video tracks; `encoder_throughput` only for hardware adapters; `cpu_load` only when a video encoder is software.

## Levels

- `HEAVY_SHARE` = 0.7.
- Count: `fits` when need ≤ limit, `over` when need > limit.
- Rate: `fits` when need < 0.7 × limit, `heavy` when 0.7 × limit ≤ need ≤ limit, `over` when need > limit.
- Disk space: as a rate, but `over` is reported as `heavy`; a disk with no room for the start itself is found when files open.
- Unknown limit: level `unknown`, never `over`.
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
- `partial` is true, and `unknown` lists them, when a growing resource is unknown: the count is then an upper bound.
- `count` and `limit` are `None` when no growing resource other than the ceiling is known.
- No template (no picture or no sound): `count` 0.

## Track costs

Each separate track lists what it adds per resource: a video track one session, its pixel rate, its bytes per second, its export memory and, when measured, its export time over the tick; an audio track its bytes per second.

## Cost

Nothing runs unless the caller asks. The function is called by the control thread when the UI opens the Separate files card or the Record separately row, when one of their settings changes, and when Record or Go live is pressed (later steps). It touches no hot path.

## Later steps

- B: the disk probe (2 s timeout, 60 s cache per folder) and `calibration.toml` (format 1, beside `app.toml`).
- C: `record.estimate`, the start gate (`acknowledge_heavy`, `on_hardware_limit`), the profile setting `recording.iso.on_hardware_limit`, and the plan built by the host; the estimate replaces the fixed limit of 3 separate video files.
- D: *Test this computer* (`calibration.run`, `calibration.cancel`, `calibration.status`, topic `calibration.state`).
- E: the render partition's previous-tick time published for the estimate (hot path: measurements and sign-off).
- F: the meter, the sentence, the button and the start confirmation in the desktop app.
