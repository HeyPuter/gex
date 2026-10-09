# The browser GL path: cost against pewasm's (stream GL)

Stream GL closes the GL/compositor cost gap between gex's browser page and
pewasm's (OPTIMIZATION-BACKLOG.md E1-E4, E10, W1). Branch `merge/glpath`,
worktree `wt/glpath`, scratch `~/gex-sandbox/scratch/GL` (`$G` below).

## Resume

Final state (2026-10-08). Stream GL is closed. It was meant to be transient,
and this section is what a later stream needs to pick it up.

**Landed on merge/glpath** (gate head 4955a441; for gex master):
- E2 diagnosis, CLIENT_ARRAY (bdb682d9), the decoder report fix and opt-in
  WebGPU counters (21baa051), E3 map rules (bcc93e30), five more fast-path
  GL calls (430acec7), E4 cross file (c5e6dddb: SIMD, ThinLTO, -O3 link).
- Host op timing and `crates/gex-gl/examples/replay.rs` (4fda5dd0).
- The guest's framebuffer state-query cache (85c3bb78,
  tests/gl-state-query-cache.sh).
- Desktop `#version` 410-460 fitted to the context's GLSL (707e84b8, a
  stopgap until the page's context is 4.6).
- Async present at swap interval 0 (7605de5c).
- The native host makes compatibility contexts for core requests
  (c39706ba; fixes gl-map-buffer 0/3 and native Muck).
- The decoder's full 4.1-4.6 requirement report (e204fa1b; its device-limits
  request is backed out by 32f58c4e).
- Serial host GL tests (7d7e45a8).

**On mesa-wink `gl/perf` in `$G/mesa-wink`, awaiting the user's approval to
push** (rebased onto upstream wink 0dd84167; nothing pushed):
- 062f46a0 (was 6d3a4cfd): the pipeline cache's own epoch.
- 523362aa: zink/ntv never lets Tint fold -0.0 into 0.0.
- 60202246: the gex-wasm targets compile webgpu.cpp as C++20 (needed by
  emdawn v20250822).
- 9f1da0fa was dropped in the rebase, because upstream's `handle_epoch` is
  the same fix. The pre-rebase branch is `gl/perf-5f5572e`.
- HK on the rebased tree is not slower (repeat: gpu 4.99 vs gpn 5.39 ms per
  frame, inside the noise); see the push report under "mesa-wink gl/perf
  rebased".
- gex's pin (third_party/mesa) is still wink 5f5572e6. Moving it also needs
  web/build-mesa-wasm.sh on emdawnwebgpu v20250822 (wink/build.sh fetches it).

**Open items:**
1. *Device limits leave black silhouettes.* With the adapter's limits
   (e204fa1b's web/src/gpu-device.js and mesa-wgpu-replay.html) the context
   is GL 4.6, but Muck's 3D scene is black with a cyan sea (U mp26, and mp28
   with the -0.0 fix: 0 Tint rejections, 30 presents/s against 47 at GL 4.0).
   This is not rejected programs. Suspect Unity's 4.x lighting/shadow
   variants (compute or image paths that only 4.3+ enables).
   Resume:
   - capture a GL 4.6 Muck trace (`?glcapture=`);
   - replay it with `replay-run.mjs gpu TRACE --query readback=1`;
   - bisect the shader variants with `shaders.mjs`;
   - then re-apply e204fa1b's two web files.
2. *Grass:* fixed at GL 4.2 by GL 4.1's texture limits with mesa-wink
   60202246 (U mpg3: blades, 36.6-38.1 presents/s). gex's pin is 60202246
   and `?gllimits=41` is the default since U's pin commit on merge/unity
   (`?gllimits=0` turns it off; 43 stays per run, item 1).
3. *Noita's world fps* is bound by its job hand-off, not by GL: the main
   thread is parked about 37% waiting on job threads, and KLock is 0.14%
   (stream F's area). The page's GL submit is no longer the limit
   (CLIENT_ARRAY: 55.5 vs 51.1 world fps on the old path, pewasm 47.2).
4. Remaining E1: GL draw/pointer calls are on the fast path, but the page
   gain is below noise.
5. *The 4-slot bind-group repro on the rebased tree:* done (U,
   2026-10-09, `scratch/unity/urep-pin.sh` on edger, run like trep2.sh:
   full trace, `--query stream=1`, timeout 1500): mods/tinyu replays all
   2,777 frames (3,403 batches) of Noita's trace with 0 validation errors
   (5f5572e6 and +6d3a4cfd had 2,668 and 2,720 from frame 1343).

Tooling (unchanged):

- Live A/B harness for the ejected page: `$G/patch-site.mjs SITE PAGE OUT
  LAUNCHER` swaps a site's page (shim, kernel, Mesa, JS) for one built by
  web/build-eject-page.sh, keeping its units; the page must come from the
  tree the site's units were built at (site30: wt/eject9 52bb4b96, or the
  NT trampolines' unit misses). `wt/gl-e9` (branch glpath-e9) = 52bb4b96 +
  this stream's commits; `bshim-e9.sh` builds its shim/kernel/seed/gexc.
  `glegs.sh TAG LEG...` (pew | SITE[@ENV]) measures world fps and the
  compositor's gex_gl_submit ms per frame.
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

### CLIENT_ARRAY, live (ejected page, site-gl2 = site30 + this stream's
page; the old path by `GEX_NT_GL_CLIENT_ARRAYS=buffer` on the same site)

| leg order | CLIENT_ARRAY | scratch buffers (old) | pewasm |
| --- | --- | --- | --- |
| j2: A, B, pew | 67.2 fps, 4.64 ms | 64.0 fps, 7.03 ms | 48.5 fps |
| j4: pew, B, A | 65.2 fps, 4.99 ms | 65.8 fps, 6.88 ms | 49.4 fps |
| mean | 66.2 fps, 4.82 ms | 64.9 fps, 6.96 ms | 48.95 fps |

ms = the compositor's gex_gl_submit wall time per produced frame (world-up
+90 s to 330 s; one submit a frame). The compositor's time per frame falls
by 31% (6.96 -> 4.82 ms); world fps moves by 2% (66.2 against 64.9),
inside the legs' spread: as eject-pe.md found, the compositor worker is not
the page's critical path (the main thread is). The ejected page with
CLIENT_ARRAY is 1.35 times pewasm's world fps (66.2 against 48.95).

### What still makes NT syscalls (GEX_NT_GL_STATS, site-gl2, per frame)

623 GL calls a frame reach the layer (the eject9 tree has no client-state
fast path, which master has: glEnable/DisableClientState are 284 of them
there). The rest: glDrawArrays 47, glVertexPointer/glColorPointer/
glTexCoordPointer 142 (pure layer state), glBlendFunc 39, glBindFramebuffer
18, glUniform1f 14, glTexParameteri 12, glViewport 9, glBlendFuncSeparate 8,
glTexSubImage2D 5, wglSwapBuffers 1. 430acec7 moves glBlendFunc,
glBlendFuncSeparate, glViewport, glTexParameteri and glUniform1f to the
fast path. The pointer calls need the layer's array state in guest memory
(as GL_FAST_ARRAYS holds the enables) before the provider can take them.

### After CLIENT_ARRAY (replay profile p3)

About 5.5 ms of CPU a frame: writeTexture 0.85 ms (the five world-pixel
uploads, 839 KiB; pewasm's page pays 0.52 ms for the same), native WebGPU
time 1.1 ms, the rest spread thin over st/mesa and the driver. The largest
named item left is the per-draw label formatting (about 0.23 ms a frame in
the profile; not separable from noise by replay).

## Findings

## Unity's GL stream (Hollow Knight's menu, 2026-10-07 night)

Captured natively on edger (`GEX_GL_CAPTURE` on U's harness:
`tests/games/unity-bench.sh hk ... --fjit`, warm fjit cache; trace
`out/hk-menu.gexgltr`, 587 MB, 3166 frames; the run's menu was 25.6
presents/s with the capture on). Per frame over the last 300 (`ops.mjs`):

- one submit (the frame's PRESENT; GEXGL_SUBMIT_TRACE agrees: 81% of
  submits follow PRESENT), 999 commands, 78 KiB;
- 45 draws (35 glDrawElementsBaseVertex, 10 glDrawElements), 47
  glBindFramebuffer, 186 glBindBuffer, 160 glTexParameteri, 92
  glBindTexture, 58 glUniform4fv;
- 77 glBufferSubData, 58 KiB, all to GL_COPY_WRITE_BUFFER, 53 of them under
  256 bytes (per-draw uniform blocks); 54 rewrite a buffer a draw of the
  same frame already read, 14 at unaligned offsets (`subdata.mjs`).

**Natively the GL stream is not what limits the menu.** The same trace
replayed on edger's NVIDIA through gex-gl's host (`gl-replay-native`, the
new `crates/gex-gl/examples/replay.rs` with per-opcode timing): 0.84 and
1.03 ms a frame in `Gl::run`, of which glBindFramebuffer is 0.30-0.39 ms
(47 binds) and the draws 0.15-0.20 ms; the whole 3166-frame trace replays in
under 3 s. One round trip a frame. At 25-28 presents/s (35-40 ms a frame)
that is a few percent: U's thread profile (main thread 59-69% of a core,
render thread 25%, no thread saturated) is the place to look, not gex-gl.

**In the browser (Mesa on WebGPU, the same trace, replay page) it is waste,
and fixable in the driver.** Base (wink 5f5572e6 + E4 flags): 4.65 and 4.71
ms of CPU a frame, with 23.5 queue submits, 42 render passes for 45 draws,
12 createRenderPipeline, 66 views, 32 copyBufferToBuffer and 6
copyTextureToTexture a frame. Causes found:

1. Pipelines rebuilt every frame: the pipeline cache's entries were
   invalidated by an epoch it shares with the bind group cache, which moves
   on every deleted sampler view or sampler state (Unity deletes some every
   frame).
2. Unaligned glBufferSubData into a buffer the batch reads submitted the
   batch (in-batch staging took whole words only).
3. Feedback snapshots (a draw sampling its own attachment) went through
   copy_texture_region, which submits the batch and then its own command
   buffer: two submits each.
4. Every rewrite of a uniform buffer the batch reads was staged and copied
   in the batch, ending the render pass, although the bytes written are
   never the bytes a pending draw reads.

Each was tried as a mesa-wink commit and replayed (r=urep3, last 600
frames, CPU ms of gex_gl_submit per frame, two passes in and reversed order;
the last frame's readback digest equal for every build):

| build | ms a frame | submits | pipelines | copies (buffer) |
| --- | --- | --- | --- | --- |
| base (5f5572e6 + E4) | 5.00, 4.72 | 23.5 | 12 | 32 |
| + pipeline epoch | 4.42, 4.16 | 23.5 | 0 | 32 |
| + unaligned staging | 5.34, 4.82 | 12 | 0 | 44 |
| + snapshots in the batch | 5.39, 5.79 | 2 | 0 | 54 |
| + read/write ranges | 5.30, 5.44 | 2 | 0 | 54 |

Only the pipeline epoch pays: 12% less CPU a frame and no pipeline
creation (which also costs Dawn's GPU-process time the CPU column does not
show). Fewer submits bought by staging more copies is a loss here, and the
range check never applied (Unity's rewrites hit buffers also bound as vertex
or index data, which count as whole-buffer reads). Kept: `$G/mesa-wink`
branch `gl/perf` = 6d3a4cfd only (not pushed; needs the user's approval);
the other three are on `gl/experiments`.

## Noita: E1 on the ejected page, the normal page, and what limits world fps

**E1 (fast-path draws and pointers), ejected page** (site-gl4 = site-gl3 +
F's client-state fast path + 6dd6b3fc; ABBA j7/j8): site-gl4 65.0, 65.1
world fps against site-gl3 70.8, 65.6 and pewasm 48.0, 48.4. No measurable
gain: the main thread is not bound by the remaining GL syscalls.

**What limits world fps (ejected page, worker profiles 100-120 s, site-gl3,
`out/prof-gl3`):** the compositor worker is 66% idle. Noita's main thread is
fully busy, 37% of it parked in `guest_yield` (NtHost::park: waiting for its
job threads), the rest guest code and NT calls (mono_ns clock reads 2.4%,
dlmalloc 1.2%, syscall glue). The seven job workers each spend 55% in the
ConcRT condvar provider (`cond_wait_`) and ~13% in f_724580 (the world
simulation). So the frame is bound by the hand-off between the main thread
and the simulation workers, not by GL and not by one thread's code: the
next lever is the guest's own threading (F's side: wake latency of
cond_wait/park, and the simulation's per-frame critical path).

**The normal gexc page** (tests/nt/browser-noita.mjs through
`$G/np/nplegs.sh`: wt/glpath at merge/glpath, units built at the branch's
keys into target/bm-gl (`$G/build-bm.sh`, seeded from F's bm-s objects:
all 3127 noita.exe objects reused), seed pinned, 4x clock; world fps from
the stats lines, world-up+30 s to the end; legs n1/n2 ran 213 s with F's
default worker profile at 60-90 s, n3/n4 only ~50 s because `PROF=none`
trips browser-noita's AOT-share check, which then ends the run):

| arm | n1 | n2 | n3 | n4 | mean |
| --- | --- | --- | --- | --- | --- |
| CLIENT_ARRAY (default) | 48.6 | 59.8 | 61.0 | 52.5 | 55.5 |
| `GEX_NT_GL_CLIENT_ARRAYS=buffer` | 49.3 | 49.9 | 48.9 | 56.1 | 51.1 |
| pewasm | 45.9 | 45.2 | 49.5 | 48.0 | 47.2 |

The normal page is 1.18 times pewasm's world fps (55.5 against 47.2), and
the new path's mean is 9% above the old one's, but the legs spread over
48-61 world fps, so that difference is not established. A proper ABBA
needs longer legs without the profile and with the AOT check satisfied
(keep a short profile window, or relax NOITA_MIN_AOT for such runs).

## Unity in the page: the bind-group error, the grass, and synchronous submits (2026-10-08)

**"Bind group layout ... does not match layout ... of bind group" (Muck's
page blanks at 500-700 s).** gex_wgpu skips setBindGroup/setPipeline when
the handle equals the one it last set on the open pass (`ctx->emitted`).
emdawnwebgpu handles are heap addresses; a bind group evicted from the cache
is released one draw later (a pipeline at once), and the next object made can
get the same address, so the draw skipped binding it and ran with the old one,
built for another program's layout. The epoch that used to reset
`ctx->emitted` stopped moving on eviction (for the cache's hit rate).
Fix: a release serial that resets it, mesa-wink 9f1da0fa on `gl/perf` (not
pushed). Repro: the same driver with a 4-slot bind group cache, replaying
Noita's trace: 2668 (wink 5f5572e6) and 2720 (+6d3a4cfd) validation errors
from frame 1343, 0 with the fix; so 6d3a4cfd neither causes nor fixes it.
Muck's natively captured menu (29,466 frames) replays clean at stock size.
U's page run with the fix (mp18): 0 errors over 572 s, presents past 500 s.

**The grass (dots instead of blades).** gex_wgpu emulates geometry shaders;
what failed is the shader version: st/mesa caps GLSL at the GL version
gex_wgpu earns (4.0), Unity writes `#version 410` for the grass's geometry
stage (460 natively). 707e84b8 compiles such sources as 400 with the
4.1-4.6 ARB extensions enabled (all of Muck's native 460 shaders then
compile). U's page run (mp19) still shows dots; its log has a Tint
rejection, "index 2147483648 out of bounds", in a fragment shader at menu
start: a driver SPIR-V emission bug, being chased with the shader's hex.

**Synchronous submits.** Muck's native menu: 10 submits a frame, 8 of them
glGetIntegerv(GL_SAMPLES / GL_READ_BUFFER), 1 glGetTexParameteriv
(GL_TEXTURE_IMMUTABLE_FORMAT), 1 the present. In the page each is a round
trip to the compositor (U: render thread 48% in gl_submit). 85c3bb78 caches
framebuffer state queries in the guest library (tests/gl-state-query-cache.sh;
21 submits -> 12 in the probe); 7605de5c submits a swap at interval 0
without waiting (GEX_GL_SUBMIT_ASYNC, already served by both hosts).
Page numbers pending (U).

**KLOCK and Noita.** In the ejected page's profile (site-gl3) KLock::take is
30 ms of Noita's main thread's 20.9 s (0.14%) and 8 ms on a job worker:
Noita's GL submits are asynchronous already (the NT layer's PRESENT batch),
so U's KLOCK change cannot move it.

Native note: with master's guest GL (and this branch's), Muck natively
segfaults U's edger gexc (10-07) and shows no frame within 420 s with a
current gexc; U's 10-04 gl64 works. Reported to U.

## GL 4.6 in the page, native core contexts, Tint's negative zero (2026-10-08)

- **Why the page's context was GL 4.0.** Every 4.1-4.6 extension is there;
  what failed were constants, because the page's WebGPU device kept WebGPU's
  default limits (8192 texels, 256 layers, 256 compute invocations, 16 KiB of
  workgroup memory): GL 4.1 wants 16384/2048, GL 4.3 1024/32 KiB (the
  decoder's report now lists every unmet requirement to 4.6). Asking for the
  adapter's limits (e204fa1b) makes the context 4.6 -- and then Muck's 3D
  scene drew black (U, mp26), so 32f58c4e backs the request out until that is
  understood. 707e84b8's version fit covers 4.1-4.6 sources meanwhile.
- **Native core contexts (c39706ba).** Since 6230d6f4 the native host gave a
  core-profile request a core context, which refuses the guest library's own
  object names: tests/gl-map-buffer.sh 0/3, native Muck drew nothing. The host
  makes a compatibility context of the requested version; the guest reports
  the profile asked for. E3 (bcc93e30) was not involved.
- **Tint's negative zero.** Tint dedups scalar constants by value and
  -0.0 == 0.0: Unity's fragment shader has vec2(-0.01, -0.0) and indexes a
  34-entry uniform array with floatBitsToUint(0.0), which Tint folded to
  0x80000000 and rejected (Muck's page, 5-7 programs). mesa-wink gl/perf
  523362aa (on upstream wink 0dd84167) emits such constants through ntv's
  unfoldable path. Upstream wink independently fixed the emitted-handle bug as
  `handle_epoch` (same design as 9f1da0fa, which the rebase drops).
- **Muck's page with the guest GL changes (U, mp23):** 46.8-47.6 presents/s
  against 24.2-25.3 (mp19); the async present at swap interval 0 is nearly
  all of it.

## mesa-wink gl/perf rebased onto upstream wink 0dd84167 (2026-10-08)

- **Rebase.** One conflict: gex_wgpu_screen.c, where upstream's
  `handle_epoch` met gl/perf's pipeline epoch. Both are kept: upstream's
  resets the emitted handles, and `pipeline_epoch` keys the pipeline cache.
  9f1da0fa was dropped, because upstream's `handle_epoch` is the same fix.
- **Build.** The rebased tree builds against emdawnwebgpu v20250822, the
  first release with WGPUTextureFormat_R16Unorm. Its webgpu.cpp uses
  std::span, so the gex-wasm targets need C++20 (60202246).
- **Checks on the rebased build (mods/gpu):**
  - browser-gui --gl, composited x86 and x64: 131 and 134 frames, 0 failures.
  - browser-gui --gl with direct=1, x86 and x64: 198 and 197 frames,
    0 failures.
  - softpipe gl-replay on tests/gl-traces: gears 210 batches PASS,
    program-setup 3 batches PASS.
- **Hardware replays (edger, `urep4.sh`).** Before is gpn (gl/perf on wink
  5f5572e6 with the -0.0 fix); after is gpu (rebased).
  - Correctness: identical readback digests on both traces (Noita 92f095c5,
    HK 9eece5c5), with 0 rejects and 0 validation errors in each.
  - Noita world trace, CPU ms per frame over the last 600 frames: gpn 5.90
    and 4.78, gpu 5.04 and 5.20. No change outside the noise.
  - HK menu trace, same measure: gpn 5.03 (its second leg hung until the
    timeout), gpu 6.24 and 6.61. That looked up to 25% slower, but the
    readback pass pointed the other way (gpn 7.07, gpu 6.71 ms). The repeat
    below (`hkbis.sh`) does not reproduce it.
- **HK repeat and bisect (edger, `hkbis.sh`, out/hkbis.out).** HK menu
  trace, 600 frames, median CPU ms per frame, legs ABBA-ordered
  (gpn b0 b1 b2 b3 gpu gpu b3 b2 b1 b0 gpn); b0-b3 are plain upstream wink
  (no gl/perf commits; + C++20, emdawn v20250822) at 5f5572e6, 686e897a,
  2e94fadd, 066fbd73. All legs ok, 0 rejects, 0 validation errors.

  | build | leg 1 | leg 2 | mean |
  |---|---|---|---|
  | gpn (gl/perf on 5f5572e6) | 5.165 | 5.605 | 5.39 |
  | b0 upstream 5f5572e6 | 6.985 | 5.750 | 6.37 |
  | b1 upstream 686e897a | 5.855 | 6.625 | 6.24 |
  | b2 upstream 2e94fadd | 5.835 | 5.495 | 5.67 |
  | b3 upstream 066fbd73 | 6.780 | 7.195 | 6.99 |
  | gpu (gl/perf on 0dd84167) | 4.790 | 5.185 | 4.99 |

  The rebased gl/perf is not slower on HK: gpu is 7% faster than gpn here
  (4.99 vs 5.39 ms/frame), which is inside the noise (one build's two legs
  differ by up to 1.2 ms, b0). The upstream points do not step in one
  direction (5.67-6.99 ms), so no upstream commit is implicated; urep4's
  6.24/6.61 gpu legs were noise. What the run does show: on either base,
  gl/perf's own commits are cheaper than plain upstream (gpn 5.39 vs b0
  6.37 ms, 1.0 ms less, same base; gpu 4.99 vs b3 6.99 ms), consistent with
  the pipeline cache keeping its own epoch (062f46a0).
- **urep5 (the 4-slot bind-group repro on the rebased tree, `urep5.sh`)
  gave no result**: it held the bench lock 14:29-14:49, hit its 1200 s
  timeout with `maxframes=2000`, and wrote nothing (empty out/urep5.out, no
  log). The rebased tree's bind-group fix is upstream's `handle_epoch`, not
  ours; the stock-size Noita and HK replays above show 0 validation errors,
  but the small-cache repro has not been run on it (open item 5).

### Push report (nothing pushed; awaiting the user)

- **Upstream pulled into gl/perf:** all 78 commits of upstream wink
  5f5572e6..0dd84167 (origin/wink is still 0dd84167 as of 2026-10-08).
- **Conflicts:** one, gex_wgpu_screen.c: upstream's `handle_epoch` (resets
  the emitted handles on release) met 6d3a4cfd's `pipeline_epoch` (keys the
  pipeline cache). Both kept (062f46a0). 9f1da0fa dropped as a duplicate of
  `handle_epoch`.
- **What would be pushed:** new branch `gl/perf` on
  MercuryWorkshop/mesa-wink (no gl/* branch exists there), head
  602022460540b75b41d5e97f2c41207dbd79ce17, base wink
  0dd84167fcd2b54c1d84e02ce2e246637ffc17ab, three commits, fast-forward
  from wink (3 files, 29 insertions, 6 deletions):
  - 062f46a0084306d9a2a6d5703c78c438a1864d9f gex_wgpu: the pipeline cache
    keeps its own epoch, moved only by shader deletion;
  - 523362aa82e79ab40a4e9c4a5ec720f198113fd9 zink/ntv: negative zero never
    becomes a constant Tint can confuse with zero;
  - 602022460540b75b41d5e97f2c41207dbd79ce17 gex-wasm: the replay and smoke
    targets compile webgpu.cpp as C++20.
- The pre-rebase branch `gl/perf-5f5572e` (bc42c2dd) stays local only.
