# The browser GL path: cost against pewasm's (stream GL)

Stream GL closes the GL/compositor cost gap between gex's browser page and
pewasm's (OPTIMIZATION-BACKLOG.md E1-E4, E10, W1). Branch `merge/glpath`,
worktree `wt/glpath`, scratch `~/gex-sandbox/scratch/GL` (`$G` below).

## Resume

- State: E2 diagnostic in progress (see "E2" below for what is measured).
- Tools (all in `$G`, run on edger through `scratch/bench/bench.sh`; never on
  gooner):
  - `wgpu-count.js`: a WebGPU call counter (calls, ms inside, bytes) by
    prototype wrapping. Injected into pewasm's page (`run-pw-count.sh TAG
    SECS`, a copy of `port/browser.mjs` with `--inject`) and prepended to the
    Mesa worker's module in an eject site copy (`site32c`).
  - `gl-run.mjs`: eject9's page-run.mjs plus `--every N` (counter snapshots
    from the compositor worker with the world frame) and `--capture A,FILE`
    (freezes the worker's `?glcapture=` recording A s after world-up and
    pulls it over CDP in 8 MiB chunks).
  - `pwcount.py LOG [SPAN]`: WebGPU calls per world frame from either log.
  - `replay/index.html` + `replay-run.mjs MODS TRACE`: replays a trace
    through Mesa builds in `$G/mods/NAME/` on hardware WebGPU, timing every
    submit; prints CPU ms per frame and WebGPU calls per frame over the last N
    frames; `--profile` writes a V8 CPU profile of the measured window.
  - `ops.mjs TRACE [N]`: opcode histogram per frame.
  - `bm.sh NAME ROOT [CROSS2]`: builds gex-wgpu-replay from
    `ROOT/third_party/mesa` (a worktree of `$G/mesa-wink`) into
    `$G/mesa-NAME`. Roots: `roots/cur` (wink 5f5572e6, gex's pin) and
    `roots/pw` (wink 1f16bf09, pewasm's).
  - `prof/top.py`, `prof/callers.py`: V8 profile summaries.
- mesa-wink changes are prepared as commits in `$G/mesa-wink` (a blobless
  clone, origin GitHub) and never pushed without the user's approval.

## E2: where the compositor's time goes (2026-10-07)

All numbers from edger (Quadro P620, WebGPU over Vulkan, headless Chromium
153, BENCH_CORES default, world seed 1767225600, 4x clock).

### Live counts, per world frame (WebGPU calls)

pewasm's page (`out/pwc1.log`, 4641 world frames over 100 s, 46.3 world fps)
against the ejected page (site30 + counter, `out/cap1.txt`, 7026 world frames
over 100 s, 70.1 world fps). Calls per world frame, and ms of JS inside them:

| call | pewasm | gex (eject) |
| --- | --- | --- |
| beginRenderPass | 15.0 (0.16 ms) | 52.0 (0.55 ms) |
| createView | 16.0 (0.16 ms) | 78.0 (0.53 ms) |
| writeBuffer | 87.0 (0.21 ms, 19 KiB) | 179.3 (0.39 ms, 21 KiB) |
| copyBufferToBuffer | 0 | 123.3 (0.13 ms) |
| createBuffer | 0 | 15.0 (0.08 ms) |
| createRenderPipeline | 0 | 4.0 (0.16 ms) |
| setBindGroup | 64 | 108 |
| setViewport / setScissorRect | 9 / 9 | 45 / 45 |
| draw + drawIndexed | 12 + 35 | 12.9 + 35.1 |
| queue submit | 14.0 | 2.7 |
| writeTexture | 5.0 (839 KiB) | 5.0 (841 KiB) |
| all calls, JS ms | 1.85 | 3.13 |

The draws are the same (47 a frame). Everything around them is two to five
times as much in gex.

### The trace

`gl-run.mjs --capture` took the ejected page's GL stream to 30 s after
world-up: 1.59 GB, 3403 batches, 2777 frames (`out/noita-world.gexgltr` on
edger and gooner). Per world frame (last 300): one batch, 3321 commands,
901 KiB, of which 839 KiB is five glTexSubImage2D (the world's pixels). 47
glDrawArrays, 18 glBindFramebuffer, and **142 glBufferData of 17 KiB in all:
the NT layer's copies of Noita's client-side vertex arrays** (three arrays a
draw, `upload_scratch`). No buffer maps, no glBufferStorage (so E3 and
pewasm's `-GL_ARB_buffer_storage` cannot matter for Noita's frame).

### Replays of the same trace (`replay-run.mjs`, last 600 frames, CPU ms of
`gex_gl_submit` per frame, counters off; two passes, in and reversed order)

| Mesa build | ms per frame | passes | copies | submits |
| --- | --- | --- | --- | --- |
| site30's (wink 5f5572e6, as shipped) | 8.90, 9.08 | 51 | 123 | 2 |
| wink 5f5572e6 + the decoder fix below | 8.87, 9.01 | 51 | 123 | 2 |
| pewasm's pin (wink 1f16bf09), same flags | 33.9, 34.3 | 51 | 0 | 49 |

So pewasm's older driver is *slower* on gex's stream (34 against 9 ms a frame: it
submits on every rewrite of a buffer the batch reads). The gap is not Mesa's
version or build: it is **the stream**. pewasm passes client arrays to Mesa
as pointers (st/mesa uploads them through its stream uploader into fresh
ranges); gex rewrites one scratch buffer per array with glBufferData before
every draw, and since the pending batch already reads that buffer the driver
stages each rewrite and copies it inside the batch, which ends the render
pass: 142 uploads, 123 copies, 51 passes for 47 draws.

### Smaller findings

- **The decoder printed ten report lines every frame**
  (`web/mesa/gexgl-mesa.cpp` display_backend: `gex_wgpu_report_draw_targets()`
  sat outside its `if (frames % 600 == 0)` for want of braces): about
  0.2 ms a frame of vsnprintf, stdio and console plumbing (display 0.27 ->
  0.09 ms in the replay). Fixed.
- **The WebGPU call counter in mesa-gl.js ran always**: a performance.now
  pair per call, 2.5% of the compositor worker in F's co1 profile (and 13%
  of the replay's CPU per frame). Now opt-in, `?wgpustats=1`.
- **Per-draw debug labels in the driver**: six snprintf per draw for WebGPU
  object labels (`gex_wgpu_draw_vbo_inner`, "draw %s/%s" and its group and
  layout labels), about 0.3 ms a frame in the profile. Gated behind an env
  variable and replayed (r5): 5.44 and 5.30 ms against 5.13 and 5.32 ms
  without the change, i.e. inside the noise. Dropped; no mesa-wink change.
- **Attachment views are made per render pass** (createView 78 a frame), and
  2-4 render pipelines are created every frame in the live page.
- E4 (SIMD + ThinLTO) was measured by E on the page before: no gain (control
  53.6, 56.8 vs 51.5, 56.9 world fps). By replay of the CLIENT_ARRAY stream
  (r4, 600 frames, two passes): release (gex's flags) 5.45 and 5.47 ms a
  frame; with eject9's mesa-o3.ini (`-msimd128 -flto=thin`, link `-O3`,
  which runs binaryen at link) 5.09 and 4.99 ms, 7% less; a further
  `wasm-opt -O2` over that 5.28 and 5.04 ms (nothing more).

### CLIENT_ARRAY, by replay

The NT layer now sends client arrays as `op::CLIENT_ARRAY` (bdb682d9).
`to-client-array.mjs` rewrites the recorded trace into what the new layer
sends, and the replay (r3, 600 frames, two passes; the last frame's
readback digest equal on both):

| stream | ms per frame | passes | views | writeBuffer | copies | createBuffer |
| --- | --- | --- | --- | --- | --- | --- |
| as recorded (scratch buffers) | 8.17, 8.45 | 51 | 77 | 178 | 123 | 15 |
| CLIENT_ARRAY | 5.32, 5.33 | 15 | 16 | 83 | 0 | 0 |

The counts are now pewasm's live ones (15 passes, 16 views, 87 writeBuffer,
no copies).

## Findings
