# ADR-0149: Browser page automation, zoom, background pages and load-error retry

- Status: Proposed
- Date: 2026-11-18
- Deciders: Salih (owner)
- Builds on: [ADR-0052](0052-ai-agent-control-through-mcp.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md), [ADR-0109](0109-browser-source.md), [ADR-0132](0132-browser-engine-css-interaction.md), [ADR-0143](0143-performance-rules-for-every-change.md), [ADR-0145](0145-add-ons-one-place-to-download-and-install.md)
- Changes: ADR-0132 decision 4 ("no other script is ever injected") for the user's own script only (decision 3), and the length of its new-document hold (decision 6); ADR-0107 decision 8 (no keep-warm override) for browser sources only, as that decision allows (decision 5); ADR-0052 decision 4 gains one `not_permitted` reason, `local_only` (decision 7)

## Context

ADR-0109 and ADR-0132 give a browser source parity with the usual tools: address, size, frame rate, transparency, custom CSS, reload, Interact, sign-in. Users of overlays and dashboards ask for more, and the usual tools need extra plugins or tricks for it (owner, 2026-11-18):

- **Widgets that stall.** Chat, alerts and score pages lose their socket and freeze. Users reload them by hand or with a scene-switcher plugin.
- **Small page fixes need script.** Click a "dismiss" button, hide an element that CSS cannot reach, poke a page that only updates on a timer. CSS alone cannot do it, and the usual tools have no script setting.
- **Pages made for another size.** A dashboard built for 1280 wide looks tiny at 1920. Users write `zoom` CSS, which some pages break.
- **State lost on a scene switch.** ADR-0107 stops a hidden page (no process, P13). A chat overlay then starts empty, and a timer starts over, each time its scene comes back. The usual tool keeps every hidden page running by default, which costs CPU and memory for every page the user ever placed.
- **Error pages on air.** When the network blips, the engine draws its own error page into the program until someone reloads.

The page is a picture with no binding into RBS (ADR-0109 decision 10). A page can fetch the network, so anything that runs code in it needs the network permission (ADR-0052). Every feature must cost nothing while off (P13, ADR-0143).

## Decision

Decisions 2 to 6 are independent features. The owner may accept, reject or later supersede each one alone; decisions 1, 7, 8 and 9 then keep only the parts of the features that stand.

1. **New saved settings of `org.rbs.source.browser`,** all off by default except `retry_on_error`, added at the next free scene-format version (confirmed at merge; an older file reads them as absent, which is the default). Each is set with `source.set_settings` and shown on the source's Page tab, so the UI and the control API have the same reach (U20).

   | Key | Type and range | Default |
   | --- | --- | --- |
   | `reload_every_s` | integer, 0 (off) or 10..=86400 | 0 |
   | `script` | UTF-8 text, at most 32 768 bytes | empty |
   | `script_on_load` | bool | false |
   | `script_every_s` | integer, 0 (off) or 1..=86400 | 0 |
   | `zoom_percent` | integer, 25..=500 | 100 |
   | `keep_running` | bool | false |
   | `retry_on_error` | bool | true |

   A value out of range is `invalid_params`. A change while the tree runs reaches the runner over IPC without a restart. `keep_running` takes effect at once in both directions (decision 5).

2. **Periodic reload.** With `reload_every_s` set, the runner reloads the main frame that many seconds after the last finished load, the same way as `browser.reload` (ADR-0132 decision 5): no new process, no new slot. Across every reload the program keeps the last good picture until the new document has its first paint with the CSS and zoom applied, for at most 2 seconds; a page that has not painted by then is shown as it is. The timer is paused while a load error waits for its retry (decision 6). The timer lives in the runner and exists only while the tree runs; the studio adds no thread or timer. The help states that a reload also resets the page state that `keep_running` keeps.

3. **The user's own script.**
   - **When it runs.** `script` runs in the main frame, in the page's own script world, with the page's own rights: once after each finished main-frame load, after the CSS is applied, when `script_on_load` is true; every `script_every_s` seconds while the page is loaded, counted from the end of the load, when that is not 0; and once on `browser.run_script` (decision 7). An edit of `script` never runs it at once: the new text is used from the next load or tick.
   - **Only on its own origin.** It runs only while the main frame's origin equals the origin of the saved `url` (for a `file` URL: a document inside the directory ADR-0109 decision 10 allows). A sign-in page opened through Interact, a redirect target, or a link the page followed to another origin never runs it. The origin match is a pure function in `rbs-browser-ipc`.
   - **No binding.** The page still gets no binding, no RBS API and no reply channel. The runner passes the text unchanged to the engine (`ExecuteJavaScript`) and discards the result; it reports no script errors, so no handler is added to the renderer. Iframes are not run. ADR-0132's fixed CSS script stays as it is; this is the only other script the runner may inject, and it is always text a local user or a permitted client wrote.
   - **Who may write it.** Writing `script`, `script_on_load` or `script_every_s` needs the network permission (ADR-0052 decision 4, the irreversible-actions permission), like `url` and `css`, because a script can fetch. A script on a `keep_signed_in` source runs inside a signed-in account, so that combination is local-user only: a write is refused with `not_permitted`, reason `local_only`, when the caller is a token (any token, whatever it holds; ADR-0052's caller context tells the desktop app and the IPC local user apart from tokens) and the source would afterwards have a non-empty `script` with a run setting on (or a `browser.run_script` with a `script` argument) together with `keep_signed_in` true. The check runs on the result of every write of `script`, `script_on_load`, `script_every_s`, `keep_signed_in` and `url`, so no order of writes gets around it, and a token may not move the `url` of such a source to another origin either.
   - **Exact origin.** The help says that the match is on the exact origin (`example.com` and `www.example.com` differ). The source status reports the last outcome, `script: "sent" | "skipped_origin"` ("sent" means handed to the engine, not that it ran to the end), which needs no renderer handler.
   - **Imports and reads.** An OBS import never sets them (OBS has no such setting). A collection imported from a file, or a source pasted from another collection, keeps `script` but turns `script_on_load` and `script_every_s` off; the Page tab says so, and the user turns them on again. `source.get_settings` returns `script` only to sessions with the `control` scope; a session with `read` only gets the key with an empty value and `redacted: true`. A collection export through the API by a token leaves `script` out (widget tokens can sit in it); an export from the desktop app keeps it. Script text is never written to logs.

4. **Zoom.** `zoom_percent` sets the engine's page zoom (CEF zoom level `ln(zoom_percent / 100) / ln(1.2)`), so the page lays itself out for a larger or smaller viewport at the source's own `width` and `height`. It is not the item transform: the picture size does not change. The runner sets it on every main-frame commit, ignores any zoom Chromium kept per host, and counts "zoom applied" in ADR-0132's hold of a new document, so no frame at the old zoom is sent.

5. **Background pages (`keep_running`).** ADR-0107 decision 8 asks for a separate decision for any keep-warm override; this is it, for browser sources only.
   - **Who may turn it on.** Writing `keep_running` true needs the network permission, because it starts a process that opens connections while nothing shows the page; an agent without it still cannot wake a hidden page (ADR-0132 decision 9).
   - **When it is Active.** A browser source with `keep_running` true is Active while the open collection places it anywhere: an item in any scene on any canvas, hidden or not, inside a group or a nested scene, or as a global source. A source placed nowhere is Standby. Turning `keep_running` on makes a placed source Active at once; turning it off returns it to ADR-0107's rule at once, which stops it if no shown scene draws it. A source with `keep_running` false keeps ADR-0107 unchanged.
   - **No cost while unused.** The binder keeps the set of `keep_running` sources, updated when a source's settings change, and does no scene scan while the set is empty. Installed and Standby stay at zero cost.
   - **Slots.** Slots (ADR-0109 decision 7) gain a third class after program and preview, `background`; inside a class the lower source id still wins. A background page never takes a slot from a shown page. When a shown page needs its slot, the background page is stopped (its state is lost) and reports `browser_cap`; the Page tab says why. The state is kept across scene switches only while at most two pages need slots; the switch's help says so.
   - **While hidden.** While no shown scene draws it, the source's picture node stays out of the scheduled graph (no upload, no per-frame work in the studio). The studio tells the runner `shown false`, and the runner lowers its paint rate to 1 per second (`SetWindowlessFrameRate`) without telling the page it is hidden, so its timers and sockets keep running; `requestAnimationFrame` and CSS animations then run at that rate. The supervisor reads the frame ring at that rate. The studio sends `shown true` as soon as the page enters the preview class or a transition starts drawing it, and the runner restores `fps`, so a take usually starts with a fresh picture; the first picture can be up to one second old.
   - **Sound.** Its strip follows the existing on-air rules: it is not in the program mix while the item is not on air. While hidden, the supervisor drains the sound ring every 100 ms and drops the samples; the ring of 65 536 samples holds about 680 ms of stereo at 48 kHz, so a drain must come at least every quarter of the ring's length at the graph's rate, and the supervisor shortens the interval when the ring is smaller. On `shown true` it skips the ring to the present, plays no backlog, and returns to its 5 ms reads.
   - **Cost.** The Page tab states the cost next to the switch: one browser process and one of the two page slots while the source is in any scene. Estimate (ADR-0143 R1, not measured): one CEF tree of a light overlay is about 150 to 300 MB resident; the studio side adds about 12 wake-ups per second (2 frame-ring reads, 10 sound drains), plus whatever the page's own timers cost inside the tree. The implementing PR measures the resident memory and wake-ups per second of a hidden background page on the reference computers and lists the runner timers in the thread inventory (ADR-0143).

6. **Retry on load errors.**
   - **What is an error.** With `retry_on_error` true, a main-frame navigation fails when it ends in a network error, or when its response status is 408, 429, 502 or 504. The runner reads the status from the main-frame response before the document commits. A 500, a 503, a 404 or any other answer of the site is a page and is shown, because some sites answer a check page that resolves by itself with 503.
   - **Nothing of it on air.** ADR-0132's hold of a new main-frame document ends only when all of these are true: the response status is known and is not an error of this decision, the CSS is applied and the zoom is applied. Bounds: on a first load and on a retry, at most 500 ms after the document's first paint (ADR-0132's bound, now counted from the first paint instead of the commit); on a reload, decision 2's 2 seconds. When a bound passes, frames are sent as they are and `css_failed` is reported as ADR-0132 says. A failed navigation's document, including the engine's own error page, is never sent, whatever the time; the program keeps the last good picture, or nothing if there was none.
   - **When to retry.** After 2, 4, 8, 16, 32 and then every 60 seconds; a `Retry-After` of 1 to 600 seconds on a 429, 502 or 504 replaces the next step. A good load resets the sequence. The reload and script timers are paused while a retry waits. The source status reports `page_error {code, retry_in_s}` while it waits.
   - With `retry_on_error` false the engine's own error page is drawn, as in the usual tool. The schedule is a pure function in `rbs-browser-ipc` with an injected clock.

7. **Control API** (next free protocol minor at merge):
   - `browser.run_script {source, script?}` runs `script` once, or the saved `script` when absent, under the origin rule of decision 3. Effect `live` (a script can change what is on air) and the network permission; with a `script` argument on a `keep_signed_in` source, refused to any token (`local_only`, decision 3). No running tree is `browser_not_running`; an agent cannot wake a hidden page. The answer is `{}` once the runner has accepted it; no script result is returned.
   - The source status gains `page: {state: "loading" | "loaded" | "error", code?, retry_in_s?}`, `script: "sent" | "skipped_origin"` (absent before the first try) and `shown: bool` for a running page.
   - `not_permitted` gains the reason `local_only` (decision 3), a protocol-minor addition next to ADR-0052's `irreversible`, `on_air` and `network`. A client must treat a reason it does not know as a refusal; `rbs-mcp` reports it to the agent as "only the local user may do this".
   - The new settings go through `source.set_settings`; nothing else is added.

8. **Runner protocol.** This ADR depends on ADR-0132 decision 8 (a message channel from the studio to a running runner, and a status channel back), which the CEF runner does not have yet. `rbs-browser-ipc` then gains messages for `zoom`, `script` (set, run now), `timers` (reload and script intervals, on-load flag), `shown` and `retry_on_error`, and the status fields of decision 7. One message must carry 64 KiB of `css` or 32 KiB of `script` plus its header: the implementing PR raises `MAX_PAYLOAD` above the largest message, which also fixes `css`. It stays private (ADR-0109 decision 1).

9. **Checks.**
   - **Without CEF:** out-of-range values and a `script` over 32 768 bytes are `invalid_params`; script keys and `keep_running` true without the network permission are `not_permitted`/`network`; a token gets `local_only` for each order of writes that would end with a running script on a `keep_signed_in` source (script then sign-in, sign-in then script, a `url` move); the desktop app's caller context is allowed; a `read`-only session sees `script` redacted and a token's collection export leaves it out; the status reports `skipped_origin` after a navigation to another origin; a hidden background page's sound ring never overruns between drains and `shown true` plays no backlog; an imported collection keeps `script` with both run settings off; `browser.run_script` with no tree is `browser_not_running`; the origin match accepts the saved origin and the allowed `file` directory and refuses others; with the test-double runner, changing zoom, script or timers keeps the same process and a script edit does not run it; a `keep_running` source placed nowhere is Standby, in a hidden item of an off-air scene is Active in the background class, never takes a slot from a program or preview page, and its picture node is not scheduled; an empty `keep_running` set does no scene scan; the retry schedule is 2, 4, 8, 16, 32, 60, 60 seconds, takes a `Retry-After` and resets on success; the test-double runner's reload and script timers fire by an injected clock, never by sleeping (ADR-0032).
   - **On a device (Windows and macOS, short runs, like ADR-0132's pin check):** a network error and a 502 never put an error page on air; a 404 and a 503 page are shown; a zoom change never sends a frame at the old zoom; a hidden background page paints once per second and its socket keeps receiving; the script does not run after the page navigates to another origin; a periodic reload keeps the last picture until the new one is painted.

## Consequences

- Cost (ADR-0143): every setting is off by default except `retry_on_error`, which costs nothing until a load fails. Timers live in the runner while the tree runs. `keep_running` is the only setting that costs while hidden, and the user turns it on per source with its cost stated.
- Positive: overlays recover from network blips and stalls without a person; small page fixes need no external tool; scene switches keep chat and timers while slots allow.
- Negative: a user's script can do anything the page can do on its own origin, including fetching; it cannot reach RBS or another origin. A background page keeps a slot of two and is the first to lose it. Removing `script` later needs a scene-format migration that drops saved scripts.
- Spec follow-ups in the implementing PRs: scene sources (settings, reasons, status, IPC kinds), control API (`browser.run_script`, status fields, `local_only`, redaction), scene model (temporal sources: the `keep_running` exception), glossary ("Background page" if the UI uses it), `docs/status.md`.

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio:** no periodic reload, no user script, no zoom; a hidden page keeps running unless "Shutdown source when not visible" is on, so every placed page costs by default; the engine's error page goes on air.
- **What RBS does better:** each of these is a per-source saved setting and a control API command; hidden pages cost nothing unless the user asks; errors never reach the program; a background page never steals a slot from a shown one; a script never follows the page to another site.
- **Left for later:** a per-source user agent and auto-scroll (owner, 2026-11-18); a reload when a page stops painting (stall detection); a read-only status API for pages (OBS's `window.obsstudio`), which ADR-0109 decision 10 still forbids.

## Alternatives considered

- **Built-in actions only (click selector, scroll, hide element), no free script.** Safer, but users paste snippets from widget docs; every new action would need a release. The script has no more rights than the page on its own origin.
- **Reload and scripts driven by the studio.** A studio timer per source adds a thread or a wake-up on the control side; the runner already has an event loop.
- **Reload when the page stalls instead of on a timer.** Better targeted, but a stalled socket often still paints (a clock, an animation); kept for later, next to the timer.
- **Keep-running as a global switch, like the usual tool.** Charges every placed page; per source keeps P13.
- **Keep-running pages at full frame rate while hidden.** Wastes CPU on pictures nobody sees; 1 per second keeps a fresh picture ready.
- **Retry every 5xx.** Cuts off self-resolving 503 check pages and ignores `Retry-After`.

## Review

Round 1. Reviewer: independent AI session. Date: 2026-11-18. Verdict: changes requested. Concerns and resolutions (author, 2026-11-18):

1. Five features bundled, two of them changing accepted rules. -> Each feature is accepted or rejected alone (preamble of the decision).
2. A script on a `keep_signed_in` source can take the account. -> Agents may not write it there (`local_only`, decision 3).
3. The script follows the page to other origins. -> Bound to the saved origin (decision 3).
4. Imports and read-only tokens. -> Imported scripts keep their text with both run settings off; redacted for read-only sessions; never logged (decision 3).
5. `script_errors` cannot be counted with `ExecuteJavaScript`. -> Dropped; no renderer handler (decision 3).
6. 64 KiB cannot cross IPC; no message channel yet. -> 32 KiB limit, `MAX_PAYLOAD` raised, dependency on ADR-0132 decision 8 stated (decision 8).
7. HTTP errors arrive after commit. -> Status read before commit, the hold extended until known (decision 6).
8. 503 challenge pages, `Retry-After`, timers during errors. -> 500 and 503 shown, `Retry-After` honoured, timers paused (decisions 2, 6).
9. Slot eviction loses state. -> Stated, reported as `browser_cap`, explained in the UI (decision 5).
10. `keep_running` semantics. -> Effect in both directions, "placed" defined, no scan while empty (decision 5).
11. 1 Hz paint and its cost. -> rAF effect stated, `shown true` on preview and transition start, cost measured in the implementing PR (decision 5).
12. Zoom flash. -> Zoom applied inside the hold, per-host zoom ignored (decision 4).
13. Reload on air flashes. -> Last picture kept until the first paint, up to 2 s; help warns about `keep_running` (decision 2).
14. A script edit runs code at once. -> An edit runs nothing; the new text is used from the next load or tick (decision 3).
15. Device-only checks. -> Device checklist added (decision 9).

Round 2 (same independent AI session, 2026-11-18): changes requested. Round-1 concerns 1, 3, 5, 6, 9, 10, 12, 14 and 15 resolved. Concerns and resolutions (author, 2026-11-18):

1. "Agent session" names no flag, and the order of writes bypasses it. -> Any token is refused, using ADR-0052's caller context; the check runs on the result of every write of the five keys (decision 3), one test per order (decision 9).
2. `edit` is not a scope; exports. -> `control` scope; a token's API export leaves `script` out (decision 3).
3. The hold wording reads as "any of". -> "All of these are true", with bounds (decision 6).
4. ADR-0132's hold changes undeclared. -> Listed under "Changes", one bound per case (decision 6).
5. 503 `Retry-After` contradicts "503 is shown". -> `Retry-After` on 429, 502 and 504 only (decision 6).
6. No cost estimate. -> Estimate added, sound drained every 100 ms while hidden (decision 5).
7. `keep_running` wakes a page without the network permission. -> Turning it on needs the permission (decision 5).
8. `local_only` is a new reason. -> Declared as a protocol-minor addition; unknown reasons are refusals (decision 7).
9. A skipped script is invisible. -> `script` status field and exact-origin help (decisions 3, 7).

Round 3 (same independent AI session, 2026-11-18): changes requested, minor (three text edits, then approve without another design round). Round-2 concerns all resolved. (1) Decision 7 said "agents" where decision 3 says any token. -> Fixed. (2) A 100 ms drain needs a ring that holds it, and a backlog must not play. -> Drain at most a quarter of the ring apart, skip to the present on show, both checked (decisions 5, 9). (3) `ran` claimed more than the runner knows. -> Renamed `sent` (decisions 3, 7).

Round 3 confirm (same independent AI session, 2026-11-18): **approve**, no open concerns. Acceptance waits on the owner.
