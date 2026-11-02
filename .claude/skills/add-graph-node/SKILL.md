---
name: add-graph-node
description: Add a new media graph node type to RBS (source, video/audio filter, transition, mixer). Use when implementing any new effect, source or processing step.
---

# Add a media graph node

Spec: `docs/specs/media-graph.md`.

1. **Decide the kind** (Source, VideoFilter, AudioFilter, Transition, AudioMixer). If it needs external technology (OS API, vendor SDK), that part is an adapter behind a port; the node only uses the port.
2. **Write the descriptor:** kind, input/output links with formats, settings schema (types, ranges, defaults). The UI is generated from the schema; do not write UI code.
3. **`prepare`** may allocate: create GPU pipelines, buffers, pools sized for the negotiated format.
4. **`process`** is real-time: no allocation, no blocking, no I/O, no logging per frame (use counters). Video filters work on GPU textures with WGSL; never read back to the CPU.
5. **Timestamps** pass through unchanged unless the node is a source (sources convert to master-clock time at the edge, ADR-0004).
6. **Failure:** return typed errors; the graph handles bypass and restart. Never panic.
7. **Tests:**
   - Unit tests for settings validation.
   - Golden image test (video) or sample-exact test (audio) with the test clock.
   - Allocation guard test around `process`.
8. **Metrics:** make sure processing time is reported by the graph wrapper; add node-specific counters only if useful.
9. If the node is on a hot path for common scenes, run `measure-performance`.

Done when: checks pass, golden/sample tests committed, the node appears in the node catalogue docs.
