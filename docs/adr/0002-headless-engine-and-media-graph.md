# ADR-0002: Headless engine built on a media graph

- Status: Accepted
- Date: 2026-11-02

## Context

OBS couples its core to a single canvas, a single stream output and a Qt UI. Features such as vertical + horizontal output, many destinations, or remote control were added later and remain fragile. We want those to be natural consequences of the architecture.

## Decision

The engine is a headless library and process whose runtime is a directed acyclic **media graph** of nodes connected by typed links (video frame, audio block, encoded packet, data/event). Sources, filters, compositors (one per canvas), encoders, muxers and outputs are all nodes. The graph can be edited while running; edits apply atomically at a frame boundary. Details: [media-graph spec](../specs/media-graph.md).

## Consequences

- Multiple canvases, shared encoders and any number of outputs need no special cases.
- Plugins extend the system by contributing node types.
- Per-node metrics give clear answers to "where are frames dropped?".
- We must build a scheduler that respects real-time constraints; this is core work in Phase 1.

## Alternatives considered

- Fixed pipeline (OBS style): simpler at first, but every new topology needs core changes.
- GStreamer as the graph runtime: mature, but its dynamic typing, GLib object model and threading make zero-copy GPU paths and a stable plugin ABI on Windows/macOS harder to control. GStreamer may still be used inside an adapter.
