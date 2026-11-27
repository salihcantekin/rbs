# ADR-0200: Automation rules and the automatic scene switcher

- Status: Proposed
- Date: 2026-11-27
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md) (zero cost when unused), [ADR-0044](0044-control-api-protocol-and-security.md) (control API), [ADR-0158](0158-diagnostics-log.md) (diagnostics log), [ADR-0162](0162-extension-ready-registries-and-plugin-order.md) (extension-ready registries; this ADR is its "automation triggers" registry, step 5), [ADR-0163](0163-one-command-path-for-the-window-and-the-api.md) (one command path)

## Context

The owner wants RBS to switch scenes and run other actions on its own, as OBS users do with the Advanced Scene Switcher plugin: "switch to Game when the game has the focus", "go to BRB after 10 s of silence on the microphone", "show the speaker who talks", "cycle these scenes every 30 s", "when the stream starts, start recording", "at 20:00 switch to Intro". The owner asked (2026-11-27) what infrastructure this needs and whether it is a scheduler that keeps checking the triggers.

Constraints:

- **P13 / ADR-0013:** with no rules, the feature costs nothing: no thread, hook, timer or subscription. The open Mac idle-CPU issue (a main-thread timer that wakes too often) shows what a polling design costs.
- **P6, P15:** nothing in audio callbacks, `Node::process` or the render tick.
- **One command path:** Stream Deck (ADR-0198), MCP (ADR-0052) and the window (ADR-0163) change RBS through control API methods; automation gets no private way in, and no new `studio_host::Command` variant.
- **Rule 19 / P16:** trigger and condition types are registered by id with an `rbs-params` schema, so a sandbox plugin can add one later.
- No Linux work for the first releases (owner, 2026-11-24).

OBS's Advanced Scene Switcher polls every condition on a fixed interval (default 300 ms): simple, but it wakes the CPU forever and reacts no faster than the interval.

## Decision

1. **Event driven, no polling scheduler.** The engine is a pure state machine in a new crate `rbs-automation`: `handle(input, now) -> Vec<Action>` and `next_deadline()`, as the control API session logic is (ADR-0044). Inputs are only: control API state events, level samples, foreground-app changes and its own deadline. Between inputs its thread sleeps on one wake primitive until the deadline, or forever when there is none. Tests drive it with `TestClock` and an injected wall clock (ADR-0032).

2. **One cold thread, only while a rule needs it.** The `automation` thread exists while at least one enabled rule has a trigger other than `manual` or an action with a delay, and automation is not paused (a manual rule without delays runs its actions straight from `automation.run`), whether or not remote control is on. It never runs on the audio, render, UI or control thread.

3. **The control thread owns the rules.** `automation.*` methods run on the control thread, which keeps the rule set, writes the document and publishes `automation.changed`. After each change it hands the automation thread a new snapshot of the enabled rules through a latest-value slot. Pending delayed actions of a rule are dropped when the rule is changed or disabled, automation is paused, or the collection switches.

4. **A rule is: trigger, conditions, actions, guards.**
   - **Trigger:** one registered trigger type (`org.rbs.trigger.*` built-ins, phase 1): `app-focus` (the foreground app becomes or stops being a named app: executable name on Windows, bundle id on macOS; RBS itself ignored by default), `audio-level` (a source or bus stays below or above a level for a hold time, with separate on and off levels), `state` (a control API state event matches: stream or recording started or stopped, program scene changed, transition finished), `time` (a time of day on chosen weekdays, or the program scene has been on air for a duration) and `manual` (`automation.run`, so a key, Stream Deck button or MCP can run a rule as a macro). Details in `docs/specs/automation.md`.
   - **Conditions:** zero or more registered condition types (`org.rbs.condition.*`: program scene is or is not one of a list, stream or recording running or not, time window), all of which must hold. They read the engine's copy of state rebuilt from the same events, never a query.
   - **Actions:** an ordered list of control API requests (`method` + `params`), each with an optional delay. A failed action logs, stops that run and does not stop other rules.
   - **Guards:** per-rule cooldown (default 2 s) and priority; when several rules fire on one input, the highest priority runs, ties broken by rule order in the list, the rest are skipped. After a program-scene change whose request did not come from automation, scene-switching rules hold for a set time (default 10 s, 0 = off). A per-rule option: not while live or recording.

5. **Actions enter as their own caller, never with more than the rule's author has.** The automation thread pushes requests into a third request queue of the control thread (beside the UI and API queues, behind the same wake-up). `handle_as` checks no scopes today (sessions do, on the API thread), so this queue's drain must check each method's scope itself before `handle_as`, and the `Caller` gets an automation origin marker as a separate field, never a token name a user could also choose. A rule whose author is a token runs as a token caller for every token-only check (`local_only` today, ADR-0149, and any later one); only a rule authored by "this computer" passes them. `automation.set` receives its caller (today `ApiTarget::handle_as` drops it before routing), and the rule stores its author: the token name, or "this computer" for the window and same-user IPC. At run time a rule runs with the intersection of `read` + `control` and its author token's current scopes and irreversible flag; a revoked or missing token makes the rule not runnable. `admin`, `plugins`, `tokens`, `ui.*` and `automation.*` methods are refused as actions at save and at run, so rules cannot change tokens, drive a window that has no client, or make rules.

6. **Origin and loop guard.** The engine is a second local event subscriber on the control thread (it gets a starting snapshot when its thread starts, then events); the publisher records the origin of the command being applied, so events the engine receives carry the `Caller` of the command being applied when they come from it synchronously. A rule triggered by an automation-origin event is skipped once the chain is 3 deep. Effects that come later (a transition ending) carry no origin, so the backstop is the global limit: more than 20 actions in 10 s pauses automation, shows a warning toast and logs one line with the rule ids, never action payloads (rule 20).

7. **Foreground app as a small port.** `rbs-input` gets a second trait `ForegroundWatch` (`start(sink) -> Result<Watch, InputError>`, stop on drop), separate from the mouse monitor and from Input Monitoring. `rbs-input-win` installs `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT)` on its own thread with a blocking `GetMessageW` loop (no timeout). `rbs-input-mac` registers a `NSWorkspaceDidActivateApplicationNotification` observer, which AppKit delivers on the main run loop: in the app that is the UI thread's existing loop, with no added timer and no polling loop (adds `objc2-app-kit` to that adapter crate; the delivery thread is checked on a Mac before phase 1 lands). The callback reads the app id, stores it and signals the automation thread. A watch runs only while an enabled `app-focus` rule exists. `rbs-cli` on macOS has no main run loop, so `app-focus` is not offered there. Window titles are phase 2 (on macOS they need Screen Recording).

8. **Levels through one reader.** `LevelMeter::observe` resets its slot, and today only the API thread's `Sampler` calls it, only while remote control or AI tools are on. Phase 1 moves that reading into one level reader that exists while either an API subscription or an enabled `audio-level` rule needs it (on the automation thread when the API thread is off; the spec defines peak and RMS per consumer); it calls `observe` once per tick and fans the values out (the API at its rate, automation at 10 Hz, peaks kept as the maximum since each consumer's last read). An enabled `audio-level` rule counts as a meter subscriber in `sync_meters`, which keeps the audio branch awake (ADR-0169); that is part of its Active cost. Rules can name only sources that are heard (program scene and globals, what `LevelFeed` holds) until bus meters are exposed. The audio thread does not change; a separate reader slot instead would be a hot-path change with P15 numbers.

9. **Time from the wall clock.** `time` triggers compute their next deadline from wall time. While such a rule exists the engine sleeps at most 15 minutes, re-plans from wall time on every wake (sleep, daylight saving or clock change are caught on the next wake), runs a time of day once per day and skips it when it was missed by more than 60 s.

10. **Stored per scene collection, by id.** Rules live in `automation/<collection id>.toml` in `rbs-user-data` (`format = 1`, `kind = "automation"`), removed with the collection, so the scene format does not change. Export and import of rules wait for a collection bundle format (its own change); rules that arrive by any future import or by OBS import are disabled until the local user enables them. Rules name scenes, sources and buses by id. A rule whose trigger or condition type is not registered, or whose scene or source no longer exists, loads, is shown as not runnable with one sentence, never runs and is saved back unchanged (ADR-0162 decision 5).

11. **Control API namespace `automation.*`** (next protocol minor, the coordinator assigns the number): `automation.list`, `automation.set` (add or replace, checked against the schemas and decision 5), `automation.remove`, `automation.set_enabled`, `automation.pause` / `automation.resume`, `automation.run`, `automation.types` (registered trigger and condition types with their schemas); state topic `automation.changed` (rules, paused, last run, last error and why a rule did not run, per rule). All `control`. The window uses the same methods (ADR-0163).

12. **Registry and layers.** `rbs-automation` owns `AutomationRegistry` of trigger and condition types, filled at start-up with the built-ins. A type declares id, name, `rbs-params` schema and the inputs it needs (event topics, level keys, foreground, deadline), and evaluates `(state, input, now)` with no I/O; the engine subscribes only to inputs its enabled rules declare. Actions need no registry: they are control API methods with schemas. A plugin type would get its inputs as control API state events, which the plugin SDK host services gain as a read-only service when the loader comes. `rbs-automation` depends on `rbs-core`, `rbs-params` and `rbs-api` only (a control-adjacent class in `check-layers`); `rbs-app` and `rbs-cli` wire the thread, queue, sampler and adapter (P1). Logging uses a new `Area::Automation` (rule ran: `debug`, refused or failed: `warn`, paused by the limit: `warn`).

13. **Phases.** Phase 1: engine, registry, built-in types, document, API namespace, tests. Phase 2: the UI (rules panel, pause control; its own visual proposal and dark and light screenshots, rule 16) and window titles. Later: media-ended trigger (needs a `media` state topic), idle-input trigger, plugin types (ADR-0162 step 3), rule export with a collection bundle.

## Consequences

- Reaction time is the event's own latency. Cost: Installed (no enabled rule) nothing; Active a sleeping thread, plus per enabled input only what it needs: one OS hook; 10 Hz level reads, with meters attached and the audio branch awake; at most 4 wake-ups an hour for time rules. Phase 1 measures idle wake-ups per second with and without rules on Mac and Windows.
- Automation, Stream Deck, MCP and the window share one command path, so actions are scoped, logged and covered by API tests; the third request queue and the second local subscriber are small control-thread changes (control API "Threads" table).
- The engine keeps its own state copy from events and `seq`; tests cover resync after a collection switch.
- Holds, cooldowns and the chain limit are rules users must understand; the UI must show why a rule did not run.
- `rbs-input` gains a trait that is not about input devices; it reuses the two OS adapter crates instead of adding two more.
- Follow-ups with phase 1: `docs/specs/automation.md` (new, with the type list), `control-api.md` (namespace, topic, Threads table), `user-data.md` (document), `input-port.md` (`ForegroundWatch`), `plugin-sdk.md` "Registries" row, `diagnostics-log.md` (area and levels), `check-layers` class for `rbs-automation`.

## Alternatives considered

- **A polling scheduler (OBS Advanced Scene Switcher).** Simple, but wakes the CPU every interval forever and reacts no faster than it; breaks P13.
- **Automation on the control thread.** No new thread, but evaluation and delayed actions would delay every UI and API request.
- **Actions with the window's `Caller::local()`.** No new queue, but all scopes and no author limit: a remote token could store a rule that escapes its own scopes.
- **A script runtime (Lua or JS) first.** Most flexible, but a second runtime and sandbox; typed rules cover the common cases and WASM types come with the plugin loader.
- **Polling the foreground window.** `GetForegroundWindow` or `frontmostApplication` on a timer is simpler than hooks, but it is the polling the owner asked about.
- **Rules in the scene format.** One file fewer, but a scene format change (P14) for data that changes on its own schedule.

## Review

Reviewer: independent AI session (given only the ADR, principles, specs, ADRs and code)
Date: 2026-11-27
Verdict: changes requested (round 1); design direction confirmed
Concerns:
1. No scoped in-process session exists; `Command::Api` runs as `Caller::local()` -> decision 5: a third request queue with `handle_as` and an automation `Caller`.
2. A `control` token could store a rule that runs with more rights -> decision 5: rules keep and are limited by their author's scopes and irreversible flag.
3. Imported rules would run someone else's actions -> decision 10: imported rules arrive disabled.
4. Collection export writes one document -> decision 10: rule export waits for a bundle format.
5. `LevelMeter::observe` resets its slot; meters detach without subscribers -> decision 8: one shared sampler, a rule counts as a meter subscriber, audio thread unchanged.
6. Events carry no origin -> decision 6: second local subscriber with the applying `Caller`, sync effects only, global limit as backstop.
7. `media.state` is a query, not a topic -> media-ended trigger moved to later.
8. Foreground adapters: own thread pattern, headless, AppKit dependency -> decision 7.
9. Rule ownership across threads -> decision 3.
10. Time of day cannot be an `Instant` deadline -> decision 9.
11. Conditions were a closed list -> condition types in the registry (decisions 4 and 12).
Nits fixed: `Area::Automation` (a follow-up), zero-cost costs and measurement in Consequences, priority tie-break, pending delayed actions, `check-layers` class, type details moved to the spec.

Reviewer: independent AI session (fresh, round 2)
Date: 2026-11-27
Verdict: changes requested (round 2); round-1 resolutions 3, 4, 7, 9, 10, 11 confirmed against the code
Concerns:
1. `handle_as` checks no scopes, `tokens.*` is answered before caller checks, the caller is dropped before routing, a stored `Caller` is a snapshot -> decision 5: the drain checks scopes, `automation.set` gets the caller, the rule stores its author token and is limited by that token's current scopes; revoked token -> not runnable.
2. No shared sampler; the API thread owns it; `LevelFeed` has no buses; meters keep the audio branch awake -> decision 8: one level reader fanning out, heard sources only, audio branch cost listed.
3. macOS workspace notifications arrive on the main run loop; the port's 2 s run-loop pattern is a timer -> decision 7: main run loop, no timer, not offered in `rbs-cli` on macOS, delivery checked on a Mac.
Nits fixed: origin marker instead of a token name; `automation.*` and `ui.*` refused as actions; starting snapshot; publisher records origin; manual-only rules need no thread.

Reviewer: independent AI session (fresh, round 3)
Date: 2026-11-27
Verdict: changes requested (round 3), one blocking concern; round-2 resolutions confirmed against the code, no break of P1, P6, P13, P15 or rule 19
Concerns:
1. A caller with no token passes `check_local_only`, so a token-authored rule could run `browser.run_script` -> decision 5: token-authored rules run as a token caller for every token-only check; the origin marker is a separate field.
Nits: who owns the level reader with the API thread off and RMS per consumer -> named in decision 8, details in the spec; `set_meters_on` also calls `observe` to clear, and the third queue's order in the Threads table -> spec follow-ups.
