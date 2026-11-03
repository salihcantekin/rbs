# ADR-0042: Loopback audio is delayed by the render endpoint's measured output latency

- Status: Rejected
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

The first Windows recordings (ADR-0040, `docs/specs/recording.md`) had system audio ahead of the picture. Measurements on SC-PC (RTX 4090, 3440x1440 at 120 Hz, output to the monitor over NVIDIA HDMI/DP audio, 2026-11-03) used a clip that flashes white and beeps at the same instant every second, played full screen while RBS recorded the display and system audio. The analysis (`avsync.py`) found the flash onset from per-frame luma and the beep onset with a matched filter. On the clip itself both onsets coincide.

| Player, build | Audio relative to picture (mean, n) |
| --- | --- |
| Windows Media Player (Media Foundation, presentation clock driven by the audio device position), main | -26.3 ms (19), repeat -29.5 ms (24, stdev 0.3) |
| `ffplay`, main | -33.7 ms (24; its display rate dropped to about 52 fps) |

Negative means the audio leads. The lead does not depend on the player.

- **Video side:** the frame age at each render tick (tick time minus the capture timestamp of the newest frame, `scene_sources` example) is 0.5 to 4 ms on average for the display. The canvas is stamped with the tick time, and export and encoder keep that timestamp. A 60 fps canvas shows an event at the first tick after it, on average half a frame (8.3 ms) late.
- **Audio side:** a probe on the endpoint (a shared render stream fed with silence, frames the engine has taken against `IAudioClock`'s playing position) measured a 10 to 20 ms sawtooth, mean 14.8 ms. `GetStreamLatency` reported 0.

The hypothesis was that WASAPI loopback stamps a packet (`u64QPCPosition`) when the engine mixes it, so its timestamps are earlier than the sound by that ~15 ms.

## Proposed decision

1. `AudioInputStream::presentation_latency() -> Option<Duration>`, default `None`. For loopback streams it is the render endpoint's output latency, measured at `start`.
2. `InputNode` adds it to the source's sync offset.
3. WASAPI measures it at `start` with a short silent render stream.
4. Manual per-source offsets stay available for what software cannot see.

A first implementation (PR #92, commit fa63634) moved the WMP result from -29.5 ms to -18 / -12 ms (two runs) and the `ffplay` result from about -30 to -11.5 ms (the usual value).

## Why it was rejected

The premise is false. A click experiment measured directly what a loopback timestamp means, on SC-PC 2026-11-03:

- A shared render stream and a loopback capture ran on the same idle HDMI endpoint (NVIDIA HDMI audio, 10 ms period, the same driver as the default endpoint).
- A one-sample click at -60 dB was written every 500 ms at a known frame of the render stream.
- Play time: when the render stream's `IAudioClock` position reaches that frame.
- Loopback time: the click's position in the loopback data, from its packet's `u64QPCPosition`.
- Over 2 x 18 clicks, play time minus loopback time was **0.19 ms and 0.21 ms** (stdev 0.007 ms).
- In the same runs, the sawtooth read 11 to 21 ms (mean 16.1 ms).
- Loopback packets were all 480 frames, stamped one engine period apart (±20 µs).

So loopback timestamps are already the device's play time on the clock that players sync to. The sawtooth measures how long frames wait in the engine before that clock reaches them, a wait that loopback stamps have already removed. Delaying loopback audio by it would put the audio about 15 ms after its true time. The improvement seen with PR #92 only offset part of some other difference.

Both sides of RBS's capture timestamp honestly: audio at play time, video at composition time with a frame age of a few milliseconds. The remaining lead is about 8 ms of 60 fps sampling, which ADR-0037 decision 11 allows, plus a difference between the player's audio clock and when its picture is composed. That second part is likely present and display latency that players do not compensate. This is an inference that was not measured. It belongs to the playback system, so a viewer of the screen hears and sees the same offset. Correcting it in RBS would record what the player intended, not what the screen showed.

## Consequences

- No port, graph or adapter change. PR #92's code is dropped.
- The measurements and the click method are recorded here and in the recording spec, for future A/V questions.
- Per-source manual sync offsets (the existing ADR-0037 channel) are exposed in the composition roots for device latencies software cannot see (cameras, microphones, Bluetooth). That is a separate change in `rbs-cli` after the studio work (PR #91).

## Alternatives considered

- A fixed default delay for system audio, to match typical players: device- and player-dependent, and not principled.
- Following the endpoint's latency continuously: moot, since there is nothing to compensate.

## Review

- Independent AI session, 2026-11-03, on the proposal and PR #92 (given the ADR, specs and diff only): changes requested, 14 items. The central item questioned what the loopback stamp refers to and asked for that to be verified before acceptance (the reviewer expected the lower envelope, about 10 ms, rather than the mean). The click experiment above showed that neither was right: the stamp is already play time. The remaining items concerned the implementation and are moot with its removal.
- Owner decision: see the thread record of 2026-11-03.
