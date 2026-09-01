# Recorded GL command streams

Each file is a `.gexgltr` recording (the format is defined by
`crates/gex-gl/src/trace.rs`) kept as a regression: a trace replays in about a
second, names the commands at fault, and fails again the moment a defect comes
back.

Replayed by `tests/gl-capture-replay.sh`, and usable directly with
`tests/gl-replay.mjs` (softpipe) or `tests/mesa-wgpu-replay.sh` (WebGPU).

The client API and version are carried in the stream's own `CONTEXT_CREATE`, so
a trace replays against whatever the guest actually asked for; there is no
`--api` to get wrong.

| trace | what it covers |
| --- | --- |
| `gles-gears-frames.gexgltr` | 210 batches of `gles-gears` recorded in a real browser through the page's WebGL 2 device. The cross-backend workload: `tests/mesa-wgpu-replay.sh` runs it through softpipe and through Mesa + `gex_wgpu` on WebGPU and compares per-channel means. |
| `gles-program-setup.gexgltr` | The first three batches of that capture: context creation, shader create/source/compile, program link, and the shader queries. Guards the defect where the backend created a desktop core profile, which cannot compile the `precision`-qualified GLES source a guest emits. |

A reduced trace from `tests/gl-reduce.mjs` is usually *not* a good fixture here:
reduction deletes commands, so the result often calls into objects it never
created. That is exactly what a minimal reproducer should do, and exactly what
a clean-replay regression should not.
