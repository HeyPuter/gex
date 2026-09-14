# Session handoff — 2026-09-08

What this session was driving at, what it finished, and what the next one
should pick up. The rolling technical record lives in
[`gl-webgpu-handoff.md`](gl-webgpu-handoff.md); this file is the *goals* layer
over it, and it is deliberately short enough to read before starting work.

The standing instruction the session ran under, verbatim:

> continue with mesa webgpu progress, ensuring performance, reliability, and
> conformance. as many real world apps as possible should work. push through
> any barriers

---

## Long-term goals

These do not change between sessions. They are listed in the order a tie should
be broken, which is *not* the order of how much work is left.

### 1. Real applications run, and look right

The product is GL applications rendering correctly on WebGPU, in a browser.
Everything else is instrumental. Six are verified end-to-end against a
reference: **STK, Blender, Krita, Minecraft, FreeCAD, OBS**. "Verified" means
photographed and compared, not "drew something" — see
`canvas-vs-guest-rendering` for why those are different claims.

The bar for adding a seventh is the same: a picture compared against llvmpipe,
with the difference explained. A shader count is evidence that rendering
happened, not that it was right.

### 2. The browser path is the one that counts

Native Dawn is a development convenience. The browser exercises the JS binding
layer and the guest marshaller, neither of which any native gate touches, and
its limits are *different* — 28 inter-stage variables against 31, 16 storage
buffers against 10, 128 colour bytes per sample against 32. A fix measured only
natively is half-measured. `tests/gl44-browser-sweep.sh` is the widest
statement available and currently reads **193 PASS / 2 FAIL / 2 SKIP**, both
failures platform facts with native twins.

### 3. Conformance as a means, not a score

GL 4.6 is reached and the families are all scored. The remaining value in the
CTS is that it finds *constructs* nobody has exercised, not that a percentage
goes up. The two signals that predict whether an application works are in
`stronger-than-cts`: a refusal by name, and a rendered app whose picture is
wrong. A pass rate predicts neither.

### 4. Reliability: a wrong answer must not be silent

The recurring failure mode in this driver is not a crash, it is a draw that
disappears or a value that is quietly zero. Three separate defects this session
were invisible to every existing test. The standing rule that follows: **when
you add a mechanism, add the instrument that would have caught it being
absent.**

### 5. Performance

`perf-gears` at 211.96 FPS / 4.72 ms per frame, STK at 158.4 FPS. No regression
is acceptable without a measurement saying why. Note that the app gates are
vsync-capped and so cannot show one — `perf-gears.sh` is the uncapped number.

---

## What this session finished

All committed and pushed: mesa → `wink/gex-wgpu`, repo → `origin/gl-on-webgpu`.
Working tree clean apart from the three pre-existing dirty submodule pointers
and untracked `glpuzzle/`.

| | outcome |
| --- | --- |
| **OBS is the sixth verified application** | Was reported NO-DRIVER; both reasons were the harness (a crash marker every `SIGKILL` leaves, and `--minimize-to-tray` rendering no preview). Its frame differs from llvmpipe's by 106 pixels — the `CPU: n%` readout. The preview region's mean difference is exactly 0. |
| **The emulated stages' passthrough reads records as storage** | Was one vec4 *vertex attribute* per varying. Removes the attribute ceiling (output components 116 → 124 natively, and honestly **116 → 112** in the browser), frees the wide-point expansion, and deletes the vertex layout and its slot-compaction mapping. Four stacked defects cleared on the way. |
| **An `instances`-fold overdraw in both emulated stages** | The raster draw inherited `instance_count` on an index list that already covered every instance. Invisible to every opaque test; additive blending makes the pixel count the draws. `gl44-gs-instanced-blend-probe` guards it, verified by disabling each half of the fix in turn. |
| **`gl_PrimitiveID` in a fragment shader** | Was an application-*killing* link refusal. Now carried on a flat varying, derived from `gl_VertexID / verts_per_prim`, with indexed and strip draws de-indexed into lists to reach that shape. `KHR-GL45.shader_image_size` **32/6/36 → 38/0/36**; `gl44-primitive-id-probe` **0/4 → 4/4**. |
| **A multisampled sampler in a compute shader bound nothing** | Fixed, with a probe. |
| **A missing base module names itself** | Instead of dropping the draw silently. |
| **The browser path swept whole** | 193/2/2, and `texture-sampler` — an unexplained expected-DIFF for weeks — closed: WebGPU offers `sampleCount` 1 or 4 and nothing else, so neither driver is wrong. |
| **Shader modules carry a name** | `[ShaderModule "CS:GLSL1"]` instead of `(unlabeled)`. |
| **Diagnosis, not code** | The three 64-bit I/O cases are all *vertex inputs*, one construct. The atomic-counter lead is part-diagnosed with two hypotheses killed. Sixteen other draw-rewrite sites swept and cleared of the overdraw class. |

---

## Short-term goals — what to pick up next

Ranked. Each entry says what it costs and what guards it, because the thing
that goes wrong is picking up an item whose price was estimated once and never
re-measured.

### 1. Label the pipelines and bind group layouts too

**Small, and it pays for itself immediately.** The shader modules are named
now, but the other half of the message is not:

    Binding doesn't exist in [BindGroupLayoutInternal (unlabeled)].
    [Invalid ComputePipeline (unlabeled)] is invalid.

Both descriptors take a `label`. Bisecting *which pass* failed by reading cost
most of an afternoon on the atomic-counter lead below, and will cost the next
one the same until this is done. Do this before the next investigation, not
after it.

### 2. Finish the atomic-counter diagnosis

`shader_atomic_counter_ops_tests` is 3/4, failing `AdditionSubstraction` alone.
The reproducible failure is the *return value*, not the binding error — the
atomic-result replay's answer is not reaching the draw. Read the section in
`gl-webgpu-handoff.md` first: it records two hypotheses already killed
(including one where an A/B looked causal and was luck), so the cost is lower
than it looks.

The binding error is **intermittent against the same binary**, which is the
cache-collision shape — except that the memory recording that class says it is
browser-only. Either that reasoning has a native counterpart or the
intermittency has another cause. Unsettled.

### 3. Audit the six vertex-capture dispatches for missing SSBO binds

Noticed while reading, **not** the cause of the above, and worth its own look:
of six dispatches, only `gs_draw` and `atomic_replay_draw` call
`gex_wgpu_bind_stage_ssbos`. `line_draw`, `tess_draw`, `capture_positions` and
`flat_unfilled_draw` do not. That is the shape of a latent gap — a vertex
shader with a storage block replayed through one of those four would lose it —
but no failing case points at it, so it needs a probe to become real rather
than a fix on suspicion.

### 4. Carry `gl_PrimitiveID` out of the emulated stages

The feature is complete for plain draws. A draw *through* a geometry or
tessellation shader still reads **0**, because those stages leave the carrier
unwritten. They already compute the primitive number exactly
(`gex_wgpu_gs_builtin`), so this is plumbing rather than a mechanism: write it
into the carrier slot and let the passthrough forward it. Guards:
`gl44-primitive-id-probe` would need a geometry case adding.

### 5. 64-bit shader I/O — three cases, one construct

`vertex_attrib_64bit.vao`, `.limits_test` and `fp64.varyings` are the same
thing. The template exists (`gex_wgpu_lower_vertex_attrib_unpack`, which serves
the 2_10_10_10_REV formats), but `dvec3`/`dvec4` need six and eight `uint32`
components — past `Uint32x4` — so each needs **two vertex elements**, and that
machinery does not split. A partial fix covering `double` and `dvec2` moves
neither case, so it is all-or-nothing.

Low priority on its own merits: GPUs handle doubles slowly, so applications
convert on the CPU.

### 6. `MAX_VERTEX_STREAMS` 1 → 4

GL 4.0 asks for four. The geometry stage is already a compute pass writing
buffers, so four sets of buffers is the shape, with the rasterized draw taking
stream 0. Rare in applications; parked with its shape written down.

### 7. `copy_image` — 324 cases, ~7 hours, unattended only

**~78 seconds per case**, measured, with zero failures in the 43 cases run
across two attempts. The only family in the suite that deserves the
"prohibitive" label. Worth finishing once, overnight, to close it — not worth
blocking anything on.

---

## Walls that are not ours

Do not spend time on these without new information. Each was measured, not
assumed.

- **`workgroupBarrier` under storage-derived control flow** — WGSL forbids it.
  Blocks Blender's EEVEE viewport. Two other EEVEE blockers sit behind it, so
  lifting this alone would not help.
- **`early_fragment_tests`** — WGSL has no attribute, so Tint drops it.
- **`precise` / NoContraction**, **Tint has no 16-bit types**, **subgroup
  vote** (the invocation group is one invocation).
- **Inter-stage variables**: 31 native, 28 browser, where GL wants 32. This is
  behind *three* of the four `limits` shortfalls on each platform.
- **The 32-byte colour budget** is **native only** — the browser grants at
  least 128. Re-run a family in the browser before investigating any
  "attachments exceed 32 bytes per sample".
- **`sampleCount` 1 or 4 and nothing else**, which is why `GL_MAX_SAMPLES` is 4.

---

## How to not lose a day

The traps this session actually hit, in the order they cost the most time.

1. **A variant reason must appear in *both* of `gex_wgpu_fragment_module`'s
   early returns.** They hand back `fs->module`, which is NULL for a deferred
   shader, so the draw vanishes with no message. `gl_PrimitiveID` was the third
   feature to hit this. `grep -n 'return fs->module'` before adding one.
2. **`GEX_WGPU_DRAW_DROP=1` turns "the value is wrong" into "the draw did not
   happen"**, which is a different investigation. Reach for it early.
3. **A driver-declared storage block is renumbered, reshaped and mis-grouped**
   by three passes written for application blocks.
   `gex_wgpu_driver_owned_block` is the predicate; set `descriptor_set`
   yourself.
4. **A compute variant has only group 0.** A set-1 binding on a vertex shader
   must be rewritten in the clone and bound at six dispatch sites.
5. **An overdraw is invisible to every opaque test.** A path that replays or
   expands geometry needs one additively-blended case.
6. **Count *all* `WebGPU error` lines**, never grep for the message text. An
   A/B that looked causal this session was luck on both sides; the full count
   showed the same 3 errors in both builds.
7. **`libdril_dri.so` is a stub.** The driver is
   `libgallium-26.3.0-devel.so`. Checking the wrong file's timestamp made
   several clean builds read as stale.
8. **Build from the repo root.** `./web/build-mesa-native.sh` from anywhere
   else silently does nothing, and `grep -iE ' error|FAILED'` does not catch
   "No such file or directory".
