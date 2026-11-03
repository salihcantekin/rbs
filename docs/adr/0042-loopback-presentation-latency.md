# ADR-0042: Loopback audio is delayed by the render endpoint's measured output latency

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

The first Windows recordings (ADR-0040, `docs/specs/recording.md`) had system audio ahead of the picture. Measurements on SC-PC (RTX 4090, 3440x1440 at 120 Hz, output to the monitor over NVIDIA HDMI/DP audio, 2026-11-03), with a clip that flashes white and beeps at the same instant every second, played full screen while recording the display and system audio:

| Player | Audio relative to picture (mean, n) |
| --- | --- |
| Windows Media Player (Media Foundation, presentation clock driven by the audio device position) | -26.3 ms (19 flashes, stdev 1.4) |
| `ffplay` | -33.7 ms (24 flashes; its display rate dropped to about 52 fps) |

Negative means the audio leads. The lead does not depend on the player, so it comes from the capture side. The parts:

1. **Video:** the frame age at each render tick (tick time minus the capture timestamp of the newest frame, `scene_sources` example) is 0.5 to 4 ms on average for the display. RBS's video path adds almost nothing. A 60 fps canvas shows an event at the first tick after it, on average half a frame (8.3 ms) late. That is inherent to sampling at 60 fps (ADR-0037 decision 11 allows up to one frame of skew).
2. **Audio:** WASAPI loopback stamps a packet (`u64QPCPosition`) when the audio engine mixes it, not when the device plays it. A player that syncs to the audio clock shows the picture when the audio is *heard*. A probe on the same endpoint (a shared render stream fed with silence, frames the engine has taken against `IAudioClock`'s playing position) measured **14.8 to 14.9 ms** from mix to play (two runs, sawtooth 10 to 20 ms with the 10 ms engine period). `GetStreamLatency` reported 0, so drivers do not reliably report this value.

So about 15 ms of the 26 ms is the endpoint's output latency, which loopback capture misses. The rest is frame sampling and display composition. The output latency depends on the device (HDMI, USB, Bluetooth and onboard codecs differ by tens of milliseconds), so a fixed constant would be wrong elsewhere. Microphones and cameras have the opposite problem: a camera's video is often later than its microphone, so a user-set per-source offset is needed regardless (ADR-0037 decision 6 already has the channel).

Forces: P3 (no vendor or device branches), P6 and P15 (no new work on the audio thread beyond what the offset already costs), P13 (nothing runs while a loopback source is not Active), the audio input port's invariant 2 (a read never ends after the master time at which it was made, so the port cannot simply stamp loopback audio in the future).

## Decision

1. **The port reports, the graph compensates.** `AudioInputStream` gets `fn presentation_latency(&self) -> Option<Duration>` with a default of `None` (a minor, non-breaking addition, as the port spec foresees for "a latency query"). For `OutputLoopback` and `Application` streams it is how long after their timestamps the audio is presented at the render endpoint, as measured at the last `start`. It is `None` for inputs, in Standby, and when the adapter cannot measure it. Timestamps keep their meaning (the OS time of the audio, never in the future), so the port's invariants are unchanged.
2. **`InputNode` adds it to the sync offset.** After `start` in `prepare`, the node reads the latency once and adds it to the offset from its parameter channel. The sum is clamped as before, to `max_offset` (200 ms by default). The value is applied before the first block, so the resampler's position starts at the compensated time and there is no resync. `InputCounters::presentation_latency_ns` reports it. Per block this costs one saturating add.
3. **WASAPI measures it at `start`.** For a loopback stream the adapter measures the render endpoint's mix-to-play latency on a helper thread while the capture thread opens the stream: a shared-mode render stream on the same endpoint (process loopback: the default render endpoint), fed with silence for 150 ms. It takes the mean of frames taken by the engine minus `IAudioClock`'s playing position (extrapolated from its QPC stamp) after a 40 ms warm-up. The result counts if it arrives within the `start` budget (450 ms in total) and lies in 0 to 500 ms; otherwise there is no compensation. Only silence is played, and nothing runs once `start` returns. Input streams do not measure.
4. **Other adapters report `None` until they measure.** CoreAudio can use the device's `kAudioDevicePropertyLatency` plus safety offset and stream latency, and PipeWire the sink's node latency. Each is a follow-up with its own hardware check.
5. **Manual per-source offsets stay the user's tool** for what software cannot see: monitor-internal audio delay, camera and microphone latency, Bluetooth. The composition roots expose the existing offset channel per audio source (`rbs-cli record` flags; the desktop app's strip settings). This is planned as a follow-up PR after the studio work (PR #91) lands in `rbs-record`, to avoid overlapping changes there.

## Consequences

- On SC-PC, system audio should lead the picture by about 11 ms instead of 26 ms (the remaining frame sampling and composition), within one frame at 60 fps. To be re-measured with the flash and beep clip.
- A loopback stream's `start` takes up to about 150 ms longer, and plays 150 ms of silence on the endpoint once per start. Starting a recording is not a hot path.
- The latency is measured once per start. A device change later in the session (for example Bluetooth headphones connected while recording) is not followed until the next start. Following it would need a render stream running the whole time.
- Audio that the device delays after `IAudioClock`'s position (inside an HDMI sink, a TV's audio processing) stays invisible. The manual offset covers it.
- Hot paths touched: `InputNode::process` reads one more field (P15 numbers and sign-off). The WASAPI capture loop is unchanged.

## Alternatives considered

- **Stamp loopback audio at play time in the adapter.** This breaks the port invariant that a read never ends after the time it was read, which the audio graph and the conformance suite rely on.
- **A fixed default offset for loopback (for example 15 ms).** Correct only for this endpoint. Device latencies differ by tens of milliseconds, and P3 rules out per-device tables.
- **Shift video packets earlier instead (stamp video at tick time minus a latency).** The skew is on the audio side and depends on the audio device, not on the canvas. Video timestamps stay on the canvas cadence (recording spec rule 3).
- **`IAudioClient::GetStreamLatency` on the endpoint.** It reported 0 on the test device, so it is not reliable.
- **Keep a silent render stream for the stream's lifetime and follow the latency continuously.** It costs a render stream for as long as the source is Active and changes the capture thread. Kept in reserve in case device changes during recordings matter.

## Review

Pending: independent review per ADR-0014.
