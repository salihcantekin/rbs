# Hot paths

Code on these paths runs per frame, per audio block or per packet. Code edited inside these functions and modules, or called directly from them, follows principle [P15](principles.md#p15-hot-paths-get-extra-scrutiny): measurements, the `hot-path-review` checklist and human approval.

For `Node::process`, this covers the graph's call into nodes and the built-in nodes shipped with RBS. A third-party plugin's own `process` is its author's responsibility; the host enforces its time budget.

Keep this list current. When a new crate or module lands on one of these paths, add it here in the same PR; `.github/CODEOWNERS` mirrors the paths so reviews are requested automatically. CODEOWNERS works on whole files and directories, so it is coarser than this list; that is accepted.

| Path | Runs | Location (current or planned) |
| --- | --- | --- |
| Audio thread: device callbacks, audio graph, audio filters | Every block (10 ms) | `crates/rbs-audio`, audio adapters in `crates/rbs-audio-io-*` |
| Render loop: compositor, GPU filters, transitions | Every frame per canvas | `crates/rbs-graph` scheduler, `crates/rbs-gpu` |
| `Node::process` of every node type | Every frame or block | Wherever nodes are implemented |
| Frame, texture and audio buffer pools | Every frame or block | `crates/rbs-gpu`, `crates/rbs-audio` |
| Colour conversion to encoder formats (NV12, P010) | Every frame per encoder | `crates/rbs-gpu` |
| Encoder submit/drain and packet fan-out | Every frame per encoder | `crates/rbs-codec*`, `crates/rbs-graph` |
| Muxers and transport send loops | Every packet | `crates/rbs-mux*`, `crates/rbs-transport*` |
| Clock reads and timestamp mapping | Everywhere above | `crates/rbs-core/src/time.rs` |
| Plugin call boundaries (native vtable, WASM, IPC) | Every frame or block for active plugins | `crates/rbs-plugin-host` |

Not hot: start-up, probing, configuration, the control API, UI, scene editing (these must still not block hot paths).
