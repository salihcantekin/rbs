# Recording protection

Status: Draft. Decision: [ADR-0090](../adr/0090-recording-protection-disk-lag-and-adaptive-bitrate.md) (Proposed). This spec covers phase one; later phases are listed at the end.

## Purpose

Warn early, while a take runs, when a disk a recording writes to is about to fill, when an output loses frames, when the render or audio worker is late, or when system memory is low; and end a recording cleanly before its disk is full. Nothing about it runs while nothing records or streams.

## Parts and layers

| Part | Crate | Notes |
| --- | --- | --- |
| `Guard` (pure state machine), `ProtectionSettings`, `Sample`, `GuardStatus` | `rbs-record` (`protection`) | No OS call, no clock read, no thread. Tested with synthetic samples |
| `SystemProbe` port, `ProbeWorker` (one worker thread, request cadence, timeout) | `rbs-record` (`protection`) | Takes the caller's master-clock time |
| OS readings (`volume_space`, `available_memory`) | `rbs-system-probe` (adapter) | Plain functions; `unsafe` FFI with `// SAFETY:` comments; no other RBS crate |
| Host wiring, `protection.status`, `protection.changed` | `rbs-app` (`studio_host::protection`) | `SystemProbeFactory::os()` in the app, a fake in tests |

## Lifetime and cost

- A guard is created by the studio host when the first recording or stream output of any canvas runs, and dropped in the first host round after the last one ended. The replay buffer and the virtual camera do not create one.
- While a guard runs, the host steps it once a second on the master clock through the control loop's existing `wait`; no timer of its own.
- One probe worker thread per guard. A request (every recording folder, then memory) goes out every 5 s (`PROBE_INTERVAL`) and is taken on a later step without waiting. A request unanswered after 10 s (`PROBE_TIMEOUT`) makes disk and memory `unavailable` (reported once per request); later requests are skipped and counted until it answers. Dropping the worker closes its channel and joins it only when no request is pending, so a hung drive never holds the control thread.
- No hot-path structure is written. The guard reads `Studio::output_counters` (atomics the nodes already keep).

## Inputs

Per running output, from `Progress` counted from the output's start:

- frames lost = `export_drops + encoder_skipped + encoder_drops`, plus `transport_drops` for a stream;
- `late_render_ticks` and `late_audio_ticks` (worker-wide, the same for every output; reported as `render` and `audio`, not per output).

`backlog_peak` is not used (a run-long maximum). Per probe answer: each recording folder's volume key and free bytes, and available memory.

## Rules (defaults in `ProtectionSettings::default()`)

| Setting | Default | Meaning |
| --- | --- | --- |
| `notice_minutes` | 30 | Disk `notice` below this much time left |
| `warning_minutes` | 3 | Disk `warning` below this much time left |
| `reserve_bytes` | 1 GiB | Disk `critical` at or below this free space |
| `at_critical` | stop cleanly | Or warn only |
| `lag_limit` | 1 per second | Lost frames per second above which an output lags |
| `lag_seconds` | 3 | Seconds in a row to start and to stop lagging |
| `min_memory_bytes` | 1 GiB | Memory `warning` below this |
| `rate_window` | 60 s | Free-space readings the write rate is measured over |

- **Disk, per volume.** The write rate is the fall of free space between the oldest and newest readings in the window, with at least three readings; with fewer, or when free space does not fall, there is no time left estimate and only `critical` can be reached. Time left = (free − reserve) / rate. A level falls back only with a margin: 20 % more time left than the threshold, or free space above the reserve plus 10 %. A volume no running recording uses is forgotten.
- **Critical, stop cleanly.** Every recording on that volume (any canvas) is asked once to end through the host's normal stop (`end_recording`, then `stop_recording` when the tail is in, ADR-0045). Its separate audio files end with it. The user gets the sentence at once through the app's error notice, and the recording's summary `failure` (`record.state` `last.failure`) is that sentence. Streams are never stopped by the guard.
- **Lag.** Per output and for `render` and `audio`: lagging after `lag_seconds` of seconds above `lag_limit`, cleared after `lag_seconds` at or below it. Level `warning`. Phase one only warns: the shedding steps of ADR-0090 decision 4 are inert.
- **Memory.** `warning` below the minimum, cleared above it plus 10 %. macOS reports `unavailable` in phase one.
- **Telling the user.** Each item that reaches `warning` or `critical` (or a higher level than before) is written to the developer log and shown once with the existing error notice; a protection stop is told by its own sentence. `notice` is only in the API state. No new visual element in phase one.

## Control API (protocol 1.49, provisional; the minor is set at merge)

`protection.status` (scope `read`) answers a snapshot (`seq` of `protection.changed`):

```json
{"guarded": true, "level": "critical",
 "disks": [{"level": "critical", "free_bytes": 429496729, "recordings": ["main"]}],
 "outputs": [{"canvas": "main", "output": "record", "level": "ok"}],
 "render": "ok", "audio": "ok",
 "memory": {"level": "unavailable"},
 "alerts": [{"kind": "disk", "level": "critical",
             "text": "Recording stopped before the disk was full: 0.4 GiB left on the disk."}]}
```

- `level` values: `unavailable`, `ok`, `notice`, `warning`, `critical`. A client treats an unknown level as `warning`. The top-level `level` is the highest of everything; `unavailable` never raises it.
- `kind` values: `disk`, `lag`, `render`, `audio`, `memory`. `canvas` and `output` are set on a `lag` alert.
- `free_bytes` and `available_bytes` are rounded down to a tenth of a GiB and `seconds_left` to whole minutes, so the state changes about once a minute, not every second. Optional fields are absent when unknown.
- `alerts`: every item at `notice` or above, and a disk that could not be read, most severe first.
- `guarded` is false and the lists empty while no guard runs.
- `protection.changed` (state topic, scope `read`, no object id) carries the same object whenever it changes. Nothing is published while no guard runs and the state is unchanged.
- There is no method to change the settings or to turn the guard off in this version.

## Tests

- `rbs-record` `protection::guard::tests`: every rule above on synthetic samples and explicit timestamps (no clock, disk or sleep).
- `rbs-record` `protection::probe::tests`: cadence, timeout reported once, skipped requests, a late answer, a drop over a hung fake probe that returns at once.
- `rbs-system-probe`: the temporary folder has a volume and free space; a missing folder fails; memory is read or unsupported; `/proc/meminfo` parsing.
- `rbs-app` `tests/studio_record.rs` (`a_recording_stops_cleanly_with_the_reason_when_its_disk_reaches_the_reserve`): a fake disk below the reserve; the user is told, `protection.status` shows `critical` with the main recording, the recording stops with the sentence as its summary, the file stays, and the guard is gone afterwards. The engine runs on a `TestClock`; real time bounds only as a hang guard.

## Later phases (not in this tree)

The `[protection]` section of the output profile and its settings card, `settings.get/set` for it, `protection.acknowledge`, the live-indicator line and toasts for every level, events in the finish report, markers (ADR-0089), shedding separate video tracks, adaptive bit rate, the replay buffer evicting earlier under memory pressure, a per-interval backlog counter, macOS available memory, and `rbs-cli` wiring.
