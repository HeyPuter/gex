# Session handoff — 2026-09-14

Supersedes [`session-handoff-2026-09-08.md`](session-handoff-2026-09-08.md),
whose short-term list is now five-sixths done. The rolling technical record is
[`gl-webgpu-handoff.md`](gl-webgpu-handoff.md); this is the *goals* layer over
it, kept short enough to read before starting work.

The standing instruction, verbatim:

> continue with gpu work as a long term task. this session will be running,
> mostly unattended, for probably a week or so.

over the older one it inherits:

> continue with mesa webgpu progress, ensuring performance, reliability, and
> conformance. as many real world apps as possible should work. push through
> any barriers

---

## Long-term goals

Unchanged, and listed in the order a tie should be broken — which is *not* the
order of how much work is left.

### 1. Real applications run, and look right

**Seven are verified end-to-end against a reference**: STK, Blender, Krita,
Minecraft, FreeCAD, OBS and — as of today — **VLC**. "Verified" means
photographed and compared, not "drew something".

VLC was added for the workload rather than the name: its `gl` output uploads
YUV planes as textures every frame and colour-converts them in a shader, and
none of the other six have a streaming-upload path. That is the bar for an
eighth as well — prefer an application whose *renderer* is one nothing else
covers, and one that can be driven from the command line to a known frame.
Kdenlive was tried today for Qt Quick and is not verified; it reaches the
driver cleanly and stops on its own welcome screen, identically on llvmpipe.

### 2. The browser path is the one that counts

Native Dawn is a development convenience. The browser exercises the JS binding
layer and the guest marshaller, and its limits are *different* — 28 inter-stage
variables against 31, 16 storage buffers against 10, 128 colour bytes per
sample against 32. A fix measured only natively is half-measured.
`tests/gl44-browser-sweep.sh` is the widest statement available and reads
**197 PASS / 2 FAIL / 2 SKIP**, both failures platform facts with native twins.

Two things it needs first, and forgetting either measures the old driver:
`./web/build-mesa-wasm.sh`, then `cd web && npm run build`.

### 3. Conformance as a means, not a score

GL 4.6 is reached and the families are scored. `copy_image` — the one family
the last list called prohibitive — ran **324/324** today, so nothing in the
suite is unrun for cost reasons any more. The remaining value in the CTS is
that it finds *constructs* nobody has exercised. A pass rate predicts neither
of the two signals that matter (`stronger-than-cts`): a refusal by name, and a
rendered application whose picture is wrong.

### 4. Reliability: a wrong answer must not be silent

Still the recurring failure mode, and today was three of them in one shape —
see `capture-paths-are-a-class`. **When you add a mechanism, add the instrument
that would have caught it being absent**, and make sure the instrument can
fail: three probe cases written today passed against the driver that had the
bug, because the wrong answer drew the same picture.

### 5. Performance

`perf-gears` at **214.5 FPS / 4.66 ms per frame** (was 211.96 / 4.72), STK at
158.4 FPS. The app gates are vsync-capped and cannot show a regression;
`perf-gears.sh` is the uncapped number and wants an idle machine.

---

## What the last session finished

All pushed: mesa → `wink/gex-wgpu`, repo → `origin/gl-on-webgpu`.

| | outcome |
| --- | --- |
| **Every pipeline, layout and bind group is labelled** | And it closed the atomic-counter lead in one run: the message went from `[BindGroupLayoutInternal (unlabeled)]` to `"tess vs capture g1"`. Labelling was measured to be inert — a labels-only build scores identically. |
| **The capture paths bind their storage blocks** | Four of six dispatches never did, so a vertex shader with an SSBO lost the whole command buffer through tessellation, wide lines, flat-unfilled and the position capture. `gl44-capture-ssbo-probe`: HEAD **1/7**, now 7/7. |
| **`gl_PrimitiveID` through tessellation** | The last shape of it that read 0. Geometry already worked. `gl44-primitive-id-stages-probe` 4/4, ids matching llvmpipe exactly. |
| **`ARB_shader_draw_parameters` in a captured shader** | `gl_BaseVertex` and `gl_DrawID` were silently wrong — and two of the four had an answer already written that was *wrong by construction*. `gl44-capture-drawparams-probe`: HEAD **2/7**, now 7/7. |
| **64-bit vertex inputs link, and draw** | `vertex_attrib_64bit` 2/4 → 3/4. Lifting the refusal exposed a capture path that **aborted the process**; it declines by name now. |
| **`web/build-mesa-native.sh` installs** | It ran `ninja` and stopped, while every native gate reads `$OUT/inst`. A rebuild "succeeded" and the run under it measured the previous driver. |
| **`tests/gates/regress.sh`** | One command for the probe suite and six CTS families against recorded baselines. |
| **`copy_image` 324/324** | Five and a half hours, nothing found, and now it need not be run again. |
| **VLC verified; Kdenlive attempted** | Seventh application, rmse 0.13%. |

---

## Short-term goals — what to pick up next

### 1. An eighth application

The highest-value item, because goal 1 is the product. Pick for the *renderer*:
what is not yet covered is a Qt Quick application (Kdenlive stalls on its own
startup — a different one may not), a GTK4/GSK application, and a game engine
other than STK's. Prefer one drivable from the command line to a known frame.

### 2. Finish the 64-bit capture, or close the item

`vertex_attrib_64bit.vao` is the one case left and it wants the transform
feedback capture to fetch eight words and pack per component — which needs
`gex_wgpu_capture_layout` to carry the element's *component count*, because
the element's own format says `r32g32b32a32_uint` whatever the declaration
was. Small, now that the shader half is in. Still low priority on its own
merits: applications convert doubles on the CPU.

### 3. `MAX_VERTEX_STREAMS` 1 → 4

The only untouched item from the previous list. GL 4.0 asks for four. The
geometry stage is already a compute pass writing buffers, so four sets of
buffers is the shape, with the rasterized draw taking stream 0. Rare in
applications; parked with its shape written down.

### 4. Keep hunting the silent class

`capture-paths-are-a-class` names what is covered and what is not. Audited and
already correct: textures, images, packed images, uniform blocks and buffer
textures, for the vertex captures *and* the emulated stages. The places left to
look are the emulated stages' own equivalents of the three defects found today.

---

## Walls that are not ours

Do not spend time on these without new information. Each was measured.

- **`workgroupBarrier` under storage-derived control flow** — WGSL forbids it.
  Blocks Blender's EEVEE viewport, with two other EEVEE blockers behind it.
- **`early_fragment_tests`** — WGSL has no attribute, so Tint drops it.
- **`precise` / NoContraction**, **Tint has no 16-bit types**, **subgroup
  vote** (the invocation group is one invocation).
- **Inter-stage variables**: 31 native, 28 browser, where GL wants 32. Behind
  *three* of the four `limits` shortfalls on each platform.
- **The 32-byte colour budget** is **native only** — the browser grants at
  least 128. Re-run a family in the browser before investigating one.
- **`sampleCount` 1 or 4 and nothing else**, which is why `GL_MAX_SAMPLES` is 4.

---

## How to not lose a day

The traps this session actually hit, worst first.

1. **`web/build-mesa-native.sh` did not install.** Fixed, but the general form
   stands: check the *installed* driver's timestamp, and check
   `libgallium-26.3.0-devel.so` — `libdril_dri.so` is a stub.
   `tests/gates/regress.sh` verifies this before it measures anything.
2. **An A/B switch inside a shared helper disables the callers that predate
   the fix**, so "off" is a third behaviour rather than HEAD's, and the fix
   reads as non-causal. Name *one* site. And build **pristine HEAD into a side
   prefix** (`GEX_MESA_NATIVE_OUT=/tmp/gex-mesa-head`, then `GEX_INST=...`) —
   that is what settles it.
3. **A probe case that cannot fail.** A *sized* storage block never reaches the
   emitter, so there is no binding to mismatch; a value that is already the
   default cannot be measured. Make the wrong answer move something, then
   knock the fix out at one site and require the case to fail.
4. **Run every probe against llvmpipe first.** An outline sampled on the
   viewport's own edge row is ambiguous; the reference draws nothing there and
   the case fails on the *reference*.
5. **A trailing `&` inside a Bash tool call** makes the harness report the
   wrapper's exit, not the job's. Use the tool's own background mode.
6. **Count *all* `WebGPU error` lines**, never grep for the message text.
7. **A gate must refuse to give a verdict it cannot support.** Two runs that
   stop on differently sized dialogs compare at rmse 24681, which prints as
   "rendering differs from llvmpipe" and accuses the driver of nothing.
