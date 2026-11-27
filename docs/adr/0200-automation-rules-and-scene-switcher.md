# ADR-0200: Automation rules and the automatic scene switcher

- Status: Proposed
- Date: 2026-11-27
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md) (zero cost when unused), [ADR-0044](0044-control-api-protocol-and-security.md) (control API), [ADR-0158](0158-diagnostics-log.md) (diagnostics log), [ADR-0162](0162-extension-ready-registries-and-plugin-order.md) (extension-ready registries; this ADR is its "automation triggers" registry, step 5), [ADR-0163](0163-one-command-path-for-the-window-and-the-api.md) (one command path)

## Context

The owner wants RBS to switch scenes and run other actions on its own, as OBS users do with the Advanced Scene Switcher plugin: "switch to Game when the game window has the focus", "go to BRB after 10 s of silence on the microphone", "show the speaker who talks", "cycle these scenes every 30 s", "when the stream starts, start recording", "at 20:00 switch to Intro". The owner asked (2026-11-27) what infrastructure this needs and whether it is a scheduler that keeps checking the triggers.

Constraints:

- **P13 / ADR-0013:** with no rules, the feature costs nothing: no thread, hook, timer or subscription. Salih's open Mac idle-CPU issue (a main-thread timer that wakes too often) shows what a polling design costs.
- **P6, P15:** nothing in audio callbacks, `Node::process` or the render tick. Levels may only be read from the lock-free handles the control API's sampling already uses (control API spec, "Threads").
- **One command path:** the Stream Deck plugin (ADR-0198), MCP (ADR-0052) and the window (ADR-0163) all change RBS through control API methods. Automation must not get a private way in.
- **Rule 19 / P16:** trigger types are an automation-trigger registry (ADR-0162 decision 4), stored by type id, settings as an `rbs-params` schema, so a sandbox plugin can add one later.
- No Linux work for the first releases (owner, 2026-11-24): the OS trigger has Windows and macOS adapters only.

OBS's Advanced Scene Switcher polls every condition on a fixed interval (default 300 ms). It is simple, but it wakes the CPU forever and its reaction time is the interval.

## Decision

1. **Event driven, no polling scheduler.** The engine is a pure state machine in a new crate `rbs-automation`: `handle(input, now) -> Vec<Action>` and `next_deadline() -> Option<Instant>`, as the control API session logic is (ADR-0044). Inputs are only: (a) control API state events, (b) level summaries, (c) OS foreground-app changes, (d) its own deadline. It must not loop or sleep on a period; between inputs its thread sleeps on one wake primitive until `next_deadline()`, or forever when there is none. Tests drive it with `TestClock` (ADR-0032).

2. **One cold thread, only while a rule is enabled.** The `automation` thread exists while at least one rule is enabled and automation is not paused. It never runs on the audio, render, UI or control thread. Inputs reach it through bounded queues and latest-value slots; an OS callback only stores the new value and signals the wake primitive.

3. **A rule is: trigger, optional conditions, actions, guards.**
   - **Trigger** (one per rule): a registered trigger type, by id, with its settings. Built-ins (`org.rbs.trigger.*`), phase 1:
     | Id | Fires when | Input |
     | --- | --- | --- |
     | `app-focus` | The foreground app becomes (or stops being) a named app: executable name on Windows, bundle id on macOS, case-insensitive. RBS's own windows are ignored by default | OS adapter (decision 5) |
     | `audio-level` | A source or bus stays below a level for a time (silence) or above it (speaking); thresholds with hysteresis (separate on and off level) and a hold time | Level summary (decision 6) |
     | `state` | A control API state event matches: stream or recording started or stopped, scene changed, a media source ended, a transition finished | Control API events (`stream.state`, `record.state`, `scene.changed`, `media.state`, `transition.state`) |
     | `time` | A time of day (with weekdays), or the program scene has been on air for a duration (scene cycle) | Deadline |
     | `manual` | `automation.run` is called (a macro: a key, a Stream Deck button or MCP can run it) | Control API |
   - **Conditions** (all must hold when the trigger fires): program scene is or is not one of a list; stream or recording is or is not running; a time window. They read the engine's copy of the state from the same events, never a query.
   - **Actions:** an ordered list of control API requests (`method` + `params`) with an optional delay before each. They go through the in-process API path as one session with scopes `read` and `control` only; `admin`, `plugins` and `tokens` methods are refused when the rule is saved and again when it runs. A failed action logs, stops that run and does not stop other rules.
   - **Guards:** per rule a cooldown (default 2 s) and a priority; when several rules fire on the same input, the highest priority runs and the others are skipped for that input. After the user changes the program scene by hand (window, shortcut or any non-automation session), scene-switching rules hold for a set time (default 10 s, 0 = off). An option per rule: "not while live or recording".

4. **Loop guard.** Every request the engine sends is tagged as automation; the state events it causes carry that origin to the engine only. A rule triggered by an event of automation origin is skipped once the chain is 3 deep. Across all rules, more than 20 actions in 10 s pauses automation, raises a warning toast and logs one `warn!(Area::Studio, ...)` line with the rule ids, never the action payloads (rule 20).

5. **Foreground app as a small port.** `rbs-input` gets a second, separate trait `ForegroundWatch` (`start(sink) -> Result<Watch, InputError>`; the sink gets the new foreground app id; `Watch` stops it on drop), independent of the mouse monitor and of Input Monitoring. Adapters: `rbs-input-win` with `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT)`, `rbs-input-mac` with the `NSWorkspaceDidActivateApplicationNotification` observer. Both deliver on the UI thread's existing event loop; the callback reads the app id and signals the automation thread, nothing more, and adds no timer. A watch is started only while an enabled `app-focus` rule exists. Window titles are phase 2 (on macOS they need Screen Recording permission).

6. **Levels at a low rate, only for named sources.** For `audio-level` rules the engine reads the peak and RMS of the sources and buses its rules name, from the same read-only lock-free level handles the API sampling uses, at 10 Hz, on its own deadline, only while such a rule is enabled. The audio thread does not change.

7. **Stored per scene collection, by id.** Rules live in a new document `automation/<collection id>.toml` in `rbs-user-data` (`format = 1`, `kind = "automation"`), exported and imported with its collection and removed with it, so the scene format does not change. Rules name scenes, sources and buses by id. A rule whose trigger type is not registered (a plugin was removed) or whose scene or source no longer exists loads, is shown as not runnable with one sentence, never runs, and is saved back unchanged (ADR-0162 decision 5).

8. **Control API namespace `automation.*`** (next protocol minor, the coordinator assigns the number): `automation.list`, `automation.set` (add or replace a rule, checked against the trigger schema and the action scopes), `automation.remove`, `automation.set_enabled` (one rule), `automation.pause` / `automation.resume` (all), `automation.run` (a `manual` rule), `automation.triggers` (registered trigger types with their schemas); state topic `automation.changed` (rules, paused, last run and last error per rule). Setting methods are `control`; nothing here needs `admin`. The window uses the same methods (ADR-0163).

9. **Registry.** `rbs-automation` owns `TriggerRegistry`, filled at start-up with the built-ins. A `TriggerType` declares its id, name, `rbs-params` schema and the inputs it needs (topics, level keys, foreground, deadline) and evaluates `(state, input, now) -> Fire?` with no I/O. The engine subscribes only to the inputs its enabled rules declare. Actions need no registry: they are control API methods, which already have schemas. `rbs-automation` depends on `rbs-core`, `rbs-params` and `rbs-api` types only; `rbs-app` (and `rbs-cli` headless) wire the thread, the in-process session, the level handles and the `ForegroundWatch` adapter (P1).

10. **Phases.** Phase 1: engine, registry, the five built-in triggers, document, API namespace, tests. Phase 2: the UI (a rules panel and a pause control; its own visual proposal and dark and light screenshots, rule 16), window titles. Later: idle-input trigger over `rbs-input`, plugin triggers through the sandbox tier (ADR-0162 step 3).

## Consequences

- Reaction time is the event's own latency, not a poll interval; idle cost with rules is a sleeping thread, and without rules nothing.
- Automation, Stream Deck, MCP and the window share one command path, so every action is already scoped, logged and covered by API tests.
- The engine needs its own copy of the state it reads, rebuilt from events and the subscribe answers' `seq`; tests must cover reconnect-free resync after a collection switch.
- Holding scene rules after a manual change and the chain limit add rules users must understand; the UI must show why a rule did not run (last error, held, cooldown).
- `rbs-input` gains a trait that is not about input devices; it is kept there to reuse the two OS adapter crates instead of adding two more.
- Follow-ups with phase 1: `docs/specs/automation.md` (new), `control-api.md` (namespace and topic), `user-data.md` (document), `input-port.md` (`ForegroundWatch`), `plugin-sdk.md` "Registries" row for automation triggers, `check-layers` table entry for `rbs-automation`.

## Alternatives considered

- **A polling scheduler (OBS Advanced Scene Switcher).** Simple, but wakes the CPU every interval forever and reacts no faster than the interval; breaks P13.
- **Automation inside the control thread.** No new thread, but trigger evaluation and delayed actions would run on the thread that applies every UI and API request and could delay them.
- **A script runtime (Lua or JS) as the first form.** Most flexible, but a second runtime and sandbox; rules with schemas cover the common cases and WASM triggers come with the plugin loader.
- **Polling the foreground window.** `GetForegroundWindow` or `frontmostApplication` on a timer is easier than hooks, but it is the polling the owner asked about and costs wake-ups.
- **Rules in the scene format.** One file fewer, but a scene format change (P14 contract review) for data that changes on its own schedule.

## Review

Independent review per ADR-0014. Record: reviewer (human handle, or "independent AI session"), date, verdict (approve / changes requested), main concerns and how each was resolved.
