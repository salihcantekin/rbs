# ADR-0192: Transactional group Show

- Status: Superseded by ADR-0193
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes in part: ADR-0020 decisions 1, 3 and 4, for group Show preparation and staging only
- Supersedes in part: ADR-0013 rule 5, for unproved group Show resource retirement after failure
- Builds on: [ADR-0105](0105-timed-show.md), [ADR-0184](0184-group-properties-batch-to-members.md), [ADR-0185](0185-scroll-a-group-as-one-picture.md), [ADR-0190](0190-prepare-group-resources-on-executors.md)

## Context

Protocol 1.108 requires a group Show to start or restart every hidden
member's cue at one control-clock instant and a refusal to change nothing.
The owner requires failures and timeouts before commit not to leave a partial
group display. Once Show succeeds, individual source failures are ordinary
runtime health: the other members continue. Audio and video may switch at
their own next block and frame boundaries. The owner chose a logical
commit-versus-timeout race rather than a strict physical-clock cutoff
(2026-11-25).

Today the binder drafts a graph, but `Studio::update_with` also mutates other
state and `reconcile` sends layers and strips before applying the graph.
`Engine::apply` swaps partitions independently; a timed-out swap may still
finish and `service` may send an unsent part. ADR-0023's latest-value
parameter channel is neither a delivery acknowledgment nor a commit barrier.
Applying and then undoing can expose part of the group.

## Decision

1. **Scope and preflight.** Only `group.apply` Show uses this transaction.
   The host checks complete membership, durations, permissions, the existing
   group Scroll gate and every affected occurrence as protocol 1.108 requires.
   All-visible succeeds immediately without a change or event. Otherwise the
   host allocates a unique, never-reused operation id and drafts the cue map,
   binder and graph, source activity, video layers, audio strips, links and
   other affected parameters. Live, held and frozen outgoing cue snapshots
   retain their existing distinct semantics. No draft is exposed as live
   state, history, a source failure or a Show event.
2. **Stage and acknowledge.** Prepare only nodes needed by the candidate plan,
   without replacing or resetting reused producers. Stage required topology
   and complete old/new parameter banks for every affected compositor,
   mixer and other consumer. The old output and existing source state must
   remain correct throughout staging, including if input indices change.
   An executor may temporarily contain hidden added resources and links, but
   the authoritative Studio graph and binder remain unchanged. A source
   already running keeps running; staging must not advance an otherwise
   dormant source's playback before commit. If that cannot be guaranteed,
   refuse rather than alter live behavior.

   Every required preparation, executor-thread warm, topology installation
   and consumer bank must acknowledge successful staging. The acknowledgment
   must report the candidate's actual warm outcome, not merely return the
   previous schedule. Failed, bypassed, unsupported or unavailable required
   nodes, failed resource preparation, or a full/closed staging mailbox
   refuse the request. Banks are operation-owned and cannot be displaced by
   ordinary latest-value parameter sends. Receivers acknowledge that the
   entire matching bank and topology are usable before commit; a delayed
   delivery cannot publish only some members. Readiness is prepare and warm
   success, **not** a first frame or audio block: a source may legitimately
   be silent or transparent.
3. **Occurrences.** Build the candidate with the same per-occurrence draw
   predicate as ordinary binding, not a source-wide cue flag. Cover program,
   preview and incoming on every affected canvas, globals, nested scenes and
   their ancestor visibility, and live hold-scene outputs and their audio.
   The held program copy uses its own mapped, captured cue deadlines; a Show
   on the edited scene does not cue it. A timed transition's outgoing layers
   and cues stay frozen. A manual transition's outgoing cue visibility stays
   frozen from its first `t > 0`, including the incoming-ready wait, while
   its other program edits and source frames stay live. Incoming and preview
   cues remain live. Video, source activity, producer links and audio must
   agree with the corresponding occurrence predicate; deduplicate shared
   producers without leaking one placement's cue to another or double-mixing.
   A freeze-frame hold uses its captured picture; rehearsal samples the
   current program and preview pictures. Replay captures encoded program
   output; neither old replay frames nor saved clips are re-rendered or
   rewritten by this Show.
4. **Revision and abort ordering.** Use one engine/studio-owned `AtomicU64`
   guard word, with a phase, a worker-invalidated bit and a monotonic revision.
   One possible fixed layout is four low bits for the phase and invalidated
   bit and the remaining 60 for the revision. The control thread is the only
   writer of the revision; it must fail closed before revision exhaustion, never
   wrap or reuse it. Each candidate admission advances and records a fresh
   revision even if no live mutation occurred, so the guard identifies that
   operation without an ABA reuse. Before any potentially conflicting
   mutation, a control-side CAS advances the revision and simultaneously
   changes pending/ready to aborted, or committed to
   superseded; it preserves a concurrently set invalidated bit. If that CAS
   races a worker, the control thread retries on its cold path. Only after the
   CAS may it mutate or send. This guard belongs at the lowest shared
   mutation/send entry point, not just API routing. It also precedes ordinary
   operations that later return an error after a partial effect. Known no-ops
   need not advance the revision.

   This covers: live and held cue start, restart, cancellation, removal,
   expiry, remapping and restart handover; `item.show`; collection, group,
   membership, visibility, layout, source, filter, audio and canvas edits;
   binder reconciliation; graph adoption, lowering, swaps, moves, links,
   queues and their partial ordinary apply/service outcomes; source lifecycle
   and relevant node health, failure, bypass, recovery and restart; program,
   preview, incoming, outgoing, hold, rehearsal and transition identity or
   snapshot changes; and all layer, mixer, filter and other affected parameter
   sends, including sends outside ordinary `update`. Authorization changes
   that affect the request serialize with final revalidation and commit.
   A conservative global revision is allowed. A genuinely independent edit
   can proceed without abort only if it cannot affect any recorded dependency.
   Live frames and blocks, and animation evaluated from already-staged timing
   parameters, do not increment it per tick. A changing value not representable
   exactly by those parameters invalidates before it changes.

   On a relevant worker failure, the worker records the health detail and
   counter in its own bounded, preallocated, race-safe storage, then one
   bounded acquire-release atomic `fetch_or` sets the invalidated bit on that
   **same guard word** as the health publication, before waking control.
   Distinct workers do not overwrite each other's unpublished details, and
   counters are monotonic, non-wrapping and never reset by collection; a full
   detail slot coalesces failures without losing the fact of failure. Each
   RMW acquires the preceding guard RMW so coalesced workers' publications
   remain visible. Readers must not treat a detail as published before its RMW.
   A commit CAS expects the exact ready,
   revision and clear-bit word: if invalidation wins, commit fails; if commit
   wins first, the failure is post-commit. An acquire read of a committed word
   with the bit set treats it as superseded, not as permission to newly latch
   the old record. It does not undo the committed control state or cue map:
   runtime health is applied on top, and a successor includes the surviving
   members. The control thread processes and coalesces health using
   its existing authoritative counters, then advances the revision and
   aborts or supersedes; it never clears the bit for an outstanding candidate.
   Before a later admission it clears the bit by acquire-release CAS, collects
   published health, and rechecks the bit and counters before ready, so a
   racing failure is either included or prevents commit. Counter exhaustion
   fails closed. Only the failure path does the worker
   RMW; healthy ticks do not check for a transaction. Unreported physical
   failures cannot be detected by this protocol. Ordinary operations,
   including natural cue expiry and `item.show`, then continue under their
   own contracts without waiting for staging cleanup; their state and events
   are never rolled back.
5. **Bank identity and acknowledgment.** A staged bank is identified by
   `(operation id, partition id, consumer node id and incarnation, parameter
   kind, epoch, baseline revision, send generation)` and is bound to the exact
   staged topology generation and occurrence set. Ids, incarnations, epochs,
   revisions and per-consumer send generations do not wrap or get reused;
   exhaustion fails closed. Each consumer/parameter pair reserves its next
   send generation for the candidate after all pre-admission ordinary sends;
   all later ordinary sends use greater generations. The receiver maintains
   its applied high-water mark and rejects an older or wrong-incarnation send,
   including a staged bank arriving late. A complete replacement bundle has
   a single control revision and contains the corresponding newest value and
   send generation for each affected consumer. The control side keeps the
   latest accepted ordinary value and generation, including an unread ADR-0023
   value. `pending()` alone is not an acknowledgment or a copy.
   Operation-owned storage contains complete, immutable candidate parameters
   and keeps the ordinary live bank and its pending sends separate.

   For an active receiver, its owning executor acknowledges the exact bank,
   topology and baseline only after every older accepted ordinary send is
   consumed or its exact newest value is transferred without changing the old
   output. A parked receiver is serviced by its owning parked executor with
   attach/detach as needed; it installs and acknowledges the same baseline
   without starting a worker or running a tick. A duplicate, wrong-id, stale,
   partial or post-abort acknowledgment is ignored. The acknowledgment names
   the full bank identity and its topology generation. Every affected receiver
   must acknowledge before ready.

   A running mixer's mutable gain/ramp state remains owned by the one live
   instance; selecting the new bank at its block boundary derives continuing
   ramps from that instance's then-current state and tick time, not a stale
   stage-time copy. New strips use ordinary join behavior. The same rule
   applies to time-dependent animation; parked state retains its timing until
   the first later tick. If a receiver cannot preserve this state, it fails
   closed. Send and acknowledgment sequences reject stale generations:
   neither a late stage bank nor an older ordinary send may overwrite a newer
   applied value. Replaced banks are reclaimed on their owner only after no
   staged, active, parked, in-flight or GPU user can reference them; no
   deallocation occurs on a real-time thread.
6. **Publication and supersession.** Before ready, the host constructs an
   operation-owned `CommitRecord`: the unaffected exact cue deadlines,
   member cue durations, binder and logical graph, revision, occurrence and
   topology generations, and the complete bank identity set for every
   participant. Its candidate data is immutable; its initially empty,
   single-assignment commit-time slot is not read by participants until a
   successful commit. After all acknowledgments and other revalidation, the
   control thread takes the final pre-commit studio-clock sample and fills
   that slot and the already reserved member-deadline storage exactly once.
   Member deadlines are that instant plus each duration. No candidate is a
   live cue map before commit.
   The host and each executor hold stable references before acknowledging;
   installing the reference does not publish it. The shared guard word is
   pending, ready, committed, superseded or aborted for that record, with its
   revision and invalidated bit.
   Revalidate the recorded revision, complete membership, authorization,
   snapshot identities, required health and all exact acknowledgments. If
   any live or held cue is due at the sampled instant, abort and process its
   expiry first.

   The commit linearization is one compare-and-swap of that word from ready
   to committed; it compares the operation's exact recorded revision and
   clear invalidated bit. Timeout competes by atomically changing that same
   operation's pending or ready phase to `aborted-timeout`, preserving the
   revision and invalidated bit. The timeout CAS retries on a concurrent bit
   change only while that same operation is pending or ready; it must not
   touch a newer revision. It may report timeout only after that change
   succeeds; commit cannot succeed afterward. If commit wins, timeout must
   not report a refusal, even if the response is delayed. Other terminal
   aborts likewise cannot later become committed. An operation id and
   non-reused revision reject stale timeout wakes and acknowledgments; only
   one result is reported.

   The control loop arms its existing deadline wake for admission time plus
   at most `swap_timeout`, and checks the injected engine clock between bounded
   preparation steps and immediately before attempting commit. At or after
   that sampled deadline it attempts timeout, not commit. The timeout wake
   samples the injected clock and can attempt timeout only when due; it uses
   the same guard and is also serviced while staging is pending. No permanent
   timer thread or polling loop is required. If the commit attempt
   was begun from a pre-deadline sample and the thread is delayed, its CAS
   may win after the nominal deadline if timeout has not won. A late success
   is permitted, not a strict physical-time guarantee; ordinary OS scheduling
   places no bound on how late either result is delivered. A local timeout
   observed independently by a remote client is not a server timeout report
   and does not by itself cancel the server operation.

   On success an acquire read of the guard makes the
   record authoritative for both control queries and all partitions. The
   control thread reads this record as the committed state, promotes its
   contained binder/graph and cues without a second visibility decision,
   then emits `group.changed` and answers success. The private deadline
   values use the one sampled studio-clock instant associated with that
   commit. There is no interval in which control may report old cues while a
   partition is allowed to use the new epoch. The shared word uses
   release/acquire ordering for
   the complete record, bank and topology writes, and the record stays alive
   until every participant has stopped referencing it. Control queries and
   callbacks must serialize with promotion and cannot read stale backing
   fields after the CAS; ordinary callers need not wait for a worker.
   Unrelated events remain independent.

   Cue deadlines use the same sampled instant and are never extended to
   compensate for delay. A live or held expiry that becomes due after that
   sample is processed by the ordinary control loop; if its mutation wins the
   guard before commit it aborts the candidate, otherwise it follows the
   committed Show. The control loop checks due expiries again immediately
   after a successful commit, including member cues that may already have
   elapsed, and publishes any resulting ordinary successor. Like ordinary
   cue service, physical expiry can be delayed by scheduling; this protocol
   does not claim an atomic clock-and-commit primitive.

   At each tick boundary a participant acquire-loads this word once and
   latches the record's full topology and banks for its whole frame or block;
   an already latched tick may finish after a subsequent control change.
   Audio and video need not switch simultaneously, and queued media is not
   retracted. No fallible allocation, warm or delivery remains before a
   participant can take that epoch.

   While the fence is active, a later ordinary mutation first changes the same
   committed word to superseded with a newer non-reused generation, before
   publishing its own control state or sends. A participant that has not
   latched the commit cannot newly latch that older record after observing
   supersession. While
   this temporary fence is active, each affected partition receives a complete
   latest-ordinary replacement bundle, tagged with that generation and its
   topology; it may coalesce intermediate ordinary updates and retains its
   current output until the newest complete bundle is usable. A stale
   transaction bank or ordinary send cannot replace a newer accepted bundle
   or resurrect an expired cue. Once all partitions have latched the commit
   or a valid newer successor, or are retired/parked with that state installed,
   and no older message can still be accepted, the transaction fence and
   records can be retired safely. A new group Show
   cannot be admitted while an earlier committed fence is still outstanding;
   ordinary operations remain available. Ordinary updates outside this fence
   retain ADR-0023 semantics.
7. **Schedule isolation.** Candidate lowering, partitions, dirty and unsent
   parts, node placements, crossings, moves, endpoints and retirement records
   belong to the operation, not the ordinary engine's `current`, dirty or
   lowering state. Reserve ids and queue resources without reusing ids still
   owned by either path. Each node instance has exactly one owner: reused
   nodes move with their actual executor schedule, never a duplicate, while
   newly prepared nodes and candidate-only endpoints remain operation-owned.
   An installed staging schedule may carry existing instances, but preserves
   their old behavior and old-output slots while its candidate additions are
   hidden.

   Ordinary apply or service must never send an operation's candidate parts
   as ordinary dirty work or promote its graph, crossings or placements.
   On conflict the abort state is published first, then ordinary apply updates
   its own newest desired graph as usual. If a staged swap is in flight,
   ordinary apply/service can wait for it under existing timeout behavior
   and retain their own latest dirty graph; the control thread must not wait
   for candidate cleanup before accepting an unrelated operation. An unsent
   candidate part is discarded on abort. A worker checks its operation id
   and terminal state when accepting an in-flight part. If installation races
   the abort, an already-installed or concurrently installing superset still
   uses the old live bank and cannot publish candidate output. Its late
   completion only advances owner-directed cleanup; it cannot reinsert
   candidate dirty parts, change ordinary `current`, release a reused node,
   or publish a stale parameter bank. Once the in-flight owner returns,
   reconcile the **latest** ordinary graph, never a saved pre-transaction
   graph, and release staged resources only after all users are proved done.
8. **Abort and quarantine.** A pre-commit error or winning timeout refuses without
   changing this Show's cues, held deadlines, layout, run, history or events,
   and without exposing any candidate picture or sound. Cancellation and
   timeout are terminal even when a worker later completes: late results
   cannot commit or send candidate parameters. Hidden staging installed before
   abort must continue to produce the old output, and unsent candidate work
   must not later be applied. Cleanup runs on the resource owner; timeout is
   not proof of cancellation, release or GPU retirement. Do not wait for
   cleanup before answering, reuse the id, drop a still-owned resource or
   restore a stale whole-studio snapshot over independent work.

   Retain unproved resources and their owner, reject conflicting work and
   use the bounded wake, retirement and studio-retention rules of ADR-0190.
   At most one group Show preparation or quarantine is retained by the host;
   a quarantined one blocks further group Show preparations and studio
   replacement until resolved, preventing accumulating retained studios.
   No periodic retry of the candidate remains for a terminally quarantined
   operation; independent ordinary work retains its existing service rules.
   This is a failure-only exception to ADR-0013 rule 5, not a standby cost.
9. **Bounded preparation.** Admission starts one deadline no later than
   `swap_timeout` from admission. A new opt-in node preparation contract
   must use bounded, nonblocking control-thread steps with explicit pending,
   wake, completion, unsupported and failure outcomes; an existing node's
   default is unsupported and must not call legacy blocking `prepare`.
   A backend may delegate its own work only with explicit ownership and
   correct OS thread affinity; the engine does not accept arbitrary work
   closures. Cancellation or timeout retains a pending instance and its
   dependencies until completion and safe retirement are proved. No blocking
   `release`, destructor, worker join or GPU wait may be on the response
   path. Staging cleanup also needs bounded, nonblocking release steps or
   must retain the instance as unsupported; ADR-0191's planned quiesce-only
   steps do not by themselves provide this. Like those release steps, a
   defective step that never returns cannot be preempted by a timeout on the
   same thread.
10. **After commit and idle cost.** A failure reported after the commit is
   runtime health, not a refused Show; remaining members continue. A worker
   that stalls after acknowledging but before its next tick stays on its old
   output until it resumes. An unreported stall or failure cannot be detected
   by the commit barrier; failure and commit are ordered when the failure is
   published to the transaction. Success does not promise a first media
   sample or immunity to later source failure.

   Tick processing and commit observation must not wait, allocate, perform
   I/O or take blocking locks. Executor-thread `warm` may allocate between
   ticks as in ADR-0020, but must not block. A worker needed by the candidate
   may start during staging and is retired when no longer needed; do not
   create an idle worker or add group-specific checks to ordinary
   steady-state ticks. Staged
   schedules/banks are retired on their owners without freeing values on
   real-time threads. Existing
   `item.show`, ordinary apply, ADR-0023 parameter channels and ordinary
   node preparation and release retain their current behavior.

## Consequences

- Update the planned control API, recording, engine, media-graph,
  GPU-compositor and audio-graph contracts. This ADR and those additions
  are accepted but not implemented. The group Scroll fail-closed gate
  remains; ADR-0189 is not implemented by this decision.
- Staging, warm acknowledgments, parameter banks, opt-in nonblocking source
  preparation, timeout quarantine and safe retirement do not exist today.
  Unsupported backends fail closed until implemented. The staging protocol
  must not change the existing engine's ordinary partial-apply semantics.
- Deterministic tests must cover:
  - active and parked receivers, pending old parameters, a delayed bank,
    newer mixer/layer sends before and after commit, and expiry before a
    stalled receiver latches, without old-bank resurrection;
  - item Show on the same and a different member, cue restart/cancel and
    exact live and held expiry while staging; graph, membership, source,
    permission, snapshot and animation changes; abort ordering and
    independent events with no stale cue-map restoration;
  - one installed, one unsent and one in-flight partition at abort, worker
    acceptance racing abort, normal apply/service while quarantined, late
    completion, unique ownership and retirement, and reconciliation to the
    newest ordinary graph without candidate publication;
  - program and preview on multiple canvases, globals and nested ancestor
    visibility, separate held-copy cues, live hold scenes, timed and manual
    outgoing including the incoming-ready wait, live incoming, freeze-frame
    hold and rehearsal, and replay's previously encoded and later output;
    mixed audio/video and the same producer at different item ids, proving
    links, activity, pictures and mixer inputs use the right occurrence;
  - guard races between worker failure and commit, failure during admission
    and bit clearing, multiple coalesced failures, revision exhaustion,
    stale node incarnations and reordered sends; prove release/acquire record
    visibility and deferred retirement;
  - clock advance or descheduling between the pre-commit sample and CAS:
    timeout first refuses permanently, commit first succeeds even after the
    nominal deadline, and a sampled time at the deadline attempts timeout;
    stale wakes and later completions cannot reverse either outcome;
    cue expiry before and after the CAS and already elapsed member deadlines
    receive normal successor processing;
  - prepare/warm failures, source reuse, timeout/commit/failure races,
    all-visible Show, post-commit source failure, quarantine ownership,
    idle zero-cost and unchanged ordinary `item.show` and apply.
  Hot-path changes need measurements, the hot-path checklist, independent
  review and human approval.

## Alternatives considered

- **Apply then undo:** a partial picture or sound may escape, and a timed-out
  swap can complete after the undo.
- **Atomic cue map only:** independent graph and parameter delivery can
  still expose only part of the group.
- **Sequential ordinary parameter sends:** sending is not acknowledgment and
  an unread bank can be displaced.
- **Pause the engine or create a permanent worker:** harms real-time
  behavior or zero-cost-when-unused; a blocking control thread cannot
  enforce a deadline or process cue expiry.

## Review

Independent AI review, 2026-11-25: changes requested. The reviewer asked for
live revision/conflict ordering, isolated candidate schedule ownership and
ordinary service behavior, and occurrence-specific coverage. Decisions 3–7
and the conformance cases propose those details.

Independent AI follow-up review, 2026-11-25: changes requested. The reviewer
asked for a concrete shared commit and supersession primitive, a provably
deadline-checked CAS, bank identity and lifetime, and atomic worker
health/revision invalidation. Decisions 4–6 propose the guard, record, bank
and supersession protocols. The owner subsequently selected logical
commit-versus-timeout arbitration instead of the strict physical cutoff.

Fresh independent AI review, 2026-11-25: approved without blocking findings.
Owner approval, 2026-11-25: Salih approved the reviewed ADR.
