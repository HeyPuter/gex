# GL-to-WebGPU handoff

> ## !!! READ FIRST: MobileGL is not a reference implementation !!!
>
> **MobileGL does not render SuperTuxKart.** Not partially, not "the menu but
> not the race" -- it is fully broken for STK. It nevertheless starts, presents
> at a clean 60 FPS and reports healthy counters, which makes it *look* like a
> working control. It is not one.
>
> **The only backend that renders STK is virgl** (`GALLIUM_DRIVER=virpipe` ->
> virglrenderer; the `virgl` / `wayland` demos). When you need a known-good
> frame, or need to establish whether the application itself reaches some
> state, use virgl.
>
> Sections further down this document contain differential reasoning against
> MobileGL ("N of this message here, zero on MobileGL, so the defect is
> Mesa-specific"). **Those comparisons are unsound and their conclusions must
> not be trusted.** They are kept only as a record of what was tried. Do not
> add new ones, and do not re-enable or re-adopt MobileGL as a baseline.
>
> See also the guards in `web/src/mobilegl.js`, `web/src/demos.js` and
> `tests/stk-webgpu-fps.sh`.

Workspace: `/home/velzie/src/gex2`

Active goal: build a conformant OpenGL 4.4 to WebGPU path, then achieve stable real-browser 60 FPS in SuperTuxKart.

## Current direction

- Production architecture is stock guest GL transport -> Mesa `st/mesa` -> NIR -> the `gex_wgpu` Gallium driver -> WebGPU.
- MobileGL is no longer treated as the production implementation.
- STK is an acceptance workload, not the debugging method.
- Do not run STK or benchmark FPS yet. Rendering correctness is demonstrably unresolved.
- Keep all unproven Gallium capabilities disabled.
- No ThinLTO is enabled or desired.

The user's key correction was that repeated STK launches cannot uncover the implementation's many independent correctness defects. Work therefore moved to deterministic differential testing, sanitizers, capture/replay, and sequence minimization.

## Current plan

1. ~~Make the shared Gallium corpus valid under release, ASan, and UBSan.~~ **Done.**
2. ~~Add deterministic guest GL stream capture, replay, and minimization.~~ **Done.**
3. Differentially execute state, synchronization, and lifetime sequences through native, softpipe, and WebGPU.
4. Add randomized state/lifetime stress and preserve every reduced failure as a regression.
5. Implement complete feature families and expose only proven capabilities.
6. Run OpenGL CTS/dEQP-style compliance gates.
7. Replay STK traces only after those gates are stable.
8. Optimize browser presentation toward stable 60 FPS afterward.

## Gate status

Every gate below is green as of this handoff.

```sh
GEX_MESA_SANITIZE_OUT=/tmp/gex-mesa-wasm-sanitize-v2 tests/mesa-gallium-sanitize.sh
# PASS gex-gallium-softpipe ...
# PASS gex-wgpu-gallium ...
# PASS gex-wgpu-browser rc=0
# PASS gex-wgpu-gallium asan-ubsan softpipe-browser

tests/mesa-gallium-differential.sh
# PASS gex-gallium-softpipe ...
# PASS gex-wgpu-gallium ...
# PASS gex-wgpu-browser rc=0

tests/mesa-replay-smoke.sh
# PASS gex-mesa-replay version=OpenGL ES 3.1 ... pixel=64,128,191,255

tests/gl-capture-replay.sh
# ok   gl-capture-replay

tests/gl-capture-browser.sh
# ok   gl-capture-browser

tests/mesa-wgpu-replay.sh tests/gl-traces/gles-gears-frames.gexgltr es 2 0
# ok   mesa-wgpu-replay

tests/mesa-wgpu-live.sh
# ok   mesa-wgpu-live

tests/mesa-wgpu-version.sh core
#      4 requirement(s) between GL 3.2 and 3.3
#      context is 3.2 (Core Profile) Mesa 26.3.0-devel / 1.50
```

`git diff --check` is clean in both the main repository and the Mesa submodule.

## What the sanitizer gate found and how it was closed

The previous handoff was stuck on one `softpipe_delete_fs_state` assertion, with a note to audit all 168 `delete_*_state` call sites by hand. That audit was automated instead, which closed the whole class at once.

- Softpipe now asserts in debug builds that a state object is not bound when
  deleted: blend, depth/stencil, rasterizer, vertex elements, sampler (across
  every stage and slot), vertex shader and geometry shader. Only fragment and
  compute shaders asserted upstream.
- The corpus routes every `bind_*`/`delete_*`/`ctx->destroy` through a
  bound-state tracker at the top of `gex_wgpu_smoke.c`. A delete of a bound
  object unbinds it first, which is what `st/mesa` does. Sampler unbinds
  rebuild the whole range from slot zero, because draw's polygon-stipple
  wrapper asserts `start == 0`.

Subsequent findings, each a real defect:

- `make_shadow_fs` built a shadow `nir_tex_instr` with a one-component
  destination but left `is_new_style_shadow` false, whose result size is four.
  NIR validation rejected it. Set `is_new_style_shadow`; both TGSI's shadow
  `TEX` and SPIR-V's `OpImageSampleDref` produce the scalar the shader wants.
- `nir_to_tgsi` declared the combined TGSI texture/sampler from the uniform's
  type. With separate texture and bare-sampler uniforms, the shadow bit is on
  neither, so `tgsi_scan` saw the declaration and the access disagree.
  `ntt_gather_shadow_indices` now collects the bit from the texture
  instructions themselves.
- `sp_quad_depth_test.c` shifted a `uint8_t` stencil value left by 24 in `int`.
- `gex_wgpu` left `ntv_info.bindless_set_idx` at zero, which is the descriptor
  set its own samplers use, so `nir_to_spirv` treated every sampler as bindless
  and indexed a two-entry table by driver location -- an out-of-bounds write as
  soon as a shader declared a third sampler. Now `UINT_MAX`, and the write is
  bounds-asserted in `nir_to_spirv.c`.
- The browser gate treated the debug build's per-shader stderr trace as
  failures; it now ignores only that trace, and its 60s deadline is
  configurable (`GEX_WGPU_TIMEOUT_MS`) because the instrumented build is far
  slower.

## Capture, replay and reduction

New, and the reason plan item 2 is closed.

- `crates/gex-gl/src/trace.rs` -- the `.gexgltr` format: a small header, then
  every batch exactly as the guest wrote it. Round-trip and truncation tests.
- `shims/native/src/gl.rs` -- `GEX_GL_CAPTURE=<path>` records each batch
  *before* execution, so a batch that kills the host is in the file.
- `tests/gl-trace.mjs` -- the reader the tools share, plus command splitting.
- `tests/gl-replay.mjs` -- replays a trace into an Emscripten backend
  (`gex-mesa-replay`). Injects `GET_ERROR` after each batch so a GL error is
  attributed to the batch that caused it; `--readback` prints an FNV digest of
  the surface for cross-backend comparison. Exit status makes it an oracle.
- `tests/gl-reduce.mjs` -- ddmin over the flat command list. `--oracle 'CMD {}'`
  and `--expect REGEX`; `CONTEXT_CREATE` and `SURFACE_RESIZE` are pinned.
- `tests/gl-capture-replay.sh` -- the gate: record 30 frames of `gles-gears`
  through `gexc run --gl`, assert every batch decodes, then reduce against an
  oracle whose answer is known (exactly one `COMPILE_SHADER` survives).

Measured on the first real trace: 936 commands over 40 batches reduced to
6 commands over 3 batches in 21 seconds, 36 oracle runs.

## What the first traces found, and the fixes

The first two recordings each found a defect that made the Mesa replay backend
unable to run any real guest stream. Both are fixed, and both reductions are
kept as regressions in `tests/gl-traces/`.

1. **The Mesa replay backend had no default framebuffer.**
   `initialize_backend` called `st_api_make_current(ctx, nullptr, nullptr)`
   with `visual.color_format = PIPE_FORMAT_NONE` and ignored the width and
   height it was given, so framebuffer 0 did not exist: everything a guest drew
   to it was `GL_INVALID_FRAMEBUFFER_OPERATION` (0x506). It now owns a
   `pipe_frontend_drawable` whose `validate` allocates the colour and
   depth/stencil attachments at the requested size. No
   `PIPE_BIND_DISPLAY_TARGET` -- nothing is scanned out, and the null software
   winsys has no display target to give. Verified rather than assumed: the
   clear-probe trace now replays to a surface digest identical to a uniform
   64,128,191,255, which is what `glClearBufferfv(GL_COLOR, 0, {.25,.5,.75,1})`
   should produce.
2. **Guest shader source is ES, the replay context was core.** `st/mesa`
   reported `syntax error, unexpected NEW_IDENTIFIER` at 1:1 --
   `precision mediump float;` is not a GLSL 1.10 statement. The wire cannot
   say which API the guest bound (`eglBindAPI` is guest-local, and the guest
   sends `CONTEXT_CREATE` from `eglInitialize`, before the app calls it), so
   the backend is told instead: `gex_gl_set_api`, exposed to replay as
   `--api es|core`, defaulting to GLES because that is the transport's own
   default. This needed `-Dgles2=enabled` in `web/build-mesa-wasm.sh`;
   `HAVE_OPENGL_ES_2` gates ES context creation, and with GLX and EGL disabled
   the option builds no extra libraries. The 30-frame gears trace now replays
   all 40 batches with no GL error.

Note that `crates/gex-gl/src/host.rs` (the native Rust host behind
`gexc run --gl`) is an older, smaller backend than the wire decoder in
`web/mobilegl/gexgl-mobilegl.cpp`; it has no `CLEAR_BUFFER_FV`, for one. That
does not affect recordings, which are taken before execution.

## Files added

- `web/mesa-wasm-sanitize-cross.ini`
- `tests/mesa-gallium-sanitize.sh`
- `crates/gex-gl/src/trace.rs`
- `tests/gl-trace.mjs`, `tests/gl-replay.mjs`, `tests/gl-reduce.mjs`
- `tests/gl-capture-replay.sh`, `tests/gl-capture-browser.sh`
- `tests/gl-traces/` (reduced regressions, with a README explaining each)
- `web/src/gl-capture.js`
- `tests/gl-make-context-trace.mjs` -- writes a one-batch trace that asks for a
  given client API and version and does nothing else
- `tests/mesa-wgpu-version.sh` -- the GL version ceiling on real WebGPU, module
  and trace published together so the answer cannot be stale

The sanitizer build is separate, uses ASan+UBSan, frame pointers, `debugoptimized`, no LTO, a 16 MiB stack, and larger initial memory.

## Files materially changed

Top-level:

- `web/build-mesa-wasm.sh`
- `docs/gl-webgpu-architecture.md`
- `crates/gex-gl/src/lib.rs`, `shims/native/src/gl.rs`
- `tests/mesa-wgpu-browser.mjs`, `tests/mesa-gallium-sanitize.sh`
- `tests/mesa-replay-smoke.mjs` (accepts the GLES identity)
- `web/mobilegl/gexgl-mobilegl.cpp` (default framebuffer, `gex_gl_set_api`)
- `web/src/gl.js`, `web/src/mobilegl.js`, `web/test/drive.mjs`
  (browser capture, `?glcapture=1` and `--gl-capture FILE`)

Mesa submodule:

- `meson.options`
- `src/gallium/targets/gex-wasm/meson.build`
- `src/gallium/targets/gex-wasm/gex_wgpu_smoke.c`
- `src/gallium/drivers/gex_wgpu/gex_wgpu_screen.c`
- `src/gallium/drivers/softpipe/sp_screen.c`
- `src/gallium/drivers/softpipe/sp_quad_depth_test.c`
- `src/gallium/drivers/softpipe/sp_state_{blend,rasterizer,sampler,shader,vertex}.c`
- `src/gallium/drivers/zink/nir_to_spirv/nir_to_spirv.c`
- `src/compiler/nir/nir_lower_uniforms_to_ubo.c`
- `src/gallium/auxiliary/nir/nir_to_tgsi.c`

## Browser capture

`web/src/gl-capture.js` records the same format from the page, hooked into both
`GlDevice.submit` (WebGL 2) and `MobileGlDevice.submit`. Enabled with
`?glcapture=1` (or `GEX_GLCAPTURE` under Node), with a 64 MiB ceiling that
truncates at a record boundary rather than growing without bound. The recording
is published on `globalThis.__gexGlCapture`; `web/test/drive.mjs --gl-capture
FILE` collects it over CDP.

Exercised end to end by `tests/gl-capture-browser.sh`: the gears demo recorded
210 batches through the page's WebGL 2 device, and Mesa softpipe in Node then
executed every one of them with no GL error. That is the loop the whole plan
rests on -- what the browser emits is reproducible outside it.

Note that `web/dist` is what the page actually loads, so a change under
`web/src` needs `bash shims/browser/build.sh` (or `drive.mjs --build`) before
it has any effect. A stale bundle looks exactly like a feature that silently
did nothing.

## Mesa + gex_wgpu in the browser (current work)

The user chose this path over MobileGL, on the grounds that st/mesa is the
expensive half and inheriting it is less code to own. Note that the user also
said they had never been told this path existed; the earlier claim in this
document that it followed from a user correction could not be corroborated and
should not be repeated.

**State: it renders.** `gex-wgpu-replay` -- gex's wire decoder -> st/mesa ->
NIR -> gex_wgpu -> emdawn -> browser WebGPU -- replays a recorded 30-frame
`gles-gears` guest stream, 210/210 batches, with no GL error and no WebGPU
validation error, and its output agrees with softpipe:

```
softpipe mean 36.1,27.9,36.0,255.0
webgpu   mean 37.6,28.6,35.2,255.0
worst channel difference 1.50 (tolerance 4)
```

Before this, st/mesa had never driven gex_wgpu at all: the corpus in
`gex_wgpu_smoke.c` calls pipe_context directly, so the driver only implemented
the hooks that corpus happened to call, and the two halves of the claimed
production path had never been connected.

Gate: `tests/mesa-wgpu-replay.sh [TRACE] [es|core] [major] [minor]`. It replays
the same trace through softpipe first and compares per-channel means. Digests
are printed but are **not** the comparison: an exact hash cannot compare a CPU
rasterizer with a GPU, because interpolation precision and rasterization
tie-breaks differ legitimately. A real defect -- a dropped draw, a wrong blend,
an unbound texture -- moves the mean far more than the tolerance.

What had to be fixed, in the order the failures surfaced:

1. `gex_wgpu_public.h` had no `extern "C"` guard (C driver, C++ decoder).
2. `web/gex-wgpu-jspi.js` hardcoded `gex_wgpu_smoke` as the promising-wrappable
   export; it now publishes whichever entry the linked module has.
3. Fifteen missing pipe hooks: `buffer_map`, `buffer_unmap`,
   `transfer_flush_region`, `clear`, `set_sample_mask`, `set_polygon_stipple`,
   `set_min_samples`, `set_clip_state`, `set_active_query_state`,
   `set_debug_callback`, `memory_barrier`, `texture_barrier`,
   `render_condition`, `fence_reference`, `fence_finish`.
4. The screen advertised **zero** caps, so `st_create_context` could never
   succeed. There is now a `pipe_caps` table and per-stage `pipe_shader_caps`.
5. NIR compiler options were one all-zero struct shared by every stage, which
   asks NIR to lower nothing. The corpus survived that by hand-writing NIR that
   needed no lowering; GLSL-derived shaders do not.
6. `pipe_context::stream_uploader`/`const_uploader` were never created. u_vbuf
   reaches for `stream_uploader` without checking it, so this faulted inside
   `u_upload_alloc_buffer` -- nowhere near the driver.
7. `caps.max_vertex_buffers` was 0, which u_vbuf turns into its allowed-buffer
   mask. Every vertex element was therefore "incompatible", no driver vertex
   layout was ever created, and draws arrived with none bound.
8. `create_vertex_elements_state` returned NULL for zero elements. An empty
   layout is legal, not a failure.
9. `draw_vbo` required the bound vertex-buffer count to equal what the element
   state references. Gallium only requires that referenced slots are bound.
10. No `nir_lower_fragcolor`. `gl_FragColor` stays `FRAG_RESULT_COLOR`, which
    WebGPU cannot express -- a colour target must have a matching `@location`
    output -- so the pipeline had a target and no fragment output and WebGPU
    rejected it. The corpus only ever used `FRAG_RESULT_DATA0`.

Two diagnostics were added and are worth keeping:

- `audit_driver_hooks` in `initialize_backend` lists unimplemented
  pipe_context hooks by name at startup, instead of leaving "RuntimeError:
  null function" with a bare wasm index.
- `gex_fail()` replaces `ctx->failed = true` at all 53 refusal sites and
  reports the *first* one with function and line. The driver latches failure,
  so without this an unsupported operation surfaces as GL_OUT_OF_MEMORY from an
  unrelated allocation much later.

### Live, not just replay

`web/src/mesa-gl.js` (`MesaGlDevice`) plugs the same module into the page's GL
device, so an unmodified guest binary drives it. Selected with `?gl=mesa`
(plus `glapi`, `glmajor`, `glminor`). `tests/mesa-wgpu-live.sh` gates it:

```
renderer gex WebGPU (bring-up)
triangles 640   frames 200   gl error 0   covered 20%
ok   mesa-wgpu-live
```

The guest's own numbers are the verdict -- a backend that comes up but renders
nothing still prints `covered 0%`, and a runtime refusal means a draw was
silently dropped, which the guest cannot see.

Note `loadGlue` imports the Emscripten glue directly rather than through a
blob: Emscripten resolves its `.wasm` against the module's own URL, and a
`blob:` URL is not a valid base -- that fails as "Invalid URL" before
`locateFile` is consulted.

### Climbing toward OpenGL ES 3.0

`report_version_gap()` in `initialize_backend` prints the context's actual
version and lists which of the 31 requirements `compute_version_es2()` tests
are unmet. That list is generated, not guessed, and is the work queue.

**16 unmet -> 9.** Closed by making the driver report what WebGPU can actually
do rather than the narrow set the corpus happened to exercise:

- `gex_wgpu_color_renderable` reported only RGBA8 and RGBA16F. It now returns
  WebGPU's core renderable set, which the format table already mapped. This
  alone gave `ARB_texture_rg` and `EXT_sRGB`.
- Added the 32-bit float, `rgb10a2unorm`, `rg11b10ufloat`, `rgb9e5ufloat` and
  snorm formats, all core WebGPU.
- `is_format_supported` derived sampleability from a hand-copied list that had
  drifted from the format table; it now asks the table.
- `texture_half_float_linear` is true (rgba16float is filterable in core
  WebGPU); `texture_float_linear` stays false, because filtering the 32-bit
  float formats needs the `float32-filterable` feature this driver does not
  request.

The corpus's "capability boundary" test pinned the old, narrower set as
*unsupported*. It now asserts the widened set positively, and keeps the
negatives that remain true: multisample render targets, and block-compressed
formats as render targets.

A second pass took it to **5**:

- `ARB_uniform_buffer_object`: st/mesa needs `MaxUniformBlocks >= 12`, i.e. 13
  constant buffers per stage, but WebGPU only guarantees 12 and the driver's
  array was 8. The page now asks `requestDevice` for
  `maxUniformBuffersPerShaderStage: 16`, clamped to what the adapter offers,
  and the driver's limit rose to match. A hardcoded `slot >= 8` sat next to
  the define and had to move with it.
- `ARB_depth_buffer_float` and `EXT_packed_float`: both need optional WebGPU
  features (`depth32float-stencil8`, `rg11b10ufloat-renderable`). The page
  requests whatever the adapter has; the driver asks the *device* what it
  actually got, the way it already did for BC compression, rather than
  assuming.
- `ARB_draw_instanced`: the driver already implemented instancing -- stepMode,
  `instance_count`, `start_instance` -- and simply never advertised
  `vs_instanceid`. Proven before enabling, by a new corpus case
  (`vertex-gl-instance-id`) that draws two instances of a half-surface
  triangle whose colour is chosen by `gl_InstanceID`; it passes on softpipe
  and on WebGPU. Note the coupling recorded in the caps: SPIR-V's
  `InstanceIndex` counts from the draw's base instance while GL's
  `gl_InstanceID` counts from zero, so `start_instance` must stay false until
  the base is subtracted.

Still unmet (5), each for a substantive reason rather than a missing cap:

| requirement | why |
| --- | --- |
| `EXT_transform_feedback` | WebGPU has none at all; needs compute emulation, as Asahi does for geometry stages |
| `ARB_instanced_arrays` | needs `vertex_element_instance_divisor`, but WebGPU's instance step mode is divisor-1 only. `u_vbuf` preserves divisors rather than flattening them, so advertising this would over-promise for divisor > 1 |
| `NV_primitive_restart` | WebGPU restarts only strip topologies at a fixed index; GL allows an arbitrary one |
| `ARB_draw_indirect` | WebGPU has `drawIndirect`/`drawIndexedIndirect`; `draw_vbo` currently refuses `indirect`. Reachable driver work |
| `ARB_framebuffer_no_attachments` | needs the driver to render with zero attachments; `draw_vbo` requires a colour target |

Open:

1. **`ARB_uniform_buffer_object` needs a raised WebGPU device limit.** st/mesa
   requires `MaxUniformBlocks >= 12`, i.e. `max_const_buffers >= 13`, but
   WebGPU's *guaranteed* `maxUniformBuffersPerShaderStage` is 12 and this
   driver's array is 8. The honest fix is to request a higher limit at
   `requestDevice` (in `mesa-gl.js`, `mesa-wgpu-replay.html` and
   `mesa-wgpu-smoke.html`) and raise both together -- not to advertise 13 and
   fail at pipeline creation.
2. **`EXT_transform_feedback` is the hard one.** WebGPU has no transform
   feedback at all; it needs compute-shader emulation, as Asahi does for
   geometry stages. Everything else on the list is reachable with driver work:
   WebGPU has instancing and indirect draw natively.
3. **Eight pipe hooks remain**: `resource_copy_region`, `blit`,
   `flush_resource`, and the five query entry points.
4. The caps table is still a first pass. Everything unproven is deliberately
   zero; raise a cap only when the family behind it passes. `glsl_feature_level`
   stays at 130 for the same reason.

## Version reporting is now derived, not asserted

The user asked why the backend was being run as ES when the app is told it is
desktop GL. It was worse than a mismatch: **three** layers each stated a
version, none of which reflected the backend.

| layer | claimed |
| --- | --- |
| guest `gexgl.c` | `4.4 (Core Profile)` or `OpenGL ES 3.0`, chosen from `eglBindAPI`, discarding the host's reply |
| host decoder `GET_STRING` | a fixed `3.3 (Core Profile) Mesa 26.0.0` |
| actual Mesa backend | whatever `st/mesa` had granted |

All three are gone. The decoder returns the real `glGetString`, the guest
caches the host's answer instead of substituting, `eglBindAPI` only records the
choice, and `GL_MAJOR/MINOR_VERSION` are parsed from that same string so the
integer and string forms cannot disagree.

### The client API now reaches the host

`CONTEXT_CREATE` carried no payload and was sent from `eglInitialize` -- before
`eglBindAPI`, so the host could not know whether the application wanted desktop
GL or ES and had to be told out of band by a test flag.

Protocol **5 -> 6**: `CONTEXT_CREATE` is `{ client_api, major, minor }`, and the
guest defers it to `eglCreateContext`, which is where a real EGL implementation
creates a context and where the API is finally known. `eglMakeCurrent` is a
safety net for callers that reach a context another way; the latch makes it
idempotent. All three hosts consume the payload (the WebGL 2 and native GLES
devices read and ignore it -- an unread payload is rejected as malformed, which
is what keeps version skew from looking like a valid stream).

### Finding the ceiling the way a driver does

The first attempt walked a descending ladder of versions, which meant creating
and destroying up to thirteen contexts and crashing in teardown paths that had
never run. `st_api_create_context` computes the version it can support and then
only *compares* it against the request -- and a request of 1.0 skips that
comparison. So: ask for 1.0, read back `ctx->Version`, and enforce the
application's request. One context, and the number is the real ceiling rather
than the best entry of a list somebody has to maintain. A core profile below
3.2 falls back to compatibility, which is the profile that exists there.

### Measured ceilings

```
softpipe   ES 3.1    GL 3.3 (Core Profile)
gex_wgpu   ES 2.0    GL 3.2 (Core Profile) / GLSL 1.50
```

Desktop went **1.3 -> 2.1 -> 3.0 -> 3.1 -> 3.2** across this work. `report_version_gap()`
reports the desktop ladder -- the rungs are exactly what `compute_version()`
tests, now up to GL 3.3 -- and names only the next one.

Two of its rungs are *constants*, not extensions, and both were invisible
until the reporter learned to check them: GL 3.0 wants four colour attachments
and 4x multisampling, and GL 3.1 wants sixteen vertex texture image units. A
reporter that only reads the extension list cheerfully prints "0 requirements"
while the version stays put. It also refuses to run on an ES context now:
Mesa numbers ES on its own scale, so an ES 2.0 context has `Version == 20` and
the desktop rungs read it as GL 2.0.

### Measuring it

`tests/mesa-wgpu-version.sh [es|core]` is the whole measurement:

```
$ tests/mesa-wgpu-version.sh core
     at or above the highest rung listed
     context is 3.1 Mesa 26.3.0-devel / 1.40
```

It exists because the measurement has two traps that both produce a
confident-looking wrong number.

**The context comes from the wire, not from a flag.** The backend creates its
context in the `CONTEXT_CREATE` handler, because only the guest knows which
API it bound; `gex_gl_set_api` supplies only a default that a real
`CONTEXT_CREATE` overrides. So measuring a desktop ceiling needs a stream that
*asks* for a desktop context, which `tests/gl-make-context-trace.mjs` writes:
one batch, one `CONTEXT_CREATE`, nothing else.

**It asks for version 1.0 deliberately.** `st_api_create_context` computes the
highest version it can supply and then only *compares* it against the request,
so asking for the lowest reports the ceiling instead of testing a guess.

The script also copies the freshly built `.wasm` alongside the trace. Copying
only the trace measures whatever module was published last, which is a stale
answer indistinguishable from a real one -- it cost a full round of wrong
conclusions before the script existed.

### Reaching GL 3.1

Four things stood between 3.0 and 3.1, and each was its own problem.

**`ARB_uniform_buffer_object` is not a cap to raise.** st/mesa sets
`MaxUniformBlocks = max_const_buffers - 1`, reserving slot zero for the
default uniform block, so the extension needs `max_const_buffers >= 13`.
WebGPU's guaranteed floor is 12 uniform buffers per stage, so *any*
implementation at the baseline blocks GL 3.1 this way. The driver now backs
constant buffer slots past the device's uniform limit with **storage buffers**
instead: `gex_wgpu_lower_gallium_ubos` flips those variables to
`nir_var_mem_ssbo` and rebinds them above the uniform range. The block is an
array of `vec4`, for which std140 and std430 agree, so only the address space
and the binding change. The corpus proves it with
`constant-buffer-high-slot`, which reads the highest slot the driver claims.

**A limit sentinel read as a value, and overflowed.**
`wgpuDeviceGetLimits` reported `maxStorageBuffersInVertexStage` as
`WGPU_LIMIT_U32_UNDEFINED` -- this Dawn does not fill the newer split
per-stage storage limits. Taken literally that is 4294967295, and
`12 + 4294967295` wrapped to **11**, so a change meant to *raise* a cap
lowered it, and `max_const_buffers` came out one below where it started. The
symptom was an extension that stayed off with no error anywhere. The driver
now falls back to `maxStorageBuffersPerShaderStage`, which is always present.

**A device only gets the limits it asks for.** The pages requested a raised
uniform-buffer limit only when the adapter offered 16 or more, and requested
nothing else at all -- so the device came up with WebGPU's *defaults*, which
are far below what the adapter can do. The driver sizes its caps from the
device's limits, so every limit not requested was capability the backend never
saw. Both `web/src/mesa-gl.js` and `web/public/mesa-wgpu-replay.html` now ask
for everything the driver is built for, clamped to the adapter, and the
`LIMITS` line logs the **device's** limits rather than the adapter's.

**`NV_primitive_restart`** is implemented rather than claimed. WebGPU has
restart only for strip topologies and only at a fixed index, so `draw_vbo`
resolves the general GL form on the CPU with
`util_draw_vbo_without_prim_restart`, which scans the index buffer and
re-issues the draw as the ranges between restarts. That helper hands back
several draws at once, so `draw_vbo` also learned to loop over `num_draws`.
The corpus case `primitive-restart` draws a triangle, a restart, and two
indices that cannot finish another, and checks the incomplete primitive is
dropped -- a backend that ignored the restart would assemble a triangle from
the restart index itself and move it.

**Sixteen texture image units.** `GEX_WGPU_MAX_SAMPLERS` went from 8 to 16,
which WebGPU's default `maxSampledTexturesPerShaderStage` and
`maxSamplersPerShaderStage` both allow exactly. The slot masks were `uint8_t`
and are now `uint16_t`; an eight-bit mask would have silently dropped slots 8
and up.

**GLSL 1.40** is the version GL 3.1 compiles at. Its substantive addition over
1.30 is the uniform block, which is what the storage-backing above delivers.

### GL 3.2: geometry shaders as a compute pass

WebGPU has no geometry stage, so a draw with one bound becomes three passes.
This is the shape Asahi uses, and the reason it is reimplemented here rather
than reused: Mesa's `poly_nir_lower_gs` (`src/poly/nir/`) does exactly this
decomposition, but it emits `load_global` / `store_global` / `global_atomic` --
raw 64-bit pointers, which WGSL has no way to express. Same wall as
`nir_lower_xfb_to_stores`. The design carries over; the memory model does not.

The three passes, all in `gex_wgpu_gs_draw`:

1. **The vertex shader, as compute**, writing its varyings to a buffer
   (`gex_wgpu_varying_variant_get`). This is the transform feedback machinery
   with a different store: rather than rewriting each output write, it reads
   the output *variables* once at the end of the shader, which is what makes
   it correct for an output written partially, more than once, or under
   control flow.
2. **The geometry shader, as compute**, one invocation per input primitive
   (`gex_wgpu_gs_build_module`). Per-vertex inputs become reads of the buffer
   from pass one. Each `EmitVertex` writes the output variables to a vertex
   buffer and, once the strip has three vertices, three indices to an index
   buffer.
3. **An ordinary indexed draw** of the result, with a generated passthrough
   vertex shader and vertex element state that read the geometry output buffer
   *as a vertex buffer*.

Three decisions worth keeping:

- **The output is allocated worst-case** -- `max_vertices` per input primitive
  -- rather than counted and prefix-summed. That is two whole passes and an
  indirect draw avoided, at the cost of memory and a zeroed index buffer.
  Unwritten index slots stay zero, which is three equal indices, which is a
  degenerate triangle that rasterizes nothing.
- **The lowering introduces no control flow.** These run as intrinsic passes
  over existing blocks, and `nir_push_if` splits the block being iterated.
  Where a rule wants a branch -- "only close a triangle once the strip is long
  enough" -- it is a select instead: an emit that closes no triangle writes
  three equal indices into the slot the next real triangle overwrites anyway.
- **Pass three reads the buffer as a vertex buffer, not a storage buffer.**
  WGSL only allows read-only storage in the vertex stage, and the SPIR-V
  emitter declares every storage buffer `read_write`, so a vertex shader
  loading it directly is rejected. Fixed-function vertex fetch has no such
  restriction, and the buffer is already vec4 slots at a fixed stride -- which
  is exactly a vertex layout.

Refused at shader creation, with a message, rather than mis-rendered: input
primitives other than triangles, output primitives other than triangle strips,
geometry shader instancing, and streams past zero.

`geometry-passthrough` in the corpus proves it on both backends. It passes the
position through unchanged, so the triangle has to land where the vertex
shader put it, and replaces the colour, so a backend that quietly skipped the
stage fails rather than passes.

### The other three GL 3.2 requirements

- **`ARB_depth_clamp`** is WebGPU's optional `depth-clip-control` feature,
  which becomes `unclippedDepth` on the pipeline. GL clips near and far
  independently; WebGPU has one flag, so `create_rasterizer_state` refuses a
  state that splits them, and `depth_clip_disable_separate` stays off. The
  corpus case reuses the existing clip shader: the *same* triangle behind the
  near plane, drawn with only the rasterizer state changed, must now appear.
- **`ARB_seamless_cube_map`** is a statement about WebGPU rather than about
  this code: cube sampling there is seamless and cannot be turned off, which
  is what every Vulkan-backed driver reports too. `seamless_cube_map_per_texture`
  stays off precisely because it cannot be turned off.
- **`ARB_texture_multisample`** is the ordinary path, not an emulation: WGSL
  has `texture_multisampled_2d` and reaches it with `textureLoad` and a sample
  index, which is what `texelFetch` on a `sampler2DMS` means. Two things had to
  change: the descriptor validation allowed only `txf` to have no sampler, and
  `txf_ms` has none either; and a multisampled slot is `UnfilterableFloat`
  whatever its format would otherwise allow.

### A resolve that destroyed its source

Found by the multisample fetch case reading black. `gex_wgpu_resolve` ended its
render pass with `storeOp = Discard`, on the reasoning that "the multisampled
contents are not needed afterwards; only the resolve is". That is not what GL
says: a resolve blit reads the multisample buffer and leaves it alone, and
resolving for display while continuing to render to the same buffer is the
ordinary way to use one. Every such program was handed an undefined attachment.
It stores now.

### GL 3.3 is blocked outside this codebase

```
gex-wgpu: 4 requirement(s) between GL 3.2 and 3.3
gex-wgpu: GL 3.3 wants ARB_blend_func_extended
gex-wgpu: GL 3.3 wants ARB_instanced_arrays
gex-wgpu: GL 3.3 wants ARB_timer_query
gex-wgpu: GL 3.3 wants GLSL 330, have 150
```

`ARB_texture_rgb10_a2ui` is done -- `rgb10a2uint` is sampleable and renderable
in core WebGPU, so it was a format table entry.

**`ARB_blend_func_extended` needs `dual-source-blending`, which this Chrome
does not have.** The adapter's full feature list, logged by the replay page as
`FEATURES`, is:

```
bgra8unorm-storage, chromium-experimental-timestamp-query-inside-passes,
clip-distances, core-features-and-limits, depth-clip-control,
depth32float-stencil8, float32-blendable, float32-filterable,
indirect-first-instance, rg11b10ufloat-renderable, subgroup-size-control,
subgroups, texture-component-swizzle, texture-compression-astc,
texture-compression-astc-sliced-3d, texture-compression-bc,
texture-compression-bc-sliced-3d, texture-compression-etc2, timestamp-query
```

No `dual-source-blending`, and the experimental features already present say
this is not a flag that was left off. So **GL 3.3 is unreachable on this
browser** until Chrome ships it -- not a gap in this driver. The other two are
ours to do:

- **`ARB_timer_query`** has `timestamp-query` available; it wants a query set
  of that type alongside the occlusion one, resolved and read back the same
  way.
- **`ARB_instanced_arrays`** wants an arbitrary attribute divisor, and WebGPU's
  `stepMode: instance` only advances once per instance. The way through
  without emulation is `arrayStride = 0` with `stepMode: instance`, which makes
  every instance read the same element, plus one draw per divisor group at the
  matching buffer offset and `firstInstance` -- the driver already loops over
  `num_draws`, and `indirect-first-instance` is available. The driver refuses
  a divisor above one today.

### A validation error costs the run; a refusal costs the draw

Enabling `u_blitter` traded a refusal for something worse, and it took a
regression to see it. `varied_rectangle` fell from 962 to **483/1577**, and the
log said why:

```
WebGPU validation: Color format (TextureFormat::R16Uint) base type (Uint)
doesn't match the fragment module output type (Float).
QPA Invalid format used but glTexImage2D/3D succeeded
```

**TGSI carries no type on a shader output**, so `tgsi_to_nir` gives
`u_blitter`'s integer texel-fetch shader a float-declared output. Against an
integer colour target WebGPU rejects the pipeline -- and a validation error
**poisons the device for every later command**, not just the failing one. The
downstream symptom is nonsense: operations that should fail stop reporting
failure, so the CTS's *negative* tests start failing too. That is the whole
1094-failure figure. `nir_lower_fragcolor` was the obvious suspect and is
innocent; it copies `out->type`.

The driver records the fragment shader's colour output base type now and
refuses a draw whose target disagrees. That is a containment, and it is not
free -- it cost `rectangle` and `pbo_rectangle` 69 cases each -- but the trade
is heavily in its favour: **+479 in `varied_rectangle` against -138 elsewhere.**

The cure, not the containment, is a fragment variant whose output type matches
the target, and it is written: `gex_wgpu_retype_color_outputs` redeclares
`FRAG_RESULT_*` with the target's base type, which is a redeclaration rather
than a conversion because a NIR value is an untyped bit pattern. The variant
cache keys on the output type alongside the sampler state it already tracked.

**It is not enough on its own**, which is why the refusal is still the active
path. With the type corrected the same shaders trip

```
WebGPU validation: The interpolation type (Flat) of the vertex output ...
```

-- the blitter's vertex and fragment halves disagree about interpolation, which
is another validation error and so another poisoned device. Until that half is
fixed the refusal is the better of the two, and the code says so at the
refusal.

### The device only gets what it asks for -- a third time

`varied_rectangle` was collapsing on this:

```
WebGPU validation: None of the supported sample types (UnfilterableFloat) of
[Texture (7x3 px, TextureFormat::R32Float)] match the expected [Float]
```

A 32-bit float texture is `UnfilterableFloat` unless `float32-filterable` is
requested, and the driver reports it as `Float`, so every bind group holding
one was rejected. The feature **is** on this adapter. Requesting it (and
`float32-blendable`, available and equally expected by GL) removed every
validation error from that subgroup.

This is the third instance of one mistake: the uniform-buffer limits, then
`depth-clip-control`, now `float32-filterable`. A capability that exists on the
adapter does nothing until the device asks for it.

One trap worth recording alongside: the CTS runs the **bundled** site, so
editing `web/src/mesa-gl.js` changes nothing until `npm run build` runs. The
harness rebuilds on its own, which makes the failure look like a slow start
rather than a stale bundle.

### What the numbers actually say about refusing

Three states, each measured across all three subgroups:

| state | rectangle | pbo | varied | GL 3.2 |
| --- | --- | --- | --- | --- |
| refuse the mismatch | 1106 | 1105 | 962 | 3848 (68.0%) |
| permit it, float32 on | 1200 | 1199 | 483 | 3557 (62.8%) |
| refuse it, float32 on | 1151 | 1150 | 962 | **3938 (69.6%)** |

Permitting is worth ~50 cases each in two subgroups and costs 479 in the
third, because with the draws going through st/mesa starts accepting format
combinations GL requires it to reject -- "Invalid format used but
glTexImage2D/3D succeeded", 39 times. So the refusal stays, and its comment now
says it is a policy rather than a stopgap.

It also means `varied_rectangle`'s 962 is **not** a clean baseline: a dead
context passes every negative test, so part of that figure was always an
artefact. 483 is what that subgroup scores when its cases genuinely run. Any
future comparison there has to account for it.

One process note, because it is the reason this got as far as it did: the
regression was reported as a **win** the turn before it was found. One subgroup
was measured (`pbo_rectangle`, +212), the family was not re-measured, and the
damage was already in the tree. The differential gate cannot catch this class
-- it covers the driver's own corpus, not the CTS -- so a change that helps one
CTS family and breaks another passes it in silence.

### Three defaults that were silently wrong

All three came out of one question -- "how badly would a real GL app break?" --
and all three share a shape: `u_init_pipe_screen_caps` and the `pipe_context`
hook table are defaulted for a driver whose mapped pointer *is* GPU memory and
whose hardware draws every topology. A driver that is neither has to say so,
and this one did not.

**`supported_prim_modes` defaults to every mode.** `draw_vbo` refused anything
that was not `MESA_PRIM_TRIANGLES`, but the cap said otherwise, so st/mesa
handed the driver triangle strips and the context died on the first draw. A
triangle strip is the most common thing a GL program draws. The driver now
reports the five topologies WebGPU has -- points, lines, line strips, triangles
and triangle strips -- and `u_primconvert` rewrites fans, loops, quads and
polygons into them.

**`buffer_map_persistent_coherent` defaults to true, and this was the bad one.**
A mapped buffer here is a CPU shadow that reaches the GPU only when a range is
flushed. `u_upload_mgr` reads that cap, concludes no flush is needed, and never
calls `transfer_flush_region`. Everything uploaded through `u_upload` therefore
reached the GPU as whatever the buffer happened to hold before: `u_vbuf`'s
translated vertex buffers, `u_primconvert`'s generated indices, st/mesa's
uploaded user buffers. No error anywhere -- the draw simply drew nothing, or
drew the previous frame's data. The cap is false now.

**`pipe_context::resource_release` was null.** Paths that allocate a one-off
resource call it; `u_primconvert`'s line-loop index buffer is one. It is a call
through a null function pointer, not a missing feature, and it surfaced as
`RuntimeError: null function` with no symbol. `u_default_resource_release` is
what it wanted.

The lesson generalises past these three: **a Gallium default that is right for
hardware is a silent claim this driver has to actively withdraw.** The two caps
above were both found by a single corpus case that draws every topology, which
is the case that should have existed from the start.

`topology-triangle-strip`, `-triangle-fan`, `-quads`, `-polygon` each cover the
viewport, so they are checked pixel-exact. `topology-points` covers exactly
four pixels. The line cases are checked on coverage rather than on a pixel
pattern, because which pixels a line covers is a rasterization rule this corpus
does not pin down -- but whether anything was drawn at all is exactly what a
rejected pipeline or an unflushed index buffer gets wrong.

### A refusal ends the run, so every gap costs a whole family

This is the dominant limit on conformance numbers, and it is structural rather
than a list of missing features. `gex_fail` latches `ctx->failed` for the
lifetime of the context -- which is what makes "refuse rather than render
wrong" safe -- so the *first* unsupported thing ends every case after it. The
CTS reports that as `Test run was ABORTED`, and the family looks like a crash
rather than like the one failure it is.

`KHR-GL32.texture_repeat_mode` is the worked example. It has 162 cases:

| after fixing | cases that ran | why it stopped |
| --- | --- | --- |
| -- | 10 | `texture_map` refused a write map |
| texture write maps | 64 | `texture_subdata` refused an unsized format |
| format sizing | 162 | nothing -- the context survived the family |

Every one of those was one refusal. Two things made them findable: the
refusals now print *which condition* rejected the call and with what
parameters, rather than only a function and line number. "first refusal at
gex_wgpu_texture_map:3428" says where; it does not say what to implement.

**Texture write maps** are how st/mesa uploads most of `glTexImage2D` when it
does not go through `texture_subdata`, and the driver had none. They are staged
in ordinary memory and handed to the same upload path on unmap, so every level,
layer and block format `texture_subdata` already handled works through a map
too. A read-write map reads back first and uploads on unmap, which is what
asking for both means.

**Format sizing was a hand-written table that had drifted.**
`gex_wgpu_format_bytes` knew 14 formats while `gex_wgpu_texture_format`
accepted 55. The failure mode is not a wrong size but a zero one: an unsized
format is refused, and a refusal ends the context. Forty-one formats were
affected -- every 16- and 32-bit format, every snorm one, rgb10a2 -- so
uploading any of them killed the run. It now asks
`util_format_get_blocksize`, which cannot drift from the set the driver
accepts. The readback path had the same bug in a different shape: it hardcoded
four bytes per pixel, and it also now handles any mip level.

The general lesson matches the caps one above: **a table maintained by hand
next to a table maintained by Mesa will drift, and the drift shows up as a
refusal rather than as a wrong answer.**

### When a refusal should latch, and when it should not

Latching is right for an operation that **cannot tell its caller it failed**.
A draw that is silently dropped makes every later result untrustworthy, so
`gex_fail` poisons the context and the run stops being believed.

A map is not that operation. `texture_map` reports failure by returning NULL,
and st/mesa turns that into a GL error rather than into wrong pixels -- so
latching there converted one unsupported readback into thousands of dead cases
and bought no correctness at all. Map refusals now decline: they name the
reason once and return NULL, and the context lives.

`texture_subdata` returns `void`, so it still latches. That is the rule, not an
inconsistency.

### A validation error is worse than a refusal

`packed_pixels` -- 4732 cases, 84% of the GL 3.2 mustpass -- ran **zero** of
them, on this:

```
WebGPU validation: More than a single aspect (TextureAspect::All) is selected
for multi-planar format (TextureFormat::Depth24PlusStencil8)
```

WebGPU copies one aspect of a depth-stencil texture at a time, and will not
copy the depth aspect of depth24plus at all, so a packed depth read has no
legal encoding. The driver was encoding it anyway. That is not merely a failed
copy: **a validation error poisons the device for every later command**, so one
illegal encode ended the family. Both the readback and the upload path now
refuse depth-stencil rather than attempt it -- the map declines, the subdata
latches, and neither reaches the device.

With that and the map change, `KHR-GL32.packed_pixels.rectangle` went from
**0 cases run** to **963 of 1578 passing (61%)**.

`tests/gl44-cts-browser.sh` takes `GEX_CTS_WALL` and `GEX_CTS_IDLE` now; the
hardcoded 300-second wall could not have finished a family this size.

**The trade this makes, stated plainly.** Declining a depth-stencil copy is not
free: `texture_repeat_mode` went from 162 cases run to 64, because st/mesa
turns a failed map into `GL_OUT_OF_MEMORY` and deqp treats that as fatal. The
net is strongly positive -- `packed_pixels` went from zero cases to several
thousand -- but it is a trade, not a pure win.

The real fix is a depth upload and readback through a render pass, sampling or
writing `gl_FragDepth`, which is what drivers on APIs without depth copies do.
Two shortcuts were considered and rejected:

- *Promote `Z24_UNORM_S8_UINT` to `Depth32FloatStencil8`*, whose depth aspect
  WebGPU can copy. This makes Gallium and WebGPU disagree about the layout of
  the texture: st/mesa still believes it holds packed 24+8, so every CPU-side
  read or write of it would be misinterpreted. Silently wrong beats loudly
  broken only until someone trusts the pixels.
- *Encode the copy anyway.* That is what the driver did before, and it is why
  `packed_pixels` ran nothing: the validation error poisons the device.

Where a depth aspect **is** copyable -- `depth32float` and
`depth32float-stencil8` -- the upload path now selects `DepthOnly` and works.
Only `depth24plus-stencil8` is declined.

### The GL 3.2 conformance number

**3940 / 5661 of the Khronos GL 3.2 mustpass, 69.6%**, measured on Mesa +
`gex_wgpu` on real WebGPU. At the start of this work the same measurement was
an effective zero -- not because the driver failed the cases but because it
never created a context.

```
packed_pixels     3263/4732   rectangle 1151/1578, pbo_rectangle 1150/1577,
                              varied_rectangle 962/1577
shaders30          649/651    (eleven chunks; every chunk that ran was 100%)
other families      28/161
```

One caution about that number: it counts only cases that *ran*. Several small
families still stop early, and two `shaders30.declarations` cases cannot run at
all because the CTS wants a data file from its archive
(`./gl_cts/data/gl30/declarations...`) that the harness never stages into the
guest -- `glfile` puts the binary there and nothing else.

`shaders30` is the clearest evidence that a family failing wholesale usually
is not the driver: chunked, **every one of its nine `glsl_constructors` groups
passed 72/72**, and the family had previously reported zero.

### clip_distance, and a WebGPU feature worth knowing about

Both `clip_distance` failures are the same shader rejection:

```
Tint rejected SPIR-V: [VUID-ClipDistance-ClipDistance-04191] According to the
Vulkan spec BuiltIn ClipDistance variable needs to be a 32-bit float array
```

So the SPIR-V this driver emits declares `ClipDistance` with a type Vulkan
does not allow -- most likely because `nir->info.clip_distance_array_size` is
not what the emitter expects when a shader writes `gl_ClipDistance` directly.
`caps.clip_planes` is unset, which tells st/mesa to lower *fixed-function*
user clip planes, but a shader writing `gl_ClipDistance` itself still reaches
`nir_to_spirv` as the builtin.

Worth recording separately: **this adapter exposes `clip-distances`**, along
with `depth-clip-control`, `float32-blendable`, `float32-filterable`,
`texture-component-swizzle`, `subgroups`, and the ASTC/ETC2/BC compression
sets. The full list is in the replay page's `FEATURES` console line. Nothing
requests `clip-distances` today, and it would be needed for any real clip
distance support -- but requesting it does **not** fix the two failures above,
because Tint rejects the module on a type error before device features are
consulted. Fix the emission first.

### Vertex shaders without gl_Position

WGSL requires a vertex entry point to return `@builtin(position)` and Tint
enforces it: *"a vertex shader must include the 'position' builtin in its
return type"*. GL does not require it -- a transform-feedback program writes
only its capture varyings and never `gl_Position`, because it runs with the
rasterizer discarded. Ten `transform_feedback` cases failed to link on this.

The vertex path now synthesizes a constant position output when the shader
declares none. That is correct rather than merely sufficient: nothing
downstream rasterizes a shader that never wrote a position, so the value is
unobservable.

It did not change the case count, because the same cases hit the next limit
immediately: **the geometry-shader path implements only triangles in and
triangle-strip out**, and these tests use `layout(points) in; layout(points,
max_vertices = 1) out;` -- the simplest geometry shader there is.

That is implemented now. Two fields carry it -- `max_output_prims` (a strip of
N vertices is N-2 triangles, a point stream is N points) and
`indices_per_prim` (3 or 1) -- so the emitter and the draw agree without
either naming a topology:

1. `gex_wgpu_create_gs_state` accepts points in/out, and no longer demands
   three output vertices when one is a whole primitive.
2. `gex_wgpu_gs_emit` has a points branch: one index at the running primitive
   counter, no strip to close and no winding to alternate.
3. `gex_wgpu_gs_draw` derives its mode check, primitive count and index count
   from those fields.
4. The rasterizing draw sets its topology from the shader's *output*
   primitive rather than inheriting the application's input mode.

`transform_feedback` went 11 failures to 10, and the shader rejections went
away entirely: 10 "Program linkage has failed" to 7, with "Geometry Feedback"
and "Get Transform Feedback Varying" now passing. The triangle path is
unchanged -- differential digest identical, sanitizer green, and the
`gex-passthrough-gs` corpus case still runs.

Two things did **not** help, recorded so they are not retried:

* Setting `max_stream_output_{interleaved,separate}_components`. The premise
  was that they defaulted to zero and blocked linking; "Limits are in range of
  specification" was already passing, so they did not. The values are kept
  because they state what the layout actually holds (16 vec4 slots is 64
  components), but they fixed nothing.
* The remaining 7 linkage failures log `Program linkage has failed due to:`
  with nothing after the colon -- the GLSL info log is not in the QPA output,
  so they are not diagnosable from the harness as it stands.

Still refused in this family: indexed and instanced capture
(`gex_wgpu_capture_dispatch`), which needs the invocation index to reach the
index buffer and the instance.

### The one defect that is provably ours, and five hypotheses that were not

Running the same subgroup on MobileGL -- same guest, same transport, a
different GL implementation -- separates shared failures from driver ones. The
split is sharp:

* **Shared** (both backends): accepting invalid formats. 16091 vs 7303 on
  glTexImage2D, 370 vs 12995 on glReadPixels. Not this driver.
* **The Mesa path only**: `FBO is not complete but expected complete with
  sizedFormat: GL_SRGB8_ALPHA8`, 1909 times, and **zero** on MobileGL. Read
  that precisely: MobileGL does not use Mesa at all, so this locates the
  failure in st/mesa-plus-this-driver, *not* necessarily in driver code. The
  frontend as configured by these caps is as much a suspect as the hooks.
* **Better here than there**: MobileGL logs 2334 "Valid format used but
  glTexImage2D/3D failed" and 1399 for glReadPixels. This driver logs neither.

The sRGB framebuffer failure is genuinely ours and still unexplained --
but there is an instrument for it now. `is_format_supported` logs every
render-target query it refuses, once per format:

```
gex-wgpu: render target refused a8b8g8r8_srgb target=2 samples=0/0 bind=0xa (sampled=0)
```

Framebuffer completeness is decided entirely by st/mesa calling that hook, and
a "no" used to leave no trace anywhere: the application saw
`GL_FRAMEBUFFER_UNSUPPORTED` and the driver said nothing. Note the dedupe is
per format, not global -- the *first* refusal in any run is st/mesa probing a
list where any one format will do, and reading that as the cause is a trap.

What it shows for the sRGB cases: the refused formats are the 16-bit
unorm/snorm set (genuinely absent from WebGPU) and three alpha-first channel
orderings -- `a8b8g8r8_srgb`, `a8b8g8r8_unorm`, `a8r8g8b8_unorm`. WebGPU has
no alpha-first format, so those cannot be mapped without a wrong swizzle.

**`r8g8b8a8_srgb` is never refused** for a 2D target. That is the format
st/mesa picks for `GL_SRGB8_ALPHA8`, and this driver accepts it as a render
target, so the incompleteness is not the colour format and the alpha-first
refusals are format *probing* rather than the attachment.

The depth attachment is ruled out too: the test builds the framebuffer with a
single `framebufferTexture2D` colour attachment and no depth at all
(`glcPackedPixelsTests.cpp`, around the `checkFramebufferStatus` call).

So no `is_format_supported` refusal explains it. Nor is it a missing
resource: `resource_create` now names any texture built with a format it
cannot map to WebGPU, and it names none in these cases.

**The cause is upstream of the framebuffer entirely.** Temporarily forcing
Mesa's own `fbo_incomplete`/`att_incomplete` messages (they are gated on
`MESA_DEBUG`, which cannot be set in the wasm build) gives:

```
gex-att-incomplete:  no teximage
gex-fbo-incomplete:  color attachment incomplete [0]
```

`no teximage` means the attached level has **no texture image at all** --
`_mesa_base_tex_image` returned NULL, so `glTexImage2D` never created one.
The framebuffer is incomplete because there is nothing attached to it, which
is a symptom. And the test's own census reports no "Valid format used but
glTexImage2D/3D failed" for these cases, so the call appeared to *succeed*
from GL's side while leaving no image behind.

Chasing that led to the mechanism. `glFramebufferTexture2D` is handed a guest
texture name the decoder's map does not contain:

```
gexgl: framebuffer texture 241 unknown, attaching zero (detach);
       map has 0 entries spanning 0..0
```

`lookup()` returns zero for an unknown name, and zero means **detach**. So the
attach silently removes the attachment, the framebuffer ends up with nothing,
and st/mesa reports `no teximage`. All 1909 messages come from that.

The provenance of the name is still open, and the trace is precise about it.
Per failing case the guest generates and binds a texture, then attaches a name
two higher:

```
GEN_TEXTURES guest=239   BIND_TEXTURE guest=239 -> host=1   attach 241 unknown
GEN_TEXTURES guest=243   BIND_TEXTURE guest=243 -> host=2   attach 245 unknown
GEN_TEXTURES guest=247   BIND_TEXTURE guest=247 -> host=3   attach 249 unknown
```

`take_name()` is one counter shared by every object kind. Logging which kind
claimed each name settles what the gap is:

```
name 243 -> texture      name 244 -> framebuffer
name 246 -> framebuffer  framebuffer texture 245 unknown
```

**No object kind claims 245.** A name is consumed and no registration command
is ever sent for it, and that unregistered name is what the application then
attaches.

The call site is identified. It is `texture2` in
`RectangleTest::doCopyInner` (`glcPackedPixelsTests.cpp`), which does:

```c
gl.genTextures(1, &texture2);          /* name 245 -- never reaches the decoder */
gl.bindTexture(GL_TEXTURE_2D, texture2); /* also never reaches it */
gl.copyTexImage2D(...);
gl.genFramebuffers(1, &fbo);           /* name 246 -- arrives normally */
gl.framebufferTexture2D(..., texture2, 0);  /* attaches an unknown name */
```

Two consecutive commands vanish while the very next one arrives. Every guest
allocator was audited -- `glGenTextures`, `glGenBuffers`, `gen_named_objects`,
`glGenSamplers`, `glGenVertexArrays`, `glCreateShader`, `glCreateProgram` --
and each takes its names *after* `begin()` has succeeded and writes them into
the same command, so none can leak a name that way. `flush()` submits and only
resets `tail` when the ioctl fails.

### A real transport bug, fixed -- and it was not the cause

Instrumenting both sides at once settles it. The guest generates the name and
the host never sees it:

```
gexgl-guest: glGenTextures -> 241        (guest side)
gexgl: DELETE texture 241 known=0        (host side -- the delete arrives, the
                                          generate never did)
```

A delete for a name whose generate never arrived is proof the command was
written and then thrown away. And `flush()` says exactly where:

```c
if (ioctl(gl_fd, GEX_GL_SUBMIT, ring) != 0) {
    ring->tail = 0;        /* the whole queued batch, gone */
    ring->reply_len = 0;
}
```

Confirmed live:

```
gexgl: submit failed, discarding 36 queued bytes (status 0)
```

**A failed submit drops every command queued behind it and tells nobody.**
The application's `glGenTextures` has already returned a name, so it proceeds
with a handle the host has never heard of. `lookup()` maps that to zero,
`glFramebufferTexture2D` treats zero as *detach*, and the framebuffer ends up
empty -- surfacing as 1909 `FBO is not complete ... GL_SRGB8_ALPHA8` messages
in a completely different subsystem, hundreds of commands later.

That is why five framebuffer-shaped fixes measured flat: the framebuffer was
never the defect.

The submit was failing because the wasm module was being **re-entered**:

```
[gl] the async device threw RuntimeError: Aborted(Assertion failed:
     ... "pthread mutex deadlock detected" ... __pthread_mutex_timedlock)
```

Two things drive `serve()` in `gl-bridge.mjs`: its own `waitAsync` loop and
the function handed back to the runtime, called on every wake-up. A JSPI
device suspends *inside* submit, so the slot still reads `REQUEST` while the
promise is pending -- and the other driver, not seeing that, called submit
again on the same thread. musl's own check catches it, the throw unwinds to
the guest as a failed ioctl, and the batch is discarded.

`serveGl` now serialises submits behind an `inFlight` promise. Verified: the
discard line no longer appears at all, where it fired reliably before.

**And it did not fix the framebuffer failures.** `packed_pixels.rectangle`
stays at 349 with the same 1909 messages, and unknown-texture attaches still
happen. So the discard was real, is fixed, and was not the cause -- the
generate commands are going missing some other way. Calling it the root cause
was premature: the evidence showed a mechanism that *could* explain the
symptom and a fix that *should* have moved it, which is not the same as
having measured that it does.

### When the throws and the FBO failures appeared

Comparing every run in this session's logs pins it exactly:

| run | device throws | FBO messages | failures |
| --- | --- | --- | --- |
| before the driver fixes | 0 | none | 427 |
| `recheck` and everything after | 16426 | 1909 | 349 |

They arrive together, in the first run after the batch of seven driver fixes,
and they have never varied since -- 16426 and 1909 exactly, across a dozen
runs and every subsequent change.

Presentation was the obvious suspect, since it added module entry points the
page calls. **It is not**: disabling `display_backend`'s surface work entirely
leaves the count at 16426.

The likeliest reading is that the re-entrancy is **pre-existing and was
exposed** rather than introduced. Before those fixes the context latched
early, so most cases stopped doing GL work almost immediately; afterwards they
run to completion, including the JSPI readbacks that suspend mid-submit. More
work reaching the device means more chances to re-enter it. Consistent with
that, the same change that revealed the throws also *reduced* failures, 427 to
349 -- the fixes were a net gain that uncovered a deeper problem.

Two guards were added and both are correct, but neither removed the throws:

* `serveGl` serialises submits behind an `inFlight` promise (submit-vs-submit).
* `MesaGlDevice` caches `presented`/`produced` and skips `display()` while a
  submit is suspended (the harness polls `__gexGlState()` every animation
  frame, which called into the module).

So some other caller still enters the module while JSPI has it parked. Every
entry point in `MesaGlDevice` was enumerated and the remaining ones
(`_malloc`, `_free`, `_gex_gl_reply_*`) run either before the suspend or after
it resolves.

That points somewhere different in kind. **JSPI re-entry does not have to come
from our JavaScript.** When `gex_wgpu_jspi_wait()` parks the module mid-submit,
WebGPU's own completion callbacks -- `gex_wgpu_map_callback` and the queue
callbacks -- are dispatched back into wasm on that same pthread, by design.
If the driver is holding a Mesa lock across the suspend and anything on the
callback path takes it again, musl reports exactly what is seen:
`"pthread mutex deadlock detected"`. That is a self-deadlock inside the
module, not a caller racing it, which is consistent with two JS-side guards
changing nothing.

The count supports it: 16426 throws over 1578 cases is about ten per case,
which is the order of readbacks per case, not the order of animation frames.

There are exactly three suspend points, all the same shape --
`wgpuBufferMapAsync` followed by `gex_wgpu_jspi_wait()` -- in `texture_map`,
`buffer_map`, and the occlusion-query readback.

And the bridge itself has a defect worth fixing regardless
(`web/gex-wgpu-jspi.js`):

```js
gex_wgpu_jspi_wait: function () {
  return new Promise(function (resolve) {
    Module['__gexWgpuResolve'] = resolve;   /* a single slot */
  });
}
```

One slot for all suspends. A second suspend beginning while one is
outstanding overwrites the first's resolve, and the first never completes.
That is reachable precisely because WebGPU dispatches its map callbacks back
into the suspended module -- so a callback that leads to another readback
starts a second suspend inside the first. A queue keyed per waiter, or an
assertion that the slot is empty, would both fix it and make a violation
loud instead of silent.

### The module aborts once, in u_blitter, and everything after is noise

The invariant 16426 was the clue that mattered. `abort` appears 16424 times
but `___assert_fail` only **23**: the module aborts, and every later call
throws the same "Aborted(...)" message. The count never moved for any fix
because it is not a count of failures -- it is a count of submits made after
the module was already dead.

The stack names the site:

```
at gex-wgpu-replay.wasm.util_blitter_blit_generic
at gex-wgpu-replay.wasm.util_blitter_blit
Aborted(Assertion failed: ... "pthread mutex deadlock detected")
```

`u_blitter` calls back into driver hooks, and the abort happens while it is
running. The obvious mechanism -- one of those hooks suspending for a JSPI
readback -- does **not** hold up on inspection: `buffer_map` suspends only for
`PIPE_MAP_READ` on a STREAM_OUTPUT or SHADER_BUFFER resource, and
`u_blitter`'s upload is a write map on a vertex buffer. `texture_map` is not
on the blit path at all. So the three known suspend points are all excluded.

That leaves a plain recursive lock rather than a suspend-and-re-enter. The
wasm is built with `-pthread` (emcc warns about it at link), and emdawn's
`wgpu*` entry points can proxy with a lock in that configuration; a WebGPU
call made from inside another proxied call would deadlock exactly this way,
with no JSPI involvement. That is a hypothesis, not a finding -- the harness
truncates the wasm stack to two frames, which is why the callee is unnamed.

Getting the full stack is the next step. The harness keeps only two frames;
running the same case under `tests/mesa-wgpu-replay.sh` with a trace that
reproduces the blit, or capturing the browser's own console rather than the
reflowing terminal, would name the call under `util_blitter_blit_generic`.

The full chain, captured by logging wasm frame names compactly (the harness
mangles a real stack, and that is why the callee stayed unnamed for so long):

```
gex_gl_submit -> glGetTexImage -> _mesa_GetTexImage -> get_texture_image
  -> st_GetTexSubImage -> gex_wgpu_blit -> util_blitter_blit
     -> util_blitter_blit_generic -> Aborted(pthread mutex deadlock detected)
```

It is a **format-converting** blit (`fmt 81->53`, same size, no scissor or
swizzle), which is exactly the case the direct copy path cannot take and hands
to `u_blitter`.

**The blit is now declined instead**, and the results are worth stating
carefully:

| | module aborts | reported failures |
| --- | --- | --- |
| `u_blitter` (before) | yes, ~611 cases in | 349 |
| declined (now) | **no -- zero throws** | **615** |

615 is worse-looking and more honest. The 349 was produced by a module that
had already died: every case after the abort ran against a corpse, and a dead
context passes negative tests for free. All 1578 cases are now genuinely
evaluated.

Applications are unaffected or better -- SuperTuxKart presents 357 frames
against 326 before, and `mesa-wgpu-live` is unchanged -- because an aborted
module takes the whole application down, where a declined blit costs one
operation.

**This is containment, not a fix.** Restoring `u_blitter` should recover most
of the gap (the pre-crash baseline with the blitter working was 427), so the
real task is the fault itself. What is known about it:

* The first error is **`memory access out of bounds`**, not the mutex abort.
  The assertion comes afterwards, from a module already broken -- which is why
  16424 aborts sit behind 23 real assertions. Chase the OOB, not the deadlock.
* It is **inside `util_blitter_blit_generic` with no frame below it**. In wasm
  that shape is usually an indirect call through a null pointer -- a
  `pipe_context` hook the driver leaves unset.
* The blit that triggers it is small and ordinary:
  `r8_unorm -> r8g8b8a8_unorm`, 7x3, no scissor, swizzle, blend or condition.
  Both formats are supported and no shader is rejected, so neither a missing
  format nor a refused shader explains it.
* Four hooks `u_blitter` calls are NULL here -- `bind_tcs_state`,
  `bind_tes_state`, `bind_ms_state`, `set_window_rectangles` -- and **all four
  were checked and are guarded off**: tessellation and mesh by zero
  `shader_caps[...].max_instructions`, window rectangles by
  `saved_num_window_rectangles`. So the null-hook theory does not survive
  either, on the hooks that are obviously missing.

Run under ASan (build the sanitize tree, then `ninja` the
`gex-wgpu-replay.js` target into it -- the sanitize branch of
`web/build-mesa-wasm.sh` does not build that target -- and point
`GEX_MESA_WASM_OUT` at it), the verdict is precise:

```
AddressSanitizer: null-pointer-dereference on address 0x00000004
READ of size 4 at 0x00000004 thread T0
```

A struct field read through a null pointer, inside
`util_blitter_blit_generic`. ASan prints only a raw address -- the wasm ASan
runtime does not symbolize -- but the named stack from the ordinary build
supplies the function.

The mutex abort is fully explained as a *consequence*:

```
glReadPixels -> _mesa_ReadPixels -> _mesa_update_state
  -> _mesa_lock_context_textures -> mtx_lock -> deadlock
```

The fault leaves Mesa's context-texture lock **held**, so the next
`glReadPixels` deadlocks on it and asserts. That is the abort seen 16424
times, and it is why chasing the deadlock never led anywhere: it is two
failures downstream of the null read.

Most likely remaining candidate: `u_blitter` uses `pipe->create_sampler_view`
for the blit source and does not null-check it, and this driver's
`create_sampler_view` returns NULL for templates it refuses -- among them any
whose `templ->format` differs from the texture's. Reading `->texture` off that
NULL is a size-4 read at offset 4, which matches exactly. Verify by logging
refusals in `gex_wgpu_create_sampler_view` with the blit re-enabled.

**Re-measure everything before trusting it.** Any run in this document with
16426 throws in it -- which includes the per-family table above -- describes a
crash as much as a driver.

Still true and still unexplained, re-verified after the fixes:

```
gexgl: GEN texture 243              gexgl: framebuffer texture 245 unknown
gexgl: GEN texture 247              gexgl: framebuffer texture 249 unknown
```

The guest generates the name (its own log said `glGenTextures -> 245`), no
batch is discarded, the submit succeeds -- and the decoder never executes that
command. Two candidates remain, and they are distinguishable:

1. **The batch is truncated in transit.** `GlProxy.submit` forwards
   `HEADER_BYTES + tail` bytes, reading `tail` once from the guest header. It
   also has its own silent refusal -- `if (HEADER_BYTES + tail >
   this.#data.length) return -1` -- which returns without setting a status,
   the same shape as the bug just fixed. Worth making that one speak too.
2. **The command is overwritten before submit.** The guest's `ring->tail` is
   reset by the host writing `H_TAIL = 0`, copied back into the guest ring by
   `GlProxy`. If that copy-back ever lands after the guest has queued more
   commands, the next write starts at zero and clobbers them.

Instrument the guest's `flush()` to log `tail` before each submit and the
decoder to log the byte count it received. If they disagree, it is (1); if
they agree and the command is still missing, it is (2).

A warning about the harness while doing so: dumping the raw opcode stream
showed `op 1247`, which is not a valid opcode -- the maximum is 165 -- and
looked exactly like stream desynchronisation. It was not. The harness captures
a reflowing terminal, and two lines merged in the capture. No batch was ever
rejected, which is the check that disproves it. Verify any "impossible" value
against a second signal before believing it.

**A fix that did not work, so it is not retried:** making `BIND_TEXTURE`
create a texture for an unknown non-zero name. That is correct GL -- binding
an unused name creates the object, and `glGen*` reserving names is optional --
and it is kept, because the previous behaviour silently redirected such a bind
to the default texture of the target. But it changes nothing here: 349
failures before and after, 1909 messages before and after. The application
never binds the name it attaches.

(The Mesa-side prints were temporary and are reverted; `git diff` on
`src/mesa/main/fbobject.c` is empty. Re-add them the same way if needed --
that file's diagnostics are the only thing that explains a completeness
failure, and they are unreachable in this build without patching.)

Six hypotheses have now measured flat or negative in this family:

| hypothesis | result |
| --- | --- |
| depth readback fixes `texture_repeat_mode` | failed at shader compile, never reached readback |
| depth16 is a free capability | cost 456 cases across two families |
| missing BGRA sRGB format explains the FBO failures | flat |
| `EXT_texture_integer` unadvertised, so the integer check is skipped | guard is `Version >= 30`, and the context is 3.2 |
| `dest_surface_srgb_control` unset, so EXT_framebuffer_sRGB is off | flat: byte-identical census |
| the refused `a8b8g8r8_srgb` render target explains the sRGB FBO failures | it is probing; `r8g8b8a8_srgb` is accepted |

The last one is the most instructive. The cap really is unset, st/mesa really
does gate `EXT_framebuffer_sRGB` on it, and the driver really does have the
destination-surface sRGB machinery (`viewFormats` declares the linear/sRGB
counterpart; the render pass picks per attachment). Every step of the argument
holds and the measurement was unchanged, so the block is upstream of the cap.
It is left unset: advertising it would be a capability claim no passing test
supports.

**A measurement warning for whoever continues.** Message counts are not case
counts. 1909 FBO messages live inside 349 failing cases that fail for other
reasons as well, so eliminating a message class entirely can move the score by
zero. Rank work by cases recovered, never by message frequency -- three of the
five hypotheses above were chosen on message counts.

### Measured state after this work, and why the headline went down

### Root cause found and fixed: a refused sampler view

`gex_wgpu_create_sampler_view` required `templ->format == texture->format`.
GL routinely asks for a **linear view of an sRGB texture** -- that is how sRGB
data is read undecoded -- and WebGPU allows it, because `resource_create`
already declares the counterpart in `viewFormats`. The driver refused a view
it could legally create, and the whole chain followed:

```
create_sampler_view refuses r8g8b8a8_unorm view of r8g8b8a8_srgb -> NULL
  -> u_blitter dereferences it unchecked -> READ of size 4 at address 0x4
  -> module faults, Mesa's context-texture lock left held
  -> every later glReadPixels deadlocks on it -> 16424 aborts
  -> every case after runs against a dead module -> 1909 "FBO is not complete"
```

One refused view destroyed entire runs. Allowing sRGB/linear pairs fixes it:

| family | baseline | with the crash | fixed |
| --- | --- | --- | --- |
| `packed_pixels.rectangle` | 1151 | 1229 (inflated) | **1409** |
| `packed_pixels.pbo_rectangle` | 1150 | 1228 (inflated) | **1408** |
| `packed_pixels.varied_rectangle` | 962 | 477 (deflated) | **962** |
| `texture_repeat_mode` | ~28 | 161 | **161** |
| `shaders30.glsl_constructors` | 646 | 648 | **648** |

Zero device throws in every one. About **4676 of 5661**, from a 3940
baseline.

**The format-validation conclusion was exactly backwards.** The 16091
"Invalid format used but glTexImage2D/3D succeeded" in `varied_rectangle` is
gone with the crash fixed -- the same artifact, since a dead module makes the
guest's `glGetError` return zero and every negative test then reports that an
invalid format succeeded.

Redone against MobileGL with clean numbers, the split reverses:

| message | gex_wgpu (clean) | MobileGL |
| --- | --- | --- |
| Invalid format used but glTexImage2D/3D succeeded | **0** | 7303 |
| Invalid format used but glReadPixels succeeded | **0** | 12995 |

Format validation on the Mesa path is *correct*, and it is MobileGL that
accepts what it should reject. The earlier reading -- "shared, therefore not
this driver's defect, therefore driver-side work would not move it" -- was our
crash-era numbers against their healthy ones. It removed a large piece of
apparent work that never existed, and it pointed at the wrong backend.

What is actually left in both families, measured clean:

| count | message |
| --- | --- |
| 928 | Integer comparison |
| 750 | Gradient comparison failed during ReadPixels |
| 470 | Gradient comparison failed during GetTexImage |
| 292 | Non-integer comparison |
| 18 | Valid format used but glReadPixels failed |

Those are readback correctness and precision -- real driver work, and the
first honest picture of it this document has carried.

Two further corrections this forces on earlier entries:

* The "`varied_rectangle` fell 962 -> 477 because dead contexts stopped
  passing negative tests" analysis was itself an artifact. With the module
  alive it is 962 again. The reasoning about dead contexts passing negative
  tests is sound in general; it was not what happened here.
* Every "measured flat" result during the crash era -- the sRGB cap, the bind
  fix, the JSPI queue, the batch-discard fix -- was measured against a module
  that had already died, so none of them was actually tested.

Two have since been re-measured with the module healthy, and both original
conclusions survive:

* **depth16**: 169 failures without it, **616** with. Same mechanism as
  before -- advertising it lets st/mesa build depth-only framebuffers and
  `draw refused (no colour target)` latches the context. Stays out, now on
  evidence rather than a crash-era number.
* **`dest_surface_srgb_control`**: 169 either way, zero throws either way.
  Genuinely neutral. Stays out under the advertise-only-what-a-test-proves
  rule.

The bind fix and the JSPI queue fix remain untested by measurement; both are
correct on their own reading of the specs and neither costs anything.

### Every family in the GL 3.2 mustpass, measured before the crash was fixed:

| family | cases | passing |
| --- | --- | --- |
| `packed_pixels.rectangle` | 1578 | **1229** (was 1151) |
| `packed_pixels.pbo_rectangle` | 1577 | **1228** (was 1150) |
| `packed_pixels.varied_rectangle` | 1577 | **477** (was 962 -- see below) |
| `shaders30.glsl_constructors` | 648 | **648** (was 646) |
| `texture_repeat_mode` | 162 | **161** (was ~28) |
| `packed_depth_stencil` | 25 | 24 |
| `transform_feedback` | 21 | **11** (was 10) |
| `ext_texture_shadow_lod` | 15 | 15 |
| `transform_feedback_overflow_query_ARB` | 13 | 13 |
| `CommonBugs` | 8 | 7 |
| `info` | 6 | 6 |
| `buffer_objects` | 5 | 3 |
| `clip_distance` | 3 | 1 |
| `framebuffer_blit` | 3 | 1 |
| `gpu_shader5_gl` | 3 | 3 |
| `transform_feedback3` | 3 | 0 |
| `texture_storage` | 1 | 1 |
| `transform_feedback2` | 1 | 1 |

About 3831 of 5661, against a 3940 baseline. Every family moved up or held
except `varied_rectangle`, which accounts for the whole of the decline and
more.

`transform_feedback3` is legitimately unimplemented rather than broken: it
needs `ARB_transform_feedback3`, which st/mesa gates on
`caps.stream_output_interleave_buffers`, and its `multiple_streams` case needs
geometry-shader output streams that this driver refuses outright. Advertising
the cap would make that case reach the refusal and latch -- the depth16
mistake again -- so the cap stays unset and the three cases stay failed.

Four families up, one down hard, and the total lands slightly *below* the
baseline. The drop is real and it is also the point.

`varied_rectangle` never latches now -- there is no `first refusal` line in
the run at all -- so its negative tests actually execute. They had been
"passing" because a failed context errors on everything, including on the
`glTexImage2D` calls a negative test wants to see fail. With the context alive
the driver has to answer honestly, and it answers wrongly **16091 times**:
*"Invalid format used but glTexImage2D/3D succeeded"*.

So the headline number was partly measuring how often the driver was dead.
That is worth more than the number: **the format-validation gap is now the
single dominant defect in the suite, unconfounded and counted**, where before
it was hidden behind latched contexts. It is also the thing the fragment-output
refusal was standing in for -- the note above about that comparison being
contaminated now has its evidence.

What is left, by message, now that contexts stay alive:

| count | message | where |
| --- | --- | --- |
| 16091 | Invalid format used but glTexImage2D/3D succeeded | `varied_rectangle` only |
| 2850-3538 | Gradient comparison failed during GetTexImage | all three |
| 1909 | FBO is not complete but expected complete with sizedFormat | all three |
| 1769 | Non-integer comparison | all three |

The invalid-format acceptance is confined to `varied_rectangle`, which is the
subgroup that varies the PixelStore properties -- so it is a validation gap
that only shows when unpack state is non-default, not a general one. That is
worth knowing before rewriting format validation wholesale.

The FBO completeness failures are the ones common to all three, and the
example cases name `srgb8_alpha8`. That led to adding
`PIPE_FORMAT_B8G8R8A8_SRGB`, which was genuinely missing from the format table
-- a BGRA texture had no sRGB counterpart to declare in `viewFormats`, and
WebGPU rejects a linear/sRGB view not declared at creation.

**It changed nothing.** `rectangle` stayed at 349 and `varied_rectangle` at
1100, exactly. The format is still worth having (it is real, it is renderable,
and the viewFormats pairing was genuinely absent) and it costs nothing, but
the FBO completeness failures have some other cause and the `srgb8_alpha8`
example was a coincidence of ordering -- it is simply the first case the
wrapper recorded, not a representative one.

Recording that because the same shape of guess has now been wrong four times
in this family: a plausible cause inferred from a partial signal, acted on,
and measured flat.

* depth readback would fix `texture_repeat_mode` -- it failed at shader
  compilation and never reached readback.
* depth16 is a free capability -- it cost 456 cases across two families.
* the missing BGRA sRGB format explains the FBO failures -- measured flat.
* `EXT_texture_integer` is unadvertised, so Mesa skips the integer/non-integer
  check -- the guard is `ctx->Version >= 30 || EXT_texture_integer`, and the
  context is 3.2, so the check fires regardless.

The common cause is the instrument, not the reasoning. The wrapper recorded
the *first* three cases per message, which is whatever ran first and not a
sample. It now records three early and three late, and that immediately
changed the picture for the dominant message: early is
`r11f_g11f_b10f_format_bgr_integer` and late is `compressed_signed_rg_rgtc2`,
so the wrongly-accepted combinations span unrelated internalformat families.
It is a broad validation gap, not one format -- which also means the single
example that sent me to `EXT_texture_integer` was never representative.

Final census for `varied_rectangle`, counted in-guest:

| count | message |
| --- | --- |
| 16091 | Invalid format used but glTexImage2D/3D succeeded |
| 1909 | FBO is not complete but expected complete with sizedFormat |
| 1369 | Integer comparison |
| 370 | Invalid format used but glReadPixels succeeded |
| 50 | Expected error but got no GL error |

Three of those five are the stack accepting what it should reject. **It is not
this driver.** The same subgroup run on the MobileGL backend -- same guest,
same transport, an entirely different GL implementation -- fails the same
checks:

| message | gex_wgpu | MobileGL |
| --- | --- | --- |
| Invalid format used but glTexImage2D/3D succeeded | 16091 | 7303 |
| Invalid format used but glReadPixels succeeded | 370 | 12995 |

So format validation is shared-layer or test-configuration, and **driver-side
format work would not have moved it at all**. That control should have been
the first move rather than the fifth; it cost one run and retired four
hypotheses at once.

What it is not, from checks already done:

* Not lost GL errors. The guest's `glGetError` appends and flushes, so it
  orders after the call that raises the error, and the decoder calls
  `glGetError` in exactly one place.
* Not silently dropped calls. `glTexImage2D` sends even when the guest cannot
  compute a pixel layout (`len` is 0, not a sentinel), and `glReadPixels`
  flushes and executes host-side, skipping only the local copy.
* Not an ES/desktop context mismatch. The test branches on
  `isContextTypeES`, and the driver logs `api=1` -- a desktop request -- so
  the desktop rules are the ones being applied.

Where to look next is the guest's pixel-layout table (`gexgl_compute_pixel_layout`)
and the `coreValidFormats`/`isFormatValid` rules the CTS applies, compared
against what a known-conformant desktop driver does with the same case list.
Not the Gallium driver.

### A real application found five defects the CTS never reached

SuperTuxKart -- a stock aarch64 binary, not anything written for this project
-- now runs against st/mesa + gex_wgpu with **no decoder rejection, no driver
refusal on its main path, and zero WebGPU validation errors**. Getting there
took five fixes, and the CTS had surfaced none of them:

1. **`set_sampler_views` latched on any non-fragment stage**, including a plain
   unbind. `bind_sampler_states` directly above it already reasons about this
   -- cso_context unbinds every stage on bind and teardown -- and had the
   allowance; this one did not. Every draw after was silently dropped.
2. **`set_constant_buffer` had the identical defect**, for the same reason.
   Refusing a *clear* is refusing to do nothing.
3. **The context-version gate killed the transport.** STK asks for 4.4, got
   3.2, and the gate destroyed the context and returned failure -- but the
   guest's `context_create` has no failure path, so it surfaced as
   CONTEXT_CREATE failing, which the decoder reports as a protocol error,
   which latches the transport permanently. The session died on the *request*
   for 4.4, never on a use of it. Now granted, with `glGetString` reporting
   the real version the guest passes through verbatim.
4. **Framebuffer attachments always drew into mip level zero.** The level and
   layer were asserted to be zero rather than carried into the view, so
   rendering a mip chain was refused outright. WebGPU expresses this directly
   with `baseMipLevel`/`baseArrayLayer`.
5. **Attributeless draws were refused**, keyed on leftover bound buffers --
   which the comment above the check already says are routine.

Fix 5 is worth recording as a mistake. Removing the refusal outright let a
shader that *does* read attributes reach the device with no vertex layout,
which is a validation error ("Vertex attribute slot 0 used in ShaderModule")
-- 32745 of them, the device poisoned for every later command, the worst
outcome available. The bound buffers were the wrong question and so was no
question at all: the shader's own `nir->info.inputs_read` is what decides it.

**Presentation now exists.** `display_backend()` in the Mesa build used to
increment a counter and copy no pixels -- the MobileGL branch calls
`DirectWebGPU::Display()` and the Mesa branch had no equivalent -- so the
canvas stayed uniformly black however well the driver rendered, and a
601-frame run proved only that 601 swaps completed. It now configures a canvas
surface (RGBA8Unorm, matching the visual's colour format exactly, because
WebGPU will not copy between rgba8unorm and bgra8unorm) and copies the
drawable into it. Two small accessors in `gex_wgpu_public.h` hand out the
texture and device; Gallium has no notion of a canvas, and that is narrower
than teaching the driver about surfaces.

A sixth fix followed from running with it: **a zero vertex stride was
refused**. Zero stride is a constant attribute -- every vertex reads the same
element -- and WebGPU expresses it directly as `arrayStride: 0`, bounding the
offset by `maxVertexBufferArrayStride` instead. Refusing it rejected the whole
vertex-elements state, which left nothing bound, which turned an ordinary draw
into a latched context.

Where this leaves it: STK runs with **no decoder rejection, no driver refusal,
zero validation errors**, presenting ~336 frames, and its canvas output is
pixel-identical to the MobileGL backend's at the same point. That is the
strongest evidence available so far, and it is still not gameplay: both
backends are on STK's loading screen when the harness captures, so what
matches is a loading screen. Confirming the race itself needs a longer run
than the current harness window.

**Gameplay could not be confirmed, on either backend.** STK's assets are not
in the rootfs this harness loads -- 387 "Cannot determine texture full path"
-- so it never leaves its loading screen. That is pre-existing and independent
of the GL backend: `tests/stk-webgpu-fps.sh`, the project's own MobileGL gate,
fails the same way (38.70 FPS against a 59.8 requirement, same 387 messages).
So "a real 3.2 app confirmed working" stands at: the application runs, draws,
and presents, with canvas output identical to the reference backend, and
gameplay unverified for reasons that have nothing to do with this driver.

### A sanitizer finding the gate had stopped reaching

`gex_wgpu_gs_passthrough_nir` built its shader with `NULL` compiler options,
so `nir->options` was null and NIR dereferenced it unconditionally --
`nir_cleanup_functions` reads `options->driver_functions`. Nothing crashed,
which is why it survived; UBSan caught it. Fixed by passing the screen's own
options.

Worth knowing about the gate itself: `halt_on_error=1` is applied to the
**softpipe** half only. The browser half prints UBSan findings and passes
anyway, so it reports PASS over live undefined behaviour -- there are 8 such
reports, all misaligned 8-byte access in Mesa's own
`nir_opt_load_store_vectorize` list walk, unexamined. A gate that is weaker
than it looks is the recurring theme of this work, and this is another one.

Also open: something in the stack offers GLSL ES 3.00 shaders to a desktop
core context that supports only up to 1.50.

### packed_pixels is a long tail, not one bug

Counted inside the guest, where the whole QPA file is readable:

```
internal  rgb10_a2ui=8/19 rgba12=8/19 rgba16=8/19 rgba16_snorm=8/19 srgb8=8/19
readback  red=27/83 rg=27/83 rgb=27/83 rgba=27/83 bgr=27/83 bgra=27/83 green=25/83
```

Both dimensions are flat. Every internal format fails about 8 of its 19
readback formats; every readback format fails about 27 of the 83 internal
formats. The 427 failures are a matrix of individual (internalformat,
readback) gaps, so **no single fix moves this family by hundreds of cases** --
which is what the message census (3890 "glTexImage2D failed") suggests at
first glance and does not actually support.

Since packed_pixels is 4732 of the 5661-case mustpass, that also caps how far
the headline percentage can move without broad format work: 16-bit unorm,
three-component formats, and the sized legacy formats each need real backing
that WebGPU does not provide directly.

Three separate analyses of this family were wrong before this one, each
because the captured output was lossy and read as complete: positional QPA
attribution, a tail mistaken for a census, and a case/result pairing over
dropped lines. The tally is computed in-guest and printed on two lines for
exactly that reason.

### The measurement that set the fragment-output policy was contaminated

`gex_fail(ctx)` latches, and a latched context errors on *everything*. That is
fatal to a comparison against the CTS's format tests, which come in both
directions: a dead context fails every "valid format must succeed" test and
passes every "invalid format must fail" test **without doing any work**. The
"refusing scores 71%, permitting scores 63%" number that set this policy was
partly measuring that artifact -- the same trap already noted for
`varied_rectangle`, not recognised as applying here.

Attribution shows it directly: in the refusing build, 36 of 39 surviving
messages are *"Valid format used but glTexImage2D/3D failed"* -- the driver
rejecting what GL requires it to accept.

So the refusal no longer latches. Returning early already keeps the pipeline
WebGPU would reject out of the device, which is the whole point of the check;
latching only added collateral, exactly as the depth-stencil upload measured.
This is a third configuration -- refuse *without* latching -- that was never
tested, and the two numbers above cannot rank it.

Not generalised to the other latch sites. Most are genuine "cannot do this,
don't pretend" cases where silent wrongness is worse than a dead context;
`draw_vbo`'s is distinguishable because skipping the draw *is* the containment.

### 16-bit unorm is a real gap, not a driver downgrade

`GL_RGBA16` readback comes back quantised to 8 bits (expected 1/14, got
18/255 = round(255/14)/255, against a 1/32768 tolerance). The format table has
no 16-bit unorm entry at all, so this is st/mesa falling back to RGBA8 rather
than the driver silently losing precision. WebGPU has no `rgba16unorm`;
closing it means backing the format with `rgba32float`, which is exact but
costs 2x memory. Small slice -- 2 of 39 sampled messages -- and unstarted.

### QPA messages are attributable now

The CTS's own explanations -- "Gradient comparison failed during GetTexImage",
"Invalid format used but glTexImage2D/3D succeeded" -- live in the QPA log,
which `glcts-log-wrapper` dumps **after the whole run**. So a message could not
be traced to a case: correlating by position attributes every one of them to
the last case that ran, which is wrong and looks right. One analysis in this
work reported 39 failures against a single case on exactly that mistake.

The log carries `#beginTestCaseResult <path>` markers and the wrapper was
skipping them. It emits them as `QPA-CASE` lines now, so everything after one
belongs to that case. Rebuild it with the guest toolchain after editing:

```sh
aarch64-linux-musl-gcc -O2 tests/cts/glcts-log-wrapper.c \
    -o web/.work/gl44-cts/glcts-log-wrapper
```

This is the same lesson as the driver's refusals, one level up: **a diagnostic
that does not say which costs a run every time it is consulted.**

### The small families did not move, and that is the finding

The twenty-three families outside `packed_pixels` and `shaders30` were
re-swept after roughly ten driver fixes -- topologies, upload coherency, the
missing entry points, texture write maps, format sizing, sRGB views,
`u_blitter`, the float32 features. They went from **26/159 to 28/161**.

That is worth knowing precisely because it is negative: those families are not
waiting on anything already fixed, and at 161 cases between them they are not
where the remaining work is either. Of 1721 outstanding cases, **1469 are in
`packed_pixels`**.

The exception is `texture_repeat_mode`, 162 cases in the mustpass.

**The diagnosis recorded here was wrong, and it is worth keeping the mistake
visible.** It read: the depth-stencil readback decline aborts the run, so
depth readback through a render pass is worth about 98 cases here. None of
that held. The cases never reached readback -- all 162 failed at *shader
compilation*, which the in-guest QPA census showed as `Compile failed` 162
times. The estimate came from reasoning about a plausible cause instead of
reading what the test said, and it sent the next session (this one) to build
depth support for a family that did not need it.

The real cause: `gex_wgpu_split_combined_samplers` ran only for
`PIPE_SHADER_IR_TGSI`. A GLSL `sampler2D` is itself a combined sampler and
WebGPU has none -- it wants a texture binding and a sampler binding -- so every
shader st/mesa produced from GLSL that samples arrived with its derefs already
lowered away, the texture metadata came out empty, and the shader was rejected
for having "no dereferenced texture slots". An ordinary
`texture(texture0, texCoord)` was enough to trigger it.

Running the split for NIR shaders too took the family from **162 failures to
10**.

### Depth16 was added and then taken back out

`PIPE_FORMAT_Z16_UNORM` is not in the format table, and the omission is now
deliberate rather than accidental. Adding it -- WebGPU has `depth16unorm` and
will copy out of it, so it looked free -- let st/mesa build **depth-only
framebuffers**, which this driver cannot draw into. "draw refused (no colour
target)" latches the context, and the cost was measured: 267 lost cases in
`packed_pixels.rectangle` and 189 in `pbo_rectangle`, against no measurable
gain. WebGPU supports depth-only render passes; this driver does not build
them yet, and that is the prerequisite.

Two things from that attempt were kept because they stand on their own:

* The readback path refused *every* depth format, when WebGPU only refuses the
  packed ones -- it will copy the depth aspect of `depth32float`; `depth24plus`
  is the one with no defined memory layout. The refusal is now specific, with
  `DepthOnly` aspect on the copy.
* The depth attachment check derived its accepted formats from a hand-written
  list, which is how the regression got its reach: a format added to the table
  and to `is_format_supported` but not to that list latches on attachment. It
  derives from the table now, the way the sampler path already did.

The sequencing lesson is the durable one. Advertising a capability is not one
edit -- every consumer that gates on a format list has to agree, or the new
capability turns into a latch. And the regression was invisible from the
family it was aimed at: `texture_repeat_mode` improved exactly as intended
while two families nobody was looking at lost more than the gain.

### Large families have to be chunked

`shaders30` is 648 `glsl_constructors` cases and the guest is killed for memory
(`exit 137`) long before any of them fail. Run in slices it passes cleanly:

```
GEX_CTS_CASE='KHR-GL32.shaders30.glsl_constructors.vec4_from*'  ->  72/72
```

Chunk globs come from the mustpass itself: group the case names on a prefix
deep enough that no group exceeds what one process survives. Two underscore
tokens of the leaf name gives eleven groups of at most 72 for `shaders30`,
which is both small enough to run and coarse enough not to pay browser startup
ninety-two times.

So the family is not failing; the harness is asking for too much at once.
`packed_pixels` needed the same treatment for the same reason -- run whole it
reports `error 0/0`, run as its three subgroups it reports 2887/4732.

The clean fix is an explicit case list rather than globs, and deqp does support
one: `--deqp-caselist-file` reads a plain list of names when the file does not
begin with `{`. It is **not wired up**, and the reason is recorded here so the
next attempt does not repeat it: passing the list needs both
`glarg=--deqp-caselist-file=/fs/NAME` *and* `glfile=/fs/NAME`, because `glfile`
is what puts a file into the guest's VFS -- and with that second `glfile`
present the wrapper then fails to exec the runner at all
(`execv glcts: No such file or directory`, exit 126). Something about two
`glfile` values breaks the loader path in `web/src/demos.js`; a single one has
always worked. Until that is understood, chunk with globs derived from the
mustpass case names, which is proven.

### One blit refusal explains 615 failures

`packed_pixels.rectangle` reports 963 passing and 615 failing, and the split
looked meaningful until the pattern came out: for every internal format, the
**valid** read combinations all fail and the **invalid** ones all pass. That is
not a driver that renders some formats wrong. It is a dead context -- the
invalid combinations are negative tests, and once every GL call fails they all
"pass".

The CTS says what happened, in the QPA log the wrapper surfaces as `QPA` lines:

```
QPA Valid format used but glTexImage2D/3D failed
gex-wgpu: blit refused: 7x3->7x3 fmt 81->53 res 81->53 ... mask=0xf
gex-wgpu: first refusal at gex_wgpu_blit:4704
```

One format-converting blit, refused, latched the context near the start of the
run. Everything after it failed. **Roughly 1845 cases across the three
subgroups hang on that single refusal.**

The fix is the standard one: a blit that cannot be a copy becomes a textured
draw, which is what `util_blitter` exists for. It needs a prerequisite that is
now in place -- **the driver accepts TGSI**. `u_blitter` builds its shaders
through `ureg`, and `ureg_create_shader` calls `create_vs_state` with TGSI
tokens directly; it does no NIR conversion of its own, whatever the driver's
`supported_irs` says. `tgsi_to_nir` at the top of `create_shader_state` closes
that gap, and costs nothing for the NIR path.

The blitter is now created, its state save/restore is wired, and a blit the
copy path cannot serve reaches it -- `blit through u_blitter: 7x3->7x3 fmt
81->53` appears where the refusal used to be. Retaining two pieces of state in
Gallium shape was needed for that: the driver kept a decomposed framebuffer
and vertex-buffer list, and the blitter restores by handing them straight back
to `set_*_state`.

**It does not work yet, and the blocker is exact.** The blitter's fragment
shader is rejected:

```
gex-wgpu: shader rejected: gex_wgpu requires dereferenced, consistently
                           typed texture slots
gex-wgpu: draw refused (no fragment shader) mode=4 count=6 ...
```

The reason is not what the message suggests, and the correction matters
because it changes what to build.

`tgsi_to_nir` *does* use derefs -- `nir_tex_src_texture_deref` and
`nir_tex_src_sampler_deref`, both pointing at one variable. What it does not
use is the **split** texture/sampler form. It creates a single
`glsl_sampler_type` variable: a *combined* sampler. The driver's collection
loop looks for `glsl_type_is_texture` and `glsl_type_is_bare_sampler`, and a
combined sampler is neither -- hence `textures=0 samplers=0` in its own
diagnostic while the shader plainly samples a texture.

Zink survives this because Vulkan has combined image-samplers and its SPIR-V
emitter simply emits one. **WGSL does not.** A texture and a sampler are
separate resources there, and they cannot share a `@group`/`@binding` pair,
which is what a single combined variable would force. So the shortest-looking
fix -- teach the collection loop about combined samplers -- ends at Tint
instead of at the driver.

`gex_wgpu_split_combined_samplers` does that, and the blitter's shader now
compiles and draws. Getting there needed three things, and the first two were
each a wrong turn worth recording:

1. **The derefs are gone by the time the driver sees the shader.**
   `tgsi_to_nir` builds `nir_tex_src_texture_deref` sources and then ends with
   `nir_lower_samplers`, which replaces them with plain `texture_index`. So
   both readings of the original error were true at once: the variable is a
   combined sampler *and* the instruction addresses it by index. A pass that
   only repointed existing derefs rewrote nothing -- `tex=1 candidates=0`.
   The derefs have to be *rebuilt* from the index, not redirected.
2. **The sampler half is often unused.** A blit that does not filter -- a 1:1
   copy is one -- fetches texels, so `u_blitter`'s shader has a texture and no
   sampler. Creating the pair unconditionally left a sampler variable nothing
   sampled with, which the driver rejects, and rightly: it would be a binding
   the pipeline requires and no draw provides. `nir_remove_dead_variables`
   after the rebuild drops it.
3. The pair shares a `driver_location` -- the driver's slot model expects a
   texture and its sampler to agree on that -- and takes two bindings, which is
   what makes them separately addressable in WGSL.

### Five refusals in a row, each found by naming it

With the shader compiling, the blit hit one refusal after another, and each was
found the same way: make the refusal say *which condition* rejected the call
and what the values were, then read it. No guessing was involved after the
first one, and every step took a single run.

| refusal | what it was | fix |
| --- | --- | --- |
| `draw_vbo` "no fragment shader" | the blitter's shader was rejected and it bound nothing | split combined samplers |
| viewport outside framebuffer | depth range `-1..1` | clamp into `[0, 1]` |
| `set_vertex_buffers` "no resource" | an empty slot is an *unbind*, not an error | skip empty slots |
| `set_viewport_states` | an all-zero viewport restored by the blitter | record it as "none set" |

Three of those were real driver bugs with nothing to do with blitting:

- **An empty vertex buffer slot killed the context.** Gallium's
  `set_vertex_buffers` takes a null resource in a slot to mean unbind, and the
  binding loop already handled it correctly -- only the validation above it
  refused. Any caller unbinding a subset hit this.
- **A restored all-zero viewport killed the context.** It is what a caller puts
  back when this context never had one. Not drawable, but not an error either:
  the draw path already refuses to draw without a viewport.
- **Depth ranges outside `[0, 1]` killed the context.** `glDepthRange` clamps
  to that, so such a viewport never came from an application; Mesa's internal
  helpers set the conventional full-range transform because it only matters to
  a draw that writes depth, and theirs do not.

Two more fell out the same way afterwards:

| refusal | what it was | fix |
| --- | --- | --- |
| binding validation | sample type "differs from the shader's" | derive the type from Mesa |
| `set_framebuffer_state` | the surface reinterprets the texture's format | see below |

**The sample type was the same drift as `format_bytes`, in a quieter form.**
`gex_wgpu_format_sample_type` was a short hand-written switch, so any format
not on it fell through to `Float`. Every 16- and 32-bit integer format was
missing, which meant an integer texture read by an integer shader was reported
as a *mismatch* and the draw refused -- shader says Uint, driver says Float.
It now asks `util_format_is_pure_uint` and friends. That is the third table in
this driver to drift from the formats it accepts, and the third to be fixed by
deriving the answer instead of listing it.

**The last one looked like a platform limit and was not.** A blit whose
destination surface reinterprets its texture's format needs a render target
viewed as a different format, and WebGPU allows that only for srgb/non-srgb
pairs. Reading the diagnostic instead of stopping at that sentence showed the
actual pair: surface `R8G8B8A8_UNORM` over texture `R8G8B8A8_SRGB` -- which is
*exactly* the reinterpretation WebGPU permits.

It needs three things, none of them exotic, and the driver had none:

- the texture must declare the alternative in `viewFormats` **at creation**,
  or the view is rejected later;
- the colour attachment's view must be created with the surface's format
  rather than defaulted from the texture;
- the pipeline's colour target format must follow the surface too.

The driver kept only `colors[i]`, the texture, and had thrown the surface
format away. It keeps `color_formats[i]` now. That is `GL_FRAMEBUFFER_SRGB`
working, which st/mesa also leans on for ordinary blits.

General reinterpretation is still unavailable -- WebGPU has only this one
pair -- but nothing observed so far has asked for more.

Six refusals, six single-run diagnoses, no guesses after the first. The
technique is the finding here as much as any individual fix: **a refusal that
names its condition and its values costs one run to resolve; a refusal that
names a line number costs a build.**

**What the six were worth:** `packed_pixels.rectangle` went from
**963/1578 (61.0%)** to **1123/1578 (71.2%)** -- 160 cases in one subgroup.
Measuring it needed a much longer wall than before, which is itself the
signal: the cases now render instead of failing instantly, so the run takes
real time.

The improvement is **not** uniform across the family, and that matters for
reading the number: `pbo_rectangle` and `varied_rectangle` are both unchanged
at **962/1577** -- the same figure to the case, which says they stop for one
shared reason that is not on the chain just fixed. All 160 recovered cases are
in `rectangle`. Projecting the subgroup's 61% -> 71% across the family would
have claimed roughly 71% overall; the measured figure is 65.7%, and the
difference is exactly that assumption.

The remaining failures in `rectangle` are no longer one dead context, and the
shape of them is now worth something. Of 396 failures across 82 internal
formats: **13 formats pass completely, one fails completely, and 68 are
mixed** -- which is what per-format driver behaviour looks like, rather than
what a dead context looks like.

They cluster:

- **21 are compressed formats** read back as uncompressed. The readback copies
  raw blocks; nothing decompresses them.
- The rest cluster on **float and sRGB internal formats** -- `srgb8`,
  `srgb8_alpha8`, and every one of `r16f` through `rgba32f` -- each failing
  exactly eight read formats. Three-component float formats are worth checking
  first: WebGPU has no RGB formats at all, only R, RG and RGBA, so
  `rgb16f` and `rgb32f` cannot exist as such and must be something else
  underneath.

That list is the actionable map this family did not have before, and after the
sRGB fix it sharpened again. The failures that remain in `rectangle` report

```
QPA Gradient comparison failed during GetTexImage for input = [GL_BGRA, GL_FLOAT]
```

-- a *value* mismatch, not an API failure. Nothing is refused; the pixels come
back wrong. The internal formats involved are the float and integer ones:
`r16f` through `rgba32f`, `r11f_g11f_b10f`, and `r8i`/`r8ui`/`r16i`/`r16ui`/
`r32i` and friends, each failing about eight read combinations. sRGB is down to
six failures from sixteen.

Cross-tabbing internal format against read format says where to look. For a
float texture **every non-integer read fails**; for an integer texture **every
integer read fails**. In both cases the combinations that fail are exactly the
*valid* ones -- the passes are negative tests -- so this is not a subset of
formats being wrong, it is the readback being wrong wherever it is actually
exercised:

```
r32f    FAIL bgr bgra blue green red rg rgb rgba      PASS *_integer, depth_*
r8ui    FAIL *_integer (all eight)                    PASS bgr bgra blue ...
rgba8   FAIL bgra red rg rgb rgba                     PASS bgr blue green ...
```

`rgba8` failing only five of its valid reads, rather than all of them, is the
useful anomaly: whatever is wrong is not uniform per format.

`glGetTexImage` reaches this driver as a blit into a staging texture followed
by a map. The map round-trip is covered by the `texture-write-map` corpus case
and passes, which leaves the blit -- and every blit now goes through
`u_blitter`'s shader, newly enabled. **A corpus case that blits between two
formats and checks the result is the next thing to write**, on the same
reasoning that made the write-map case worth adding: the CTS says the answer
is wrong, and nothing smaller than the CTS currently says so.

Fixing the sRGB view added **52 more** to `rectangle` (1123 -> 1175) and
removed the last refusal from all three subgroups: none of them ends in a dead
context any more. It moved `pbo_rectangle` and `varied_rectangle` not at all,
which is worth stating plainly -- their 615 failures each were never the
refusal, they only *looked* like it because the refusal was there too.

What those two were actually hitting was a *resource allocation* refusal, and
the log said so once it was asked:

```
gex-wgpu: buffer resource refused: width0=132 bind=0xa unsupported=0xa usage=4
QPA Error during glGetTexImage
```

`0xa` is `PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW` on a **buffer**.
st/mesa asks for those so it has the option of a texture-buffer path it may
never take. Refusing the allocation is the wrong place to say no: nothing here
can render to or sample a buffer, and the calls that would try refuse on their
own, whereas a null resource surfaces as a GL error from whatever unrelated
operation happened to allocate it -- `glGetTexImage` with a pixel buffer
bound, in this case. The allocation and `is_format_supported` accept them now.

That is **+212 cases** in `pbo_rectangle` alone, 962 -> 1174/1577, which lands
it level with `rectangle`.

### Two diagnostics that should have existed sooner

Both of these cost a build cycle every time something new was rejected, and
both are now permanent.

**Shader rejections were silent.** `gex_wgpu_set_compile_error` only recorded
the message when the caller set `report_compile_error` -- which st/mesa and the
corpus do, and Mesa's internal helpers do not. A shader the driver rejected for
`u_blitter` therefore vanished, and the only symptom was a draw refused much
later for having no fragment shader. It now says so regardless of who asked.

**The draw refusal named a line, not a reason.** It is the busiest refusal in
the driver and had a dozen conditions behind one `gex_fail`. It now names which
one, with the draw's shape:

```
gex-wgpu: draw refused (no fragment shader) mode=4 count=6 isz=2
          instances=1 colours=1 vs=1 fs=0
```

### Conformance is now measurable

`tests/gl44-cts-browser.sh` takes `GEX_CTS_BACKEND=mesa`, and
`tests/gl44-cts-sweep.sh` takes `GEX_CTS_MODULE` and `GEX_CTS_MUSTPASS`, so the
official Khronos CTS can be pointed at Mesa on `gex_wgpu` at the version the
driver actually reaches:

```
GEX_CTS_BACKEND=mesa GEX_CTS_MODULE=KHR-GL32 \
GEX_CTS_MUSTPASS=.../mustpass/gl/khronos_mustpass/main/gl32-main.txt \
GEX_CTS_SWEEP_OUT=web/.work/gl32-sweep-mesa tests/gl44-cts-sweep.sh
```

The GL 3.2 mustpass is **5661 cases across 25 families**. Before this the only
CTS numbers in this project were MobileGL's; `gex_wgpu` had never seen a
conformance case. Sweeping GL 4.4 against it would have measured context
creation rather than conformance, which is why the module is a parameter.

**The first sweep measured nothing, and the way it failed is worth keeping.**
It reported 21/280 with whole families failing identically -- 162 cases of
"Compile failed" in `texture_repeat_mode` alone. The cause was not GL: the
CTS's rootfs carried a *stale* `libEGL.so.1`, built before the guest shim moved
context creation out of `eglInitialize` and into `eglCreateContext`. That copy
never sent `CONTEXT_CREATE`, so the host never built a Mesa context, so every
GL call was a no-op and every shader "failed to compile".

Nothing in the run said so. The tell was an absence: `report_version_gap()`
prints on every context creation and there was no such line anywhere in the
log. `create_backend_context` now also prints what the guest *asked* for --

```
gex-wgpu: context requested api=1 3.2
gex-wgpu: context is 3.2 (Core Profile) Mesa 26.3.0-devel / 1.50
```

-- because "which context did the guest actually get" was the question the
whole sweep turned on, and it was previously unanswerable from the logs.

The guest GL shim is staged into three places -- `web/public/fs`,
`web/dist/fs` and the CTS build's `guest-gl` -- and a conformance run is only
as current as the copy its rootfs resolves. Rebuild and restage all three
before trusting a sweep.

### A missing entry point costs a whole family

With a real context, the second sweep still showed families dying at their
second or third case. `KHR-GL32.transform_feedback` trapped:

```
trapped inside the shim (memory access out of bounds, guest pc=0x4a61eb8,
lr=0x0, after syscall 29)
```

`lr=0x0` is the tell. An entry point the shim does not export resolves to
zero, the guest calls it, and the runtime traps -- so one missing symbol takes
out every case after it, and the family reports as a crash rather than as the
one failure it is.

The registry makes this measurable rather than anecdotal. `gl.xml` in
`third_party/opengl-registry` lists every command a version requires, and the
shim's exports are in its dynamic symbol table:

```sh
readelf -W --dyn-syms guest/gl/out/libGLESv2.so.2 |
    awk '$4=="FUNC" && $7!="UND" {print $8}'
```

`-W` matters: without it `readelf` truncates long symbol names and the diff
invents missing commands that are present. That produced a confident "49
missing" that was entirely an artifact.

Nine GL 3.2 commands were genuinely missing, all of them queries that return
through a guest pointer -- which is exactly the shape the shim's generator
fails closed on, so they had to be written by hand:

| command | why it needed hand-marshalling |
| --- | --- |
| `glGetTransformFeedbackVarying` | same reply shape as `GET_ACTIVE_UNIFORM` |
| `glGetShaderSource` | same reply shape as `GET_PROGRAM_INFO_LOG` |
| `glGetActiveUniformName`, `glGetActiveUniformBlockName` | a name for one indexed thing |
| `glGetActiveUniformBlockiv` | one query returns N integers, and N is itself a query |
| `glGetActiveUniformsiv` | an array of indices in, an array of integers out |
| `glGetUniformIndices` | an array of strings in; they travel as one blob of NUL-terminated names |
| `glGetBufferPointerv`, `glGetVertexAttribPointerv` | answered from guest state -- the host was never told a guest pointer |

`glVertexAttribPointer` now records its offset guest-side for the last of
those; nothing else knew it.

Adding `glGetTransformFeedbackVarying` alone took
`KHR-GL32.transform_feedback` from **trapped at case 2 with nothing passing**
to **10/21 passing with all 21 cases running**. All 316 GL 3.2 core commands
are exported now. GL 3.3 is at 344/374; the 30 missing are the packed-vertex
`glColorP3ui`-style family that `ARB_vertex_type_2_10_10_10_rev` needs.

### A use-after-free in sampler_view_release

Found by reaching GL 3.1, which was enough to make st/mesa hold a sampler view
more than once:

```
RuntimeError: operation does not support unaligned accesses
  at gex_wgpu_sampler_view_release
  at st_destroy_context
```

The hook destroyed the view outright. Gallium's contract -- and
`u_default_sampler_view_release` -- is that release means *unreference*, and
destroy happens at zero. Any view held twice was freed while still referenced,
and the next release decremented a reference count through a dangling pointer.
In wasm that surfaces as a misaligned atomic, nowhere near the cause, in a
teardown path, on a context that was only created to be discarded.

### How GL 3.0 was reached

Two requirements stood between GL 2.1 and 3.0: multisampling and transform
feedback. Both are done and proven by corpus cases.

**4x multisampling** is the requirement that was hiding. `compute_version()`
gates GL 3.0 on two *constants* as well as its extension list --
`MaxColorAttachments >= 4` and `MaxSamples >= 4 || FakeSWMSAA` -- and the
reporter only checked extensions, so it cheerfully printed "0 requirements"
while the version stayed at 2.1. It checks both now.

WebGPU supports 4x multisampling natively, so this is ordinary driver work,
not another emulation project. Softpipe reaches GL 3.0 by setting
`fake_sw_msaa`, which advertises multisampling and implements none; Mesa
sanctions it in a comment, and it is deliberately not done here -- the goal is
conformance, and WebGPU can do the real thing.

**Done and proven.** Multisampled resources carry a `sampleCount`, the render
pipeline carries a matching one, and `blit` resolves through a render pass with
a `resolveTarget` -- WebGPU has no resolve command; a resolve is the side
effect of ending such a pass. The corpus case `multisample-4x-resolve` renders
to a 4x target and resolves it, and passes on real WebGPU.

The bug that made this look impossible for a while: `multisample.count` was
assigned from the framebuffer and then **hardcoded back to 1 sixty lines
later** in the same descriptor. Dawn's message named it exactly --

```
expects an attachment state of { colorTargets: [0={RGBA8Unorm}], sampleCount: 4 }
has        an attachment state of { colorTargets: [0={RGBA8Unorm}], sampleCount: 1 }
```

-- which is worth remembering: reading the *whole* validation message first
would have saved several rounds of guessing at depth attachments and formats.

Three things this case needed that are easy to get wrong:

- A resolve is performed by **sampling** the multisampled texture, so such a
  texture needs the sampler-view bind as well as the render-target one; both
  the corpus and `is_format_supported` must allow it.
- The case must skip backends reporting `fake_sw_msaa`. Softpipe advertises
  four samples and stores one, so a resolve there proves nothing.
- It draws a **fullscreen** triangle. With real multisampling the pixels along
  a triangle edge resolve to a blend of both sides, so an exact-match check
  only means anything where no edge crosses the surface -- the first version
  failed with `48,160,144`, which was green and blue averaged, i.e. the
  feature working and the test being wrong.

### Transform feedback runs as a compute pass -- done

This was the last requirement for GL 3.0, and it is implemented and proven by
the corpus `transform-feedback` case, which checks the captured buffer
component by component on both softpipe and WebGPU. The approach recorded
earlier -- have the vertex shader store its own captures into a storage
buffer -- **does not work on WebGPU**. The device reports
`maxStorageBuffersInVertexStage: 10`, which is what made it look possible, but
that limit counts *read-only* storage buffers. WGSL forbids the writable kind
in the vertex stage, and Tint says so exactly:

```
var with 'storage' address space and 'read_write' access mode
cannot be used by vertex pipeline stage
```

So transform feedback needs the vertex shader run a second time **as a compute
shader** that writes the captures, with vertex fetch emulated against the
vertex buffers -- the same shape Asahi uses for geometry stages. That is a
substantially larger piece of work than the storage-buffer write, and it is
the honest cost of GL 3.0 on this backend.

How it is built:

- `gex_wgpu_create_shader_state` keeps a clone of a capturing vertex shader in
  `shader->capture_nir`. The render shader is left alone; the clone is what
  becomes compute.
- `gex_wgpu_capture_variant_get` builds the compute variant and caches it per
  **vertex layout**, because the variant reads the vertex buffers itself and a
  different layout is a different shader. The passes run in a fixed order:
  attribute reads and vertex-stage system values become compute terms first,
  the capture stores are emitted from the output writes, and only then do
  those writes go away.
- Attribute reads become loads from the vertex buffers bound as storage
  buffers. Only four vertex formats reach the driver -- `u_vbuf` translates
  everything else -- so the fetch handles `R32{,G32,G32B32,G32B32A32}_FLOAT`
  and `R8G8B8A8_UNORM`, and fills what a format does not supply with
  `(0, 0, 0, 1)` the way GL does.
- `gl_VertexID` becomes the invocation index plus the draw's first vertex; the
  slot captured into is the invocation index alone.
- The capture stores are the same `store_deref` walk as before. Mesa's
  `nir_lower_xfb_to_stores` is still unusable: it rewrites `store_output`
  *intrinsics*, and this driver keeps outputs as variables and derefs all the
  way down because that is the form the SPIR-V emitter consumes.
- Byte offsets travel in a parameter buffer rather than in the binding, because
  a storage binding offset must be 256-byte aligned and neither a GL vertex
  buffer nor a transform feedback range promises more than four.
- Invocations past the vertex count return early. WebGPU's bounds checking
  does not cover this: the capture buffer is usually longer than the draw, so
  an over-run write lands *inside* the buffer, past the captured range.

`GL_RASTERIZER_DISCARD` came with it. The rasterizer state is built from a
whitelist and `rasterizer_discard` was not on it, so such a state was refused
outright -- honest, but it meant no capture-only draw could exist. It is
supported now, and a discarding draw needs neither a fragment shader nor a
colour target.

Two things learned the hard way, both worth keeping:

- **The bind group layout must say `Storage`, not `ReadOnlyStorage`, even for
  buffers the shader only reads.** The emitter declares every storage buffer
  `read_write`, and WebGPU rejects a layout that disagrees with the shader's
  own declaration:

  ```
  The buffer type in the shader (BufferBindingType::Storage) is not
  compatible with the type in the layout (BufferBindingType::ReadOnlyStorage).
  ```

  Setting `ACCESS_NON_WRITEABLE` on the NIR variable does emit
  `SpvDecorationNonWritable`, but Tint does not turn that into a `read` access
  mode, so it does not help.
- `pipe_stream_output_info::register_index` is the index of the shader
  *output*, so capturing register zero silently records `gl_Position` rather
  than the intended varying -- which is why the corpus checks the expected
  values component by component. The corpus and the driver must also agree on
  the description twice: once as `pipe_stream_output_info` for Gallium, and
  once as `nir_xfb_info` for the lowering.

### A depth-stencil state the sanitizer caught

Two corpus cases unbound the depth-stencil-alpha state before drawing to a
framebuffer with no depth attachment, with a comment claiming that was
required. It is wrong twice over: in Gallium a NULL DSA state means *no state
object*, which softpipe dereferences unconditionally when it builds its quad
pipeline, and whether a depth attachment exists is the framebuffer's business
-- `gex_wgpu` already decides `depthStencil` from `ctx->depth`, not from the
bound state. The cases bind the depth-disabled base state now.

Only the sanitizer build found this, and it found it as a null dereference in
softpipe rather than anywhere near the corpus:

```
sp_quad_pipe.c:46:27: runtime error: member access within null pointer of
type 'struct pipe_depth_stencil_alpha_state'
```

### A bind-group overflow I introduced

Raising `GEX_WGPU_MAX_CONSTANT_BUFFERS` from 8 to 16 left the bind-group entry
arrays at a literal `[8]`. A shader declaring nine or more uniform blocks would
have overflowed them on the stack. They are now sized from the maxima they
hold.

### A cap I had over-advertised

The adapter reports `maxUniformBuffersPerShaderStage: 12`, and the driver was
advertising `max_const_buffers = 16` after last iteration's change -- the
request for 16 is correctly skipped when the adapter cannot meet it, but the
cap was not clamped to match. A shader using a thirteenth uniform buffer would
have failed at pipeline creation. Caps are now clamped to the limits
`wgpuDeviceGetLimits` reports. The practical consequence is that
`ARB_uniform_buffer_object` needs 13 buffers per stage (12 blocks plus GL's
default block) and this adapter offers 12, so it is honestly out of reach here;
it is a GL 3.1 requirement, so the 3.0 rung is unaffected.

### Note on the route to STK

**GLES 3.0 is the cheaper target than desktop GL 3.3.** Desktop 3.2 requires
geometry shaders, which WebGPU lacks entirely and which need compute
emulation; ES 3.0 requires none. Both ladders converge on transform feedback
first, so it is the right next work either way.

What closed the rest:

- `ARB_shadow` was the single requirement between GL 1.3 and 1.4, and the
  driver already does depth comparison -- the corpus covers implicit, explicit
  and gradient shadow lookups. Setting `texture_shadow_map` jumped it four
  rungs to 2.1 at once.
- **`gex_wgpu` never called `u_init_pipe_screen_caps`**, which every other
  Gallium driver uses for the GL-minimum cap defaults. That is the root of the
  long tail of zeroed caps found one confusing failure at a time -- a zero
  `max_vertex_buffers` made `u_vbuf` treat every vertex element as
  incompatible; a zero `max_constant_buffer_size` withheld
  ARB_uniform_buffer_object. It is now called first, with the driver's own
  answers layered on top.
- 32-bit and 16-bit integer colour formats, which `EXT_texture_integer` needs.

One diagnosis worth keeping: `ctx->Const.GLSLVersion` is **not** usable as an
input. Mesa clamps it down to match the GL version it settled on *after*
computing that version, so reading it back reports the consequence as if it
were a cause. The reporter reads `screen->caps.glsl_feature_level`, which is
what the version computation actually sees.

### blit, and what the cap defaults exposed

Adopting the Gallium defaults immediately steered st/mesa into its ReadPixels
staging cache, which calls `pipe->blit` -- and the driver had none, so the
whole gears trace rendered black. The `mesa-wgpu-replay` gate caught it as a
255/255 mean difference, which is what that tolerant metric exists for.

`blit` and `resource_copy_region` are now implemented on WebGPU's
`copyTextureToTexture`, restricted to what a copy actually is: same format,
same size, no scaling, scissor, swizzle or blend. Anything else is refused
rather than approximated.

The one exception is a vertical flip, which GL asks for whenever the window
origin disagrees with the texture's -- the ReadPixels staging blit always
does. WebGPU has no flipping copy, so it is done as one copy per row inside a
single encoder: O(height) commands, correct, and cheap enough for a readback
path, but a textured-quad blitter should replace it.

softpipe runs the identical pipeline, so **GL 3.3 core is demonstrably
reachable through Mesa + this transport**; `gex_wgpu`'s ceiling is its own
caps, not the architecture. Note what desktop costs above that: GL 3.2 needs
geometry shaders, and GL 4.0 needs `ARB_tessellation_shader`,
`ARB_gpu_shader_fp64` and `ARB_transform_feedback2/3`. WebGPU has none of
those, and WGSL has no double precision at all.

Two driver defects fell out of exercising context teardown:

- `bind_sampler_states` refused any stage but fragment, including the
  all-NULL unbind `cso_context` issues for every stage whenever a context is
  bound or torn down.
- `sampler_view_release` was never set. st/mesa drains its zombie sampler-view
  list through it during teardown without checking, so a refused context
  version became a crash instead of a fallback.

## GL 3.2 conformance, measured 2026-08-28

Every number below comes from a run that completed. `QPA-TOTAL pass=N fail=M
skip=K` is printed by `tests/cts/glcts-log-wrapper.c` from the QPA log itself,
which is complete, rather than from dEQP's console summary, which the
harness's reflowing terminal capture drops. `skip` is `NotSupported`, which is
a conformant result: dEQP counts it apart from `Failed` and a mustpass run
passes with it.

| family | pass | fail | skip | of |
| --- | --- | --- | --- | --- |
| `packed_pixels.rectangle` | 1578 | 0 | 0 | 1578 |
| `packed_pixels.pbo_rectangle` | 1577 | 0 | 0 | 1577 |
| `packed_pixels.varied_rectangle` | 1577 | 0 | 0 | 1577 |
| `shaders30` | 651 | 0 | 0 | 651 |
| `texture_repeat_mode` | 162 | 0 | 0 | 162 |
| `packed_depth_stencil` | 21 | 2 | 2 | 25 |
| `transform_feedback` | 16 | 5 | 0 | 21 |
| `ext_texture_shadow_lod` | 0 | 0 | 16 | 15 |
| `transform_feedback_overflow_query_ARB` | 0 | 0 | 13 | 13 |
| `CommonBugs` | 7 | 0 | 1 | 8 |
| `info` | 6 | 0 | 0 | 6 |
| `buffer_objects` | 5 | 0 | 0 | 5 |
| `clip_distance` | 2 | 1 | 0 | 3 |
| `gpu_shader5_gl` | 0 | 0 | 3 | 3 |
| `transform_feedback3` | 0 | 3 | 0 | 3 |
| `framebuffer_blit` | 0 | 3 | 0 | 3 (runtime trap, no results) |
| `texture_storage`, `frag_coord_conventions`, `transform_feedback2` | 0 | 0 | 3 | 3 |
| `api`, `draw_buffers`, `get_uniform_tests`, `glsl_noperspective`, `texture_lod_basic` | 5 | 0 | 0 | 5 |
| `primitive_restart`, `texture_size_promotion`, `texture_lod_bias` | 0 | 3 | 0 | 3 |

**5607 pass, 37 NotSupported, 17 fail of 5661 -- 99.7% of the cases that are
not a conformant `NotSupported`.** The session began at 3831 passing -- and
that 3831 was itself measured over runs that aborted partway, so the true
starting point was lower. Five of the seventeen are one emulator bug and two
are one WebGPU gap, both described below.

Nothing regressed across the whole suite in the run that produced this table:
`packed_pixels` is now clean in all three subgroups, `shaders30` is 651/651
and `texture_repeat_mode` 162/162.

Two caveats on the table's arithmetic, both worth keeping visible. The
`ext_texture_shadow_lod` row is measured by running the family glob, which
contains 16 cases, one more than the 15 the mustpass names; the 37 above
counts the mustpass's 15. And the five cases with no results
(`framebuffer_blit`'s 3, `texture_lod_bias`, `texture_size_promotion`) are
counted as failures, because a case that cannot be run has not passed.

`tests/mesa-wgpu-replay.sh` also went from a worst-channel difference of 1.50
against softpipe to **0.00**: the rasterizer fix below meant face culling and
winding order took effect for the first time, and the two backends now agree
pixel for pixel on a real recorded frame.

### The one structural fact that dominated everything

dEQP turns **GL_OUT_OF_MEMORY, and only GL_OUT_OF_MEMORY, into a fatal
`tcu::ResourceError`** (`framework/opengl/gluDefs.cpp:75`, `tcuDefs.hpp:129`,
`tcuTestSessionExecutor.cpp:347`), and a fatal error **terminates the whole
run**. st/mesa raises GL_OUT_OF_MEMORY whenever a Gallium hook returns NULL.

So in this suite a refused allocation does not cost one case. It costs every
case after it, and dEQP then reports the shrunken denominator as if it were
the family: `texture_repeat_mode` was recorded as **63/65** for a family of
**162**, because the run aborted at case 65. The sweep script took that 65 as
the total, so the family looked 97% conformant while 60% of it had never run.

Two rules follow, and most of this session's work is one or the other:

* A driver refusal must never surface as GL_OUT_OF_MEMORY if any other answer
  is available. Declining a draw costs the draw; latching costs the run.
* A measurement has to state how many cases *ran*. `QPA-TOTAL` does.

### What was wrong, in the order it was worth fixing

**A no-op mistaken for a failure, which cost 97 cases and looked like a format
gap.** `gex_wgpu_lower_texture_variant` returned `progress && !table.failed`,
so a shader that needed no lowering was indistinguishable from one that could
not be lowered, and the caller latched the context on the difference. st/mesa
gives every depth texture a non-identity swizzle, so this fired on the first
`depth_component16` case; the resulting GL_OUT_OF_MEMORY was observed by the
*next* case's `glCreateProgram` and ended the run. `texture_repeat_mode` went
63/162 to **162/162**.

The same reading found a real gap behind it: the swizzle was applied only by
the integer-sampling pass and the 2D-float border pass, so a swizzled 1D, 3D,
cube, array or shadow texture silently dropped it.
`gex_wgpu_lower_view_swizzle_instr` now owns exactly the instructions those
two do not reach.

**A capability query used as an allocation gate.** `resource_create` re-tested
`is_format_supported` with the binds st/mesa asked for -- and st asks for
`PIPE_BIND_RENDER_TARGET` on every texture whose format is renderable,
whatever its target. The query said no for a 3D texture, the allocation
returned NULL, and `glTexImage3D` of an ordinary `GL_RED` texture reported
GL_OUT_OF_MEMORY: **4626 occurrences of "Valid format used but
glTexImage2D/3D failed"** in `varied_rectangle`, the only subgroup that uses
`GL_TEXTURE_3D`. Allocation now checks what it actually needs -- a nameable
format, a legal shape, an honourable sample count -- and 3D textures are
attached through a `3d` view and `depthSlice`, which is how WebGPU renders to
one.

**A format-converting blit is a pixel transfer, not a textured quad.** st/mesa
expresses several GL pixel operations as a blit between two textures of
different formats: an upload whose client type does not match the texture's
goes into a staging texture of the client's format and is blitted in, and
glGetTexImage comes back the same way. GL's meaning there is a *conversion* --
an integer value too large for the destination clamps, it does not wrap -- and
u_blitter's quad cannot express it. `gex_wgpu_blit_by_transfer` reads the
source back and runs `util_format_translate`, with a two-step path through a
32-bit intermediate for the mixed-signedness case `util_format_translate`
refuses outright. That took `packed_pixels.rectangle` from 63 failures to 18,
and allowing a compressed side took it from 18 to **2**.

**16-bit normalized formats had no backing at all.** WebGPU has no 16-bit
unorm or snorm texture format, so st/mesa's fallback list for `GL_R16` ran off
the end into 8-bit RGBA and lost half the precision the test measures. They
are backed by float32 now, which holds every one of their values exactly, with
`util_format_translate` on transfer, gated on `float32-filterable` and
`float32-blendable`.

**`Z16_UNORM` went back in.** It was absent twice because a depth-only
framebuffer had no render pass to draw into. That pass exists now, and without
the format `GL_DEPTH_COMPONENT16` had no Gallium format at all -- which is the
run-ending refusal above, not a missing feature.

**glGetTexImage overwrote the caller's pack padding.** It copied the whole
`[offset, end)` run, so the bytes `GL_PACK_ROW_LENGTH` and
`GL_PACK_SKIP_PIXELS` leave between rows came back as the host staging
vector's zero fill. glReadPixels had always scattered row by row; this did
not. The reply carries the row and image strides now so the guest can.
(PROTOCOL_VERSION 6 to 7 -- `web/dist` must be rebuilt with the guest driver.)

**Compressed textures could not be read back.** `texture_map`'s read path
required a bytes-per-pixel, which a block format does not have. It sizes the
copy in blocks now.

**Interpolation had to be reconciled between the stages.** WGSL requires a
varying's interpolation to match on both sides of the pipeline; GLSL 1.50 does
not, and takes the fragment stage's qualifier as the meaning. The driver did
this for TGSI only, so a GLSL program with `flat` or `noperspective` on the
fragment side produced a WebGPU validation error -- which poisons the device.
`gex_wgpu_interp_variant_get` builds a vertex variant keyed on the fragment
stage's signature. It is keyed by `VARYING_SLOT_*`, not `driver_location`:
the two stages number their locations independently.

**An unimplemented NIR opcode aborted the whole device.** zink's
`nir_to_spirv` ends its ALU switch in `UNREACHABLE`, and
`KHR-GL32.get_uniform_tests` reaches it with `b2b1`. Every bool in that
emitter has bit size one, so `b2b1` is the identity.

**`clip_distance` needed two things, not one.** `compact_arrays` was unset, so
the GLSL linker packed `gl_ClipDistance` into a `vec4[2]` that SPIR-V forbids
as a `ClipDistance` builtin; with that fixed Tint reported that the
`clip_distances` WGSL extension was not enabled, because nothing requested the
WebGPU feature. Both are done; the family went 1/3 to 2/3.

**The CTS's own data archive was never staged.** A `ShaderLibraryGroup` reads
its `.test` file when the group is entered and a missing one throws
`tcu::ResourceError` out of `init()`, which is fatal. `KHR-GL32.shaders30`
died in five seconds on an 8 KiB file; it is 651/651 now.
`tests/gl44-cts-browser.sh` stages `gl_cts/data/{gl30,common}`, and
`web/test/drive.mjs` had to stop collapsing repeated `glfile` query values,
which had been silently dropping the guest binary itself.

Smaller, each named where the code refuses it: colour-attachment holes are
carried as WebGPU's null attachment instead of latching; a viewport outside
the framebuffer declines the draw instead of latching; `glGen*`/`glDelete*`
with a negative count reach GL rather than sizing a four-billion-element
vector; `gl_PointSize` stores are stripped, because Tint rejects any value but
a constant 1.0; the vertex-element cap went from 8 to 16, which is what WebGPU
guarantees and what `max_inputs` already advertised; primitive-count queries
are answered from a CPU count rather than NULL, because NULL was
GL_OUT_OF_MEMORY and therefore the run; transform feedback assembles decomposed
primitives through a CPU-built slot table, so a triangle strip captures
triangles rather than strip vertices.

**Every rasterizer state was being refused.** `create_rasterizer_state`
compares the incoming state against a copy holding only the fields this driver
acts on and returns NULL on any difference -- and a refused state binds as
NULL, which reads as every default: no scissor, no face culling, counter-
clockwise winding, and no rasterizer discard. st/mesa sets
`point_quad_rasterization` and `sprite_coord_mode` on every state it builds,
u_blitter sets `flatshade`, and st sets `line_stipple_pattern` to 0xffff
whether or not stippling is on, so *no* state had ever been accepted and none
of those four features had ever worked. The point and line fields WebGPU fixes
are accepted and ignored now, with the reasoning written where they are
listed; polygon mode, polygon offset, smoothing, stipple, clip planes,
two-sided colour, colour clamping and the provoking vertex are still refused.
Turning off `caps->flatshade` instead, so st lowers flat shading in NIR, was
tried and reverted: the lowering reaches an ALU opcode zink's SPIR-V emitter
does not implement, which aborts the module.

**Client-side index arrays were never transported.** `glDrawElements` may name
a client array rather than an offset into `GL_ELEMENT_ARRAY_BUFFER`, and the
guest sent the pointer as an offset -- so the host dereferenced its own
address space and read whatever was there. Every indexed draw from a client
array rendered vertex zero four times. The indices travel with the command
now, and the decoder passes them straight to GL.
(PROTOCOL_VERSION 7 to 8.)

**Transform feedback captures assembled primitives, and its stride is in
bytes.** Two separate defects in the same path. The capture pass wrote one
slot per index; GL records decomposed primitives in primitive order, so a
four-vertex triangle strip records two triangles and a line loop records a
closing line the index buffer never names. The mapping is built on the CPU now
and read as a slot table, which also subsumes the index buffer. Separately,
`nir_xfb_info`'s buffer stride was being multiplied by four -- it is already
in bytes -- which put every captured vertex four strides apart; invisible in a
one-vertex capture, and every transform feedback case that passed drew a
single point. GL also stops capturing on a whole primitive when a bound buffer
would overflow, and the queries report what was written rather than what was
generated; that is modelled now, which is what the query and discard cases
measure.

**A viewport outside the framebuffer is folded into the vertex shader.** GL
allows one and clips; WebGPU's `setViewport` requires the rectangle to be
inside the attachment. The transform is affine, so it is applied to
`gl_Position` instead and WebGPU is given the whole framebuffer -- a vertex
variant keyed on the four folded floats, sharing the machinery the
interpolation variant introduced. Draws were being declined outright before.

**`render_condition_enable` is not a refusal.** It asks a blit or clear to
respect an active conditional render, and this driver implements no
`set_render_condition`, so none can be active -- but st/mesa sets the flag on
everything it issues, which sent every same-format depth copy to u_blitter and
latched the context on ordinary clears.

### A second pass, and the two defects behind most of it

Everything in this section was found after the table above was first written,
by taking one failing family at a time and refusing to stop at "the driver
refuses X". Two of the seven were single-line mistakes standing between the
driver and a whole class of correct behaviour.

**A depth-stencil state compared by memcmp, which refused every stencil test
silently.** `gex_wgpu_create_dsa_state` rebuilt the parts of
`pipe_depth_stencil_alpha_state` it could serve into a zeroed copy and
compared the whole struct against it. That made every *disabled* feature's
leftover bytes part of the comparison -- and st/mesa fills the back face,
`stencil[1]`, with the front face's func, ops and masks whether or not
two-sided stencil is on. So an ordinary single-sided `glStencilFunc` never
matched, and the state was refused.

Nothing said so, and that is the whole shape of it: cso stores the NULL,
`bind_depth_stencil_alpha_state` sets `ctx->dsa` to NULL, and the pipeline is
then built with `depthWriteEnabled` false and `Keep` for every stencil op. The
draw runs, passes validation, and changes nothing at all. `packed_depth_stencil`
read depth back as 1.0 and stencil as 0 -- the clear values -- for exactly the
two textures its `renderToTextures` draws with `GL_STENCIL_TEST` enabled, and
the other three passed. The state is now checked field by field, every refusal
prints, and two-sided stencil is served rather than refused: WebGPU has
separate `stencilFront` and `stencilBack`, so the only part of GL's two-sided
state it cannot express is a per-face mask, and that is what is now checked.
`packed_depth_stencil` went 13 -> 21 and `CommonBugs` 6 -> 7.

**Vertex elements numbered by element index, not by the location the module
declares.** The pipeline set `shaderLocation = i`, the Gallium element index.
The module's `@location` is assigned much later, by zink's
`ntv_shader_prepare`, which recomputes `driver_location` from
`var->data.location` as "generic attribute number, or the fixed-function slot
for anything below `VERT_ATTRIB_GENERIC0`". The two agree only when a shader's
generic attributes start at zero and run contiguously.

`buffer_objects.multi_texture` binds attributes 1..3 and not 0. Its module
declared `@location(1..3)`; the pipeline offered 0..2. WebGPU rejected it --
"vertex attribute slot 3 used in ShaderModule is not present in the
VertexState" -- and a rejected pipeline is a validation error, which poisons
the device. Worse than the rejection is what the accepted cases were doing:
wherever the two numbering schemes differed without a gap, attribute data was
being fed to the wrong shader input and nothing complained. The shader now
records which `@location` each element feeds, using ntv's own rule, and the
pipeline uses that table.

Alongside it, GL lets a shader read an attribute whose array is disabled; it
gets the current generic value, and WebGPU has no such thing. The locations a
module declares that no element supplies are now filled from a 32-byte buffer
holding `(0,0,0,1)` as floats and as integers, on a layout with
`arrayStride` 0 so every vertex reads the same value.

**A conformance level that was being reported and not served.**
`is_format_supported` answers `PIPE_BUFFER` + `PIPE_BIND_VERTEX_BUFFER` by
asking whether the driver can name the format as a WebGPU vertex format, and
`PIPE_FORMAT_B8G8R8A8_UNORM` was not in that table. st/mesa reads exactly that
question for `EXT_vertex_array_bgra`, GL 3.2 requires the extension, and
without it Mesa computes a **3.1** context -- which caps GLSL at 1.40, so every
`#version 150` shader in the suite fails to compile. WebGPU has the format:
`unorm8x4-bgra`, the same bytes in the same order. Naming it restored 3.2.
This is worth stating plainly because of how it presented: the driver went on
reporting that the application had asked for 3.2 and been granted a context,
and the failure surfaced as a GLSL version error hundreds of lines later.

**A signed-RGTC readback that reported success and wrote zeros.**
`util_format_translate` takes an 8-bit-unorm route whenever *either* side fits
in 8-bit unorm, and that route needs the source's `unpack_rgba_8unorm`. Mesa
leaves that entry a stub for the signed RGTC formats --
`util_format_rgtc1_snorm_unpack_rgba_8unorm` prints its own name and writes
nothing -- and translate still returns true. A `glGetTexImage` of a
`GL_COMPRESSED_SIGNED_RED_RGTC1` texture into `GL_UNSIGNED_BYTE` therefore
produced whatever was already in the caller's buffer, which is why the value
the test reported differed between subgroups. The stub announced itself in the
log twice per run, once per failing case, which is how it was found.
`gex_wgpu_blit_by_transfer` now routes that asymmetric pair through an RGBA32F
intermediate: a source that does not fit 8-bit unorm is signed, wider, or
floating, so float is lossless for it, and the second step clamps into the
destination's range the way GL asks. Six cases, two in each `packed_pixels`
subgroup.

**b2b1 as the identity, which was worse than the abort it replaced.** The
earlier fix for zink's missing `nir_op_b2b1` returned the source unchanged.
That is only right when the source is already a SPIR-V bool; when it was
stored as a float, the float got recorded under a def whose type is bool, and
the next swizzle of it asked for `OpCompositeExtract %bool` from a float
composite. Tint rejects that, so a shader that used to take the whole device
down with an `UNREACHABLE` instead failed to link. A wide NIR boolean is a
0/~0 integer, so the conversion is a comparison against zero, and that is what
it emits now.

**Errors for objects the host never sees.** Sync objects are implemented
entirely in the guest -- identity and state, with no host object behind them
-- so `glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 1)` could return NULL but not
raise `GL_INVALID_VALUE`, and a caller that checks `glGetError` rather than the
handle saw success. The guest now keeps one pending-error slot that
`glGetError` reports before asking the host, and `glFenceSync`,
`glWaitSync`, `glClientWaitSync`, `glGetSynciv` and `glDeleteSync` raise what
the spec says. One slot rather than a queue: GL leaves the order of multiple
recorded errors unspecified, and the first is the one worth keeping.

**One name space, two tables.** Programs and shaders share a name space in GL,
and the difference between the two errors a program-typed query owes is which
table the name is in: `GL_INVALID_OPERATION` for a shader,
`GL_INVALID_VALUE` for a name GL never generated. `glGetUniformfv` looked its
argument up in the programs table alone, got zero, and the host reported the
wrong one. It now searches both and hands the host the real object.

### What the runtime trap actually shows, and what it does not

`framebuffer_blit` (3), `texture_lod_bias` (1) and `texture_size_promotion`
(1) all die the same way, and the only solid evidence about where is the
register state at the fault.

`pc` is `0x4a61eb8`, which is offset `0x61eb8` in `ld-musl-aarch64.so.1` --
its ELF entry point, `_dlstart`. `x0` equals `sp`, `x1` is the `_DYNAMIC`
segment, and `x30` is zero: exactly the state of a freshly entered dynamic
loader. But `x16` still holds libGLESv2's GOT page and `x17` still holds
`ioctl`, left over from the `ioctl@plt` that `flush()` had just made --
registers a genuinely fresh process would not have.

So a process is being entered at the loader's first instruction with a
register file inherited from a caller that was inside the GL transport, and
its first translated block faults. That points at the emulator's exec path,
and it is the one claim here that rests on measurement rather than inference.

**What it does not establish.** The trap is byte-identical on the MobileGL
backend and on the Mesa one, and an earlier version of this section took that
as proof the fault is outside GL. It is not proof of anything of the sort. The
two "backends" differ only in the host-side GL implementation: they share the
guest library (`guest/gl/gexgl.c`), the wire protocol, and the decoder --
`web/mobilegl/gexgl-mobilegl.cpp` is compiled into both. Agreement between them
rules out `gex_wgpu` and st/mesa and nothing else, and MobileGL is not an
oracle in any case; it is earlier code in this same tree, and where the two
agree they may simply be wrong together. A defect in the guest library or in
the decoder would produce exactly this symmetry.

The earlier note about memory pressure was also wrong: the 71/64/128 MiB arena
claims are the loader mapping the 75 MB `glcts` binary and happen on every
family, including the ones that pass.

The open question is therefore narrower than "emulator or GL": what causes a
process entry at that moment, and does anything the guest GL library does to
its own stack or to the syscall it is inside contribute to it. Nothing has
been measured either way yet.

### Gaps WebGPU imposes, not this driver

The two remaining `packed_depth_stencil` cases are `blit`, which loops
`glRenderbufferStorageMultisample` from 0 to `GL_MAX_SAMPLES` and blits
`GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT` between the results. At one
sample the framebuffer is reported `GL_FRAMEBUFFER_UNSUPPORTED`, because
`is_format_supported` refuses every multisample depth format.

Serving it needs a multisample stencil resolve, and WebGPU has none: a resolve
target is colour-only, a shader cannot write stencil, and a multisample
texture cannot be copied. Multisample *depth* is expressible -- the existing
depth-upload pipeline writes `@builtin(frag_depth)` and would extend to
`texture_depth_multisampled_2d` -- but the stencil half has no encoding at
all. `GL_MAX_SAMPLES` stays at 4 because GL 3.2 requires at least that, so the
inconsistency between what is advertised for colour and what is served for
depth is deliberate and recorded here rather than hidden.

**Point size is the other one.** WebGPU rasterizes a point as exactly one
pixel: there is no `pointSize` in a pipeline, and Tint rejects a WGSL vertex
stage that writes any `PointSize` other than constant 1.0, which is why the
driver strips PSIZ stores rather than honouring them. `glPointSize` and a
shader-written `gl_PointSize` are therefore both silently ignored, and
`primitive_restart`'s POINTS sub-test fails on it. Serving it means expanding
each point into a quad before rasterization -- the same compute pre-pass shape
the geometry path already uses -- and would need `gl_PointCoord` alongside.
Nothing here does that yet.

### Two harness defects that were hiding the answers

The census the wrapper prints goes through a real xterm buffer, and a line
longer than the terminal is wrapped there with its tail lost. Every message of
the form "... for input = [X, Y] output = [Z, W]" was therefore being cut
after the half that was already known -- the failing *output* format, the one
piece of information in the line, never survived. Long samples are now emitted
in terminal-sized pieces (`QPA-FULL` / `QPA-CONT`).

`tests/gl44-cts-browser.sh` also takes `GEX_CTS_QUERY` now, a space-separated
list of page flags (`GEX_CTS_QUERY='trapstack=1 checksp=1'`), so a measurement
and a diagnosis are the same command with one variable between them.

### What is left, and what is known about each

* **`packed_depth_stencil`, 2 cases.** Both are `blit`, and both need a
  multisample stencil resolve WebGPU cannot express -- see "A gap WebGPU
  imposes" above.
* **`transform_feedback`, 5 cases.** All five are geometry-shader capture,
  query or discard. `draw_vbo` hands a draw with a geometry shader to
  `gs_draw` and returns before the capture dispatch, so transform feedback
  from the geometry stage is not captured at all. The capture stores would
  have to be emitted into the geometry compute pass, from the geometry
  shader's own `xfb_info`, and decomposed the same way the vertex path now is.
* **`packed_pixels` passes, all 4732 cases.** The signed RGTC readback was
  Mesa's stubbed `unpack_rgba_8unorm`, described above.
* **`framebuffer_blit` (3) and `texture_lod_bias` (1): the gex runtime traps.**
  "linear memory grown to 658 MiB" then a memory access out of bounds after an
  ioctl, with `lr = 0`. This is the emulator, not GL, and it takes the whole
  family before any case reports.
* **`transform_feedback3`, 3 cases.** `getIntegerv(GL_MAX_VERTEX_STREAMS)`
  raises GL_INVALID_ENUM because `stream_output_interleave_buffers` and
  `max_vertex_streams` are unset. Setting them advertises
  ARB_transform_feedback3, which also needs `gl_NextBuffer` and
  `gl_SkipComponents` in the capture layout and geometry output streams the
  geometry path refuses -- so advertising alone would trade three failures for
  three different ones.
* **`buffer_objects` passes.** `gen_buffers` needed the negative count
  forwarded by the guest and the decoder rather than swallowed by the unsigned
  wire type; `multi_texture` needed the element-to-`@location` mapping above.
* **`primitive_restart`, 1 case, and it was three separate things.** It
  overflowed the host wasm stack, and bounding `draw_vbo`'s re-entry at eight
  levels turned that into a named decline: `mode=10`, `MESA_PRIM_LINES_ADJACENCY`.
  `u_index_prim_type_convert` maps every adjacency mode to *itself*, so
  u_primconvert handed the draw straight back unchanged and the topology
  branch took it again, for ever. No hardware driver hits this because the
  hardware drops the adjacent vertices itself; GL 3.2 says that when no
  geometry shader is bound, LINES_ADJACENCY draws a line from vertices 1 and 2
  of every four and TRIANGLES_ADJACENCY a triangle from 0, 2 and 4 of every
  six. `gex_wgpu_draw_adjacency` now does that rewrite and re-issues an
  ordinary indexed draw. The abort is gone and the adjacency draws render.

  What is left is narrower and more interesting: six of the eleven sub-tests
  fail, and they are **exactly the point and line topologies** -- POINTS,
  LINES, LINE_STRIP and the three other line forms -- while every triangle
  topology passes. POINTS is explained: the test sets `glPointSize(POINT_SIZE)`
  and its vertex shader writes `gl_PointSize = 4.0`, and **WebGPU has no point
  size at all**; points are one pixel, which is also why the driver strips
  PSIZ stores (Tint rejects any value but constant 1.0). The lines are *not*
  explained -- the test tolerates a full pixel of deviation on a line spot, so
  a rasterization-rule difference alone should not fail it, and no measurement
  has been made. Do not assume the two halves share a cause.
* **`api` passes.** The guest now keeps its own error slot; see above.
* **`clip_distance.coverage`, 1 case.** "Tint rejected SPIR-V: no matching
  call to `bitcast<f32>(array<f32, 8>)`". zink's `nir_to_spirv` bitcasts a
  whole `gl_ClipDistance` array to a scalar. `compact_arrays` is on in this
  driver's NIR options precisely to keep that array a float array (without it
  the linker packs it into `gl_ClipDistanceMESA`, a `vec4[2]` that ntv
  decorates as `BuiltIn ClipDistance`, which SPIR-V requires to be scalar) --
  so the two halves of the clip-distance handling now disagree, and
  `coverage`, which builds many permutations, finds the seam. `functional` and
  `negative` pass.
* **`texture_size_promotion`, 1 case.** The same runtime trap as
  `framebuffer_blit`; it hangs for the full wall rather than failing in 13
  seconds, which is what makes a full sweep take 25 minutes longer than the
  work in it.
* **`draw_buffers` and `get_uniform_tests` pass.** `draw_buffers` was failing
  on `glGetIntegerv` writing past the caller's buffer -- the guest sized the
  reply from the whole wire payload instead of from the query's own component
  count, the same defect class as the `glGetTexImage` pack-padding clobber,
  and both were writing outside the application's memory rather than merely
  returning wrong values. `get_uniform_tests` needed the `b2b1` emission and
  the shared program/shader name space, both above.
* **`CommonBugs` passes.** The "vec4-backed, 16-byte-aligned Gallium UBO
  loads" refusal was reached only because the depth-stencil state refusal had
  already put the case on a path it should not have taken.

## Immediate next actions

1. `ARB_timer_query`: the `timestamp-query` feature is available, and the
   occlusion query path already has the query-set, resolve and JSPI readback
   shape to copy.
2. `ARB_instanced_arrays` via `arrayStride = 0` with `stepMode: instance`, one
   draw per divisor group -- see the GL 3.3 section for why that works and what
   is already in place for it.
3. Geometry shaders beyond the first shape: points and line strips out, more
   than one input primitive type, and instancing. Each is a change to
   `gex_wgpu_gs_build_module` and its refusal list, not to the architecture.
4. Randomized state/lifetime stress, then the CTS gates. STK stays last.

Beyond that, GL 4.0 wants tessellation, which is the same compute-pass
treatment again (Mesa's `poly_nir_lower_tess.c` is the reference design, with
the same raw-pointer problem), and `fp64`, which WGSL does not have at all.

Capability gaps worth keeping in view, all recorded where the code refuses
them rather than left to be discovered:

- Transform feedback refuses an **indexed or instanced** capture. The
  invocation index has to reach the index buffer and the instance for those,
  which the compute variant does not model yet.
- The geometry path refuses an **indexed or instanced** draw for the same
  reason, and takes triangles in and triangle strips out only.
- The compute-side vertex fetch handles the four formats `u_vbuf` leaves
  alone, and refuses an attribute whose offset or stride is not a multiple of
  four. GL allows byte-aligned attributes, so that is a real, if narrow, gap.
- Vertex attribute divisors above one are refused.

## Cautions

- The worktree is heavily dirty and contains user/incoming work. Do not reset or discard unrelated changes.
- Use `apply_patch` for edits.
- Do not push unless explicitly requested.
- Do not describe the current corpus as proof of OpenGL 4.4 compliance; it is only an early Gallium semantic corpus.
- The full goal remains incomplete and active.

## The blank canvas was never a renderer bug

Three defects stacked on top of each other, each hiding the next. In order of
discovery, with what each one actually cost:

**1. Every unread UBO block demanded constant buffer zero.**
`gl_nir_lower_buffers` opens by setting `driver_location = -1` on every UBO and
SSBO variable, then assigns the program's block index only to the ones it walks
a deref for. `gex_wgpu_lower_gallium_ubos` read `binding` whenever
`driver_location` was `-1`, which is wrong twice over: it resurrects blocks
nothing reads, and `binding` is the GL binding point rather than the block
index. SuperTuxKart's shaders have many unread block members, so each one came
back as a demand for constant buffer zero -- which st/mesa had no reason to
bind -- and `gex_wgpu_draw_vbo` declined the draw.

Fixed by trusting `driver_location` alone once any UBO variable has one, and
sweeping unreferenced blocks with `nir_remove_dead_variables` after the loads
are lowered. Declined-for-constants went from ~690,000 to **zero**; issued
draws from 163,305 to 625,519.

**2. A read-only depth attachment left `depthClearValue` at NaN.**
`WGPU_RENDER_PASS_DEPTH_STENCIL_ATTACHMENT_INIT` leaves it NaN, and the browser
reads it off the dictionary and validates it as a float whatever the load op
says. The read-only branch sets no load op, so it never assigned one, and
`beginRenderPass` threw "The provided float value is non-finite" -- taking the
whole pass, and every draw in it, with it. The depth *upload* path already
carried a comment about this exact trap; the draw path had simply missed it.
The clear path had the same hole in a different shape: `depth < 0.0 || depth >
1.0` lets NaN through, because NaN compares false against both bounds.

**3. Headless Chromium destroyed the WebGPU device on the first present.**
The harness pinned ANGLE to SwiftShader while WebGPU ran on Dawn. No
`SharedImageBackingFactory` can back a texture both of them read, so the first
`wgpuSurfaceConfigure` on the canvas failed inside the GPU process --

    Could not find SharedImageBackingFactory with params: usage: Gles2Read|
      RasterRead|DisplayRead|WebgpuRead|WebgpuSwapChainTexture|RasterWrite|...
    SharedImageStub: Unable to create shared image

-- and Chromium destroyed the device 2 ms later.

**This is the one that made the other two look like the whole story.** A
destroyed device raises no validation error and fails no draw: commands are
accepted and discarded. So after fix (1) the driver was issuing 590,869 draws
per run into a device that had not existed since frame 1, while the frame
counter climbed and the canvas stayed black. Fixed by launching headless with
`--use-angle=vulkan --enable-features=Vulkan` (both -- the backend flag alone
is ignored). Only demos that present to a canvas were ever affected, which is
why `gears` passed throughout.

### What this changes about how to debug here

- A `device.lost` handler is not optional instrumentation. Without one, a dead
  device is indistinguishable from a driver that draws nothing.
- `/tmp/gex-harness/chrome-<port>/chrome.log` carries GPU-process errors that
  never reach the page. Read it before suspecting the driver.
- `web/test/drive.mjs` serves `web/dist`, **not** `web/public`. Deploying a
  rebuilt `gex-wgpu-replay.{js,wasm}` to `web/public/gex` alone leaves the
  browser running the previous build with no sign that it did -- a whole run's
  diagnostics then describe code that is no longer in the tree. Copy to both.
- The run screenshot now clips to the canvas and scrolls it into view. A
  full-page capture at this viewport shows a strip of the canvas's top edge,
  which reads as "still blank" whatever was drawn.

**4. Nine bound vertex buffers against WebGPU's eight slots.**
`pipe_vertex_element::vertex_buffer_index` indexes the application's bound
buffers -- st/mesa binds one per enabled array, and SuperTuxKart binds nine --
and the driver spent that index directly as the WebGPU slot. The ninth latched
the context whether or not anything read it.

Fixed by numbering only the buffers that actually feed an attribute, so the
WebGPU limit lands on eight *used* buffers rather than eight *bound* ones. The
two limits are now separate: `GEX_WGPU_MAX_VERTEX_BUFFERS` is WebGPU's (and the
capture/geometry compute variants' storage bindings), while
`GEX_WGPU_MAX_BOUND_VERTEX_BUFFERS` is how many Gallium slots are tracked. The
same change removes a latent null dereference: the bind loop used to hand every
slot up to `vertex_buffer_count` to `SetVertexBuffer`, including the empty ones
a disabled vertex array leaves behind.

**5. `u_vbuf`'s past-the-end clamp underflowed, and it cost the GPU.**
`u_vbuf_translate_begin` clamps a translate range with

    if (offset + size > width0) { size = width0 - offset; ... }

which only covers a range that *starts inside* the buffer. STK issues a draw
whose first vertex is already past the end -- offset 54868 into a 44-byte
buffer -- and `size` is unsigned, so `44 - 54868` became about 4G. That arrived
at `pipe_buffer_map_range` as a negative `pipe_box` width (`54868+-54824`), and
the `num_vertices` recomputed from it came to 97 million. Translating that much
output exhausted the card: `radv/amdgpu: Not enough memory for command
submission`, then `vkQueueSubmit failed with VK_ERROR_DEVICE_LOST`.

Fixed in `src/gallium/auxiliary/util/u_vbuf.c` by giving a wholly-past-the-end
buffer the zero-filled, zero-stride dummy the missing-resource case above it
already uses. A GL draw that reads past a bound buffer is undefined, not fatal.

### Where it stands now

**SuperTuxKart renders.** snowtuxpeak comes up -- sky, snowfield, the mountain
ridge -- with 268,969 draws issued into a live device and no refusal or device
loss. `gears` still passes; the shared corpus still fails only on the
pre-existing `transform-feedback vertex 1 component 0 = 301, expected 2`.

What is still wrong in the frame: black rectangles where HUD and minimap
elements belong, flat white patches, and a black streak across the mountain
ridge. The remaining declines are all one reason -- 3,352 of "samples a colour
attachment", the GL feedback-loop refusal -- and that count is static after
startup, so it is not what the black rectangles are. Those are the next thing
to chase.

Also open, and unrelated: the context negotiates **GL 3.1**, one requirement
(`EXT_vertex_array_bgra`) short of 3.2, so STK is running its legacy renderer
rather than the modern one. And five of STK's shaders fail to compile at
startup with `0:1(1): error: syntax error, unexpected NEW_IDENTIFIER`.

## Narrowing the remaining STK defects

The frame is right except for black rectangles where HUD and minimap elements
belong, flat white panels, and a black streak across the ridge. Four
explanations have now been ruled out, and the instruments that ruled them out
are in the tree.

**Not declined draws.** The decline count plateaus at 3,352 by frame 601 and
never moves again -- every draw during the race is issued. Those 3,352 are the
menu and loading screen, all one reason ("samples a colour attachment", the GL
feedback-loop refusal).

**Not unwritten textures.** A texture WebGPU zeroed and nothing ever filled
samples as transparent black, which is a black quad and no error anywhere.
`gex_wgpu_resource::texture_written` now records uploads, copies into a
texture, and attachment as a colour or depth target, and the draw path says so
once per texture when a sampler view reads one that was never written. It
reports nothing on STK. (It reported four the first time, all
`Z24_UNORM_S8_UINT` shadow maps -- because only colour attachments were being
marked. Marking the depth attachment too is what made it honest.)

**Not the five shaders that fail to compile.** They are Irrlicht's *GLES2*
shaders: the source begins `precision mediump float;` with no `#version`
directive at all, so it is parsed as GLSL 1.10, where `precision` is not a
keyword -- hence `0:1(1): error: syntax error, unexpected NEW_IDENTIFIER`.
That is invalid on any desktop GL context, not something this backend causes.
A failed compile now prints the first ten numbered lines of its source, since
a GLSL error names a line and column and nothing else.

**Not the vertex element that reads past its buffer.** It was already
non-fatal; only the message still said "draw refused", which it had not done
since that was made non-fatal. Reworded.

## GLX clients cannot reach this backend at all

Running anything besides SuperTuxKart on Mesa turned up a structural gap.
`gex-gl` builds `libEGL.so.1` and `libGLESv2.so.2` and **no `libGL.so.1`**, so
the direct path offers no GLX provider. SuperTuxKart is unaffected because
Irrlicht asks EGL for desktop GL directly -- which is how it negotiates GL 3.1
here with no GLX involved -- but an ordinary GLX client cannot use this path,
and Xwayland, which finds gex-gl first on `LD_LIBRARY_PATH` and then cannot
resolve what libepoxy wants, aborts with SIGABRT inside libepoxy.

Two things that looked like backend bugs and are not:

- glxgears failing in the SuperTuxKart demo with "couldn't get an RGB,
  Double-buffered visual" is that demo's curated rootfs, which has swrast
  dropped: Xwayland reports "could not load software renderer" and brings up no
  GLX. The Wayland demo's rootfs keeps swrast and loads DRISWRAST fine.
- `?gl=` used to force only the *renderer*, leaving the guest's
  `LD_LIBRARY_PATH` and `GALLIUM_DRIVER=virpipe` on the virgl path -- a guest
  that never spoke to the renderer the page had loaded. It is now one decision
  (`usesDirectGl`) covering all four sites.

## GL_DITHER was disabling every blend state

The black rectangles and white panels were not a texture, shader or blend-mode
problem. `gex_wgpu_create_blend_state` ended with

    if (memcmp(state, &supported, sizeof(supported)) != 0)
       return NULL;

as a catch-all for state the driver had not modelled. That caught `dither`.
**GL_DITHER is enabled by default**, so st/mesa set it on essentially every
blend state SuperTuxKart created, every one was refused, and -- because a NULL
CSO is not a declined draw, it just leaves st/mesa binding nothing -- the game
rendered with no blending at all. Its transparent HUD quads came out as opaque
black rectangles, and nothing anywhere reported a problem.

Dithering has no meaning on this backend and none on any modern renderer:
WebGPU has no control for it and Gallium drivers routinely ignore it. It is now
ignored rather than refused, and the catch-all is gone in favour of naming each
unmodelled field (logic op, alpha-to-coverage, alpha-to-one, advanced blend
equations, YUV targets). That also removes a second defect the `memcmp` had:
`pipe_blend_state` is a bitfield struct followed by an array, so comparing a
caller's struct against a zero-initialised one compared **padding** as well,
and a blend state could be refused for bytes no field owns.

Two related fixes went in alongside:

- The accepted blend factors were ONE, ZERO, SRC_ALPHA, INV_SRC_ALPHA and the
  four constant ones. WebGPU also has src, one-minus-src, dst, one-minus-dst,
  dst-alpha, one-minus-dst-alpha and src-alpha-saturated, so anything reading
  the destination was refused for no reason the API imposes. All of them are
  accepted and mapped now. The dual-source four stay out until the device asks
  for `dual-source-blending` and the fragment shader declares `@blend_src`.
- A refusal now says so. Silence was the whole reason this cost so long.

Known-wrong and left alone, with a comment at the mapping: GL's CONSTANT_ALPHA
means "the constant's alpha in all three colour channels", which WebGPU's
`constant` factor -- the whole blend colour, per channel -- does not express.
It is right for the alpha equation and wrong for the colour one.

**Result.** The HUD renders: the race timer is drawn in white, correctly
blended, and every black rectangle and white panel is gone. `gears` still
passes and the shared corpus still fails only on the pre-existing
`transform-feedback vertex 1 component 0 = 301, expected 2`.

Still wrong in the frame: a black streak across the mountain ridge, and the
kart itself is not drawn.

## The remaining defect, characterised but not fixed

A quarter of the frame reads past the end of a vertex buffer: **36,001
occurrences against 130,743 issued draws** in a 70-second run, and always the
same shape.

    vertex element reads past its buffer x36001
      (element 7 reads to 436, buffer slot 7 is 44 bytes)
      offset=36 src_offset=0 stride=44 last=9 size=4 divisor=0
      bind=0x10 draw=24 indexed fmt=62 alloc=44 gallium_off=36 user=0 elements=8

Read that carefully. The vertex elements state has eight elements, each on its
own buffer slot, each with `src_offset = 0` and `src_stride = 44`, and the
slots differ only in `buffer_offset` -- 0, ..., 36. So all eight describe one
44-byte vertex, and the bound resource is 44 bytes: exactly one vertex. The
draw is indexed and its indices reach 9, so it wants ten. Only vertex zero is
in range; WebGPU's robustness returns zeros for the rest.

What has been established:

- The application really does ask for it. `glBufferData(GL_ARRAY_BUFFER, 44,
  NULL, GL_DYNAMIC_DRAW)` appears in the guest's own call stream, and the shim
  passes the size through unchanged.
- It is not a lost re-specification. Per-buffer sizes were logged across
  re-specification (buffer 35 alternates 9520/5760 correctly), and no buffer is
  grown behind the driver's back.
- It is not an instanced attribute read as per-vertex: `divisor=0`, and
  `last=9` is a vertex index, not an instance index.
- It is not a user buffer: `user=0`, so this is a real `pipe_resource`.

What has not been established is why st/mesa binds a one-vertex buffer for a
ten-vertex draw. The two candidates are that the elements state describes a
different (larger) buffer than the one bound -- legal in GL, and garbage to
read -- or that this driver's `last_vertex`, which scans the index buffer's CPU
shadow, is reading a shadow that does not match what the GPU will fetch. The
second would make the whole report a false alarm, so it should be settled
first.

Also still open and unattributed: the kart is not drawn, and a black streak
crosses the mountain ridge. Both are consistent with a position attribute
fetched as zeros -- vertices collapsed onto the origin give exactly one
stretched triangle -- which is what this overrun would produce, but nothing has
tied the two together yet.

### Instruments added this round

- The past-the-end report is counted and periodic rather than one-shot, and
  carries the element format, the divisor, the resource's allocation size,
  Gallium's own record of the binding offset, and the draw shape. A one-shot
  message could not distinguish one stray draw from a quarter of the frame.
- A failed GLSL compile prints the first ten numbered lines of its source.
- `glBufferData` on `GL_ARRAY_BUFFER` reports the first time each buffer name
  is given a size and every time that size changes, capped at 24 lines.
- `?gl=virgl` forces a `directGl` demo back through virglrenderer, so the
  reference renderer can be pointed at the same application, track and camera.
  It does not currently survive a SuperTuxKart run in the headless harness --
  the page reaches about 5.5 GB and is killed -- so the comparison frame is
  still missing.

### The past-the-end read is real, and harmless

Two questions from the previous section, both now settled.

**Is the report trustworthy?** Yes. The check scans the index buffer's CPU
shadow, so it could have been reading something the GPU never sees. It is not:
Gallium's own claim agrees exactly -- `index_bounds_valid` set, `min_index` 0,
`max_index` 9, `index_bias` 0, against the scan's `last=9`. The 44-byte buffer
really is one vertex short of the draw being made from it.

**Does it cause any artifact?** No. Declining those draws as an experiment
changed the frame in exactly one way: the HUD timer disappeared. Those draws
were rendering real content correctly. The attribute that overruns is
`R16G16_SNORM` at offset 36 -- a packed normal or tangent -- which those
shaders do not read, so WebGPU handing it zeros costs nothing. The one
position-sized overrun (`R32G32B32_FLOAT`, element 4) happens once in a run,
not 36,000 times.

So a quarter of SuperTuxKart's draws read past a vertex buffer, Gallium knows
they do, and it does not matter. The experiment is reverted and the finding is
recorded at the check itself, so the next reader does not spend an afternoon on
it the way this one did.

**The black streak and the missing kart are still unattributed**, and the list
of things they are *not* is now: declined draws, unwritten textures, failed
shader compiles, blend state, front-face winding, the GL-to-WebGPU clip-space Z
conversion, and this overrun. What has not been tried is the one thing that
would answer it directly -- a reference frame of the same scene. `?gl=virgl`
exists for that now but the page is killed at about 5.5 GB before the race
starts, so making that run is the next piece of work.

## The black streak is misplaced geometry, not a cull or depth bug

Two forced experiments, reverted after measuring.

**Cull off and depth compare Always**: the streak disappears entirely.
**Cull off, depth compare left alone**: the streak becomes a *much larger* dark
wedge covering the top right of the frame.

Read together those say the opposite of what the first one alone suggests.
Culling was not causing the streak, it was *hiding* most of it -- the streak is
the front-facing sliver of a large dark polygon that is mostly back-facing.
Forcing `Always` did not fix anything either; it let later passes overwrite the
polygon. So there is a big dark primitive being drawn in the wrong place, and
both experiments only changed how much of it survived to be seen.

That also rules the streak out as a consequence of the past-the-end vertex
read: declining every overrunning draw (the previous section) left the streak
exactly where it was.

### A real gap found on the way: current attribute values

`gex_wgpu_default_attribute_buffer` serves any shader input no vertex element
supplies. It holds a fixed `(0, 0, 0, 1)` -- GL's *initial* current attribute
value, not the one `glVertexAttrib*` last set.

st/mesa normally hides this by uploading the current values itself and binding
them, which is why it has not bitten before. But it does not always: this
driver already logs

    vertex element 1 reads its constant value (buffer slot 1 is not bound)

and every location served from the default buffer in that case reads
`(0, 0, 0, 1)` whatever the application set. Anything positional fed that way
lands in the wrong place, which is the shape of defect the streak looks like.
It is now named at the buffer itself. Whether it is *this* application's
problem is unproven -- the message fires once per run, and the streak is
continuous.

### Also established this round

- The frame is identical at 00:2x and at 01:13, so the kart is absent from the
  first rendered frame rather than disappearing during the race.
- STK's loading screen renders correctly -- orange text on black, properly
  blended -- so 2D text and the 2D path are sound.
- The curated rootfs really is snowtuxpeak-only; `--track=hacienda` never
  finishes loading, and the grand-prix definitions name several tracks that are
  not present. Comparing against a differently-coloured track is not available
  without rebuilding the rootfs.

## Where the frame's work actually goes

Two new counters, both kept.

`draws by colour target` groups issued draws by the size of the colour target
they went to, and `presented buffer` counts the ones that landed in the texture
the present path copies to the canvas. A representative race frame:

    draws by colour target -- 640x480:109966 360x360:6 (presented buffer: 4693)

So essentially the whole frame is drawn at full resolution, six draws go to a
360x360 shadow map, and **about four per frame reach the presented buffer** --
the composite and the HUD. That is the expected shape for a renderer that
builds the scene off-screen and composites, and it is worth having because a
draw counter alone cannot distinguish "the scene rendered" from "the skybox
rendered": both look like a hundred thousand draws.

### The composite is not the problem

Presenting the last off-screen colour target instead of the back buffer (an
experiment, reverted) shows the scene target itself: the same white foreground,
the same missing kart, the same black streak, vertically flipped exactly as an
FBO should be relative to the window. So nothing is being lost between the
scene pass and the canvas -- the scene pass draws what we see.

That eliminates the composite, the render targets and the present path, and
localises the remaining defects to the scene pass itself.

### Next hypothesis: fog

The foreground is uniformly light and featureless while the distance is
detailed, which is the shape of a fog factor that saturates near the camera.
The UBO diagnostics from the constant-buffer work named this block directly --
its members are `u_fog_color` and `u_fog_data`, 32 bytes at constant buffer 1 --
so the data is reachable and worth reading back at bind time. A kart fogged to
white on white snow would be invisible without being missing, which fits every
observation so far and has not been tested.

## No scene geometry renders at all

This changes the shape of the problem. Everything before this section was
written as though the scene rendered and the kart was missing from it. It does
not.

**The experiment.** Decline every draw that samples a cube map -- the skybox is
the only thing in this scene that does -- and the frame goes black but for the
HUD and one faint sliver. The sky, the mountains, and the white "ground" that
looked like snow were all the skybox. Meanwhile the issued-draw count barely
moved, because cube-sampling draws are a tiny minority.

So roughly **91,000 draws per interval are issued and produce nothing visible**,
and a handful of skybox draws produce the entire picture.

**What that rules out, on top of the earlier eliminations.** The draws are
issued, not declined. Their colour write masks come from `colormask` correctly.
Their vertex-stage constant buffers hold real data -- a plausible model matrix,
`1,0,0,0 / 0,0,-1,0 / 0,1,0,0 / 235.155,-10.631,0.543,1`, written in full. And
forcing `cullMode = None` with `depthCompare = Always` does not reveal them.

**Fog is not it either.** The 32-byte fragment block at cb1 is `u_fog_color`
and `u_fog_data`, and both read as all zeros -- but `bytes_written` says 32,
so the application uploaded those zeros itself. Fog is simply off, and a black
fog colour cannot whiten anything.

**Where the draws go.** The target histogram now keys on the texture as well as
its size, and a race frame has four distinct 640x480 colour targets:

    640x480@0x1a14ff0:19848  640x480@0x43b3d88:57388
    640x480@0x43b4008:5934   640x480@0x43b3b08:9925
    360x360@...:2  360x360@...:3  360x360@...:1   (presented buffer: 3973)

Presenting the busiest of them (57,388 draws) shows the same skybox and nothing
else. So it is not a composite reading the wrong target: the target that
receives the geometry does not contain it.

### The lead this points at

Three observations that never fitted together now do:

- Turning culling off produced a large dark *wedge* rather than more scene --
  the shape a triangle takes when some of its vertices are at the origin and
  one is not.
- `vertex element N reads its constant value (buffer slot N is not bound)` is
  logged, and the default-attribute buffer that then serves it holds a fixed
  `(0, 0, 0, 1)`.
- Every scene draw is issued with a valid transform and produces nothing.

All three are what a broken attribute-to-`@location` mapping looks like: the
vertex shader reads position from somewhere that supplies `(0,0,0,1)`, every
vertex collapses onto the same clip-space point, and the primitive has no area.
`ctx->vs->vertex_element_location[]` and the `by_element` fallback beside it in
the pipeline setup are where that mapping is decided, and they are the next
thing to read.

## Following the attribute lead: it was not that either

The previous section proposed that the scene's vertex shaders were reading
position from the default-attribute buffer and collapsing every vertex.
Measured, per distinct shader shape rather than per draw:

    vs inputs 0x1f0f supplied 0x1f0f missing 0x0 (elements=9 slots=1)
      vp=0,0 640x480 depth=0.00..1.00 flip=1 scissor=0 fb=640x480
      depthtest=1 func=3 write=1
    vs inputs 0x5f   supplied 0x5f   missing 0x0 (elements=6 slots=6)  ...
    vs inputs 0xf0f  supplied 0xf0d  missing 0x2 (elements=8 slots=7)
      vp=0,0 360x360 ... fb=360x360

Location 0 -- position -- is supplied on every shape. The only shape with a
missing input is the 360x360 shadow pass, missing location 1, and that is not
the scene. So the attribute-to-`@location` mapping is sound and the collapse
hypothesis is dead.

The same measurement clears three more things at once. Viewports are
`0,0 640x480` with depth `0..1`; the scissor is disabled everywhere; and the
scene shapes run `depthtest=1 func=3` (LEQUAL) with depth writes on, against a
buffer the clear path reports as `depth cleared to 1.0000`. None of viewport,
scissor, depth range, depth function or depth clear is wrong.

### Running list of what the invisible draws are not

Declined; missing textures; failed shader compiles; blend state; front-face
winding; the clip-space Z conversion; the past-the-end vertex read; fog; the
composite and the choice of render target; colour write masks; vertex-stage
matrices (a plausible model matrix arrives, fully written); attribute-to-
location mapping; viewport; scissor; depth clear value; depth compare function.

### The strongest remaining lead

The vertex constant buffers that were read back held a *model* matrix. The
view-projection matrix was never checked, and it does not have to live in the
same buffer. If it arrives wrong, every scene vertex transforms off-screen
while the skybox -- which is drawn with one attribute and no depth test, and
conventionally uses its own matrix path -- still lands correctly. That would
produce exactly what is on screen, and it is consistent with every elimination
above, because nothing in the list touches the values inside the transform.

Reading back every vertex-stage constant buffer for one scene draw, rather than
the first sixteen floats of the first one seen, is the next step.

## The matrices are correct too

Dumping every constant buffer the 9-attribute scene vertex shader actually
reads -- it reads exactly one, `cb3`, 584 bytes, `written=584` -- gives four
matrices in sequence and all four are sane:

    [  0]   0.998  0.002 -0.056  0.000     model: rotation
    [ 16]   0.000  0.999  0.033  0.000
    [ 32]   0.056 -0.033  0.998  0.000
    [ 48] 195.801  0.676 -27.367 1.000            + translation
    [ 64]   0.894  0.000  0.000  0.000     projection
    [ 80]   0.000  1.192  0.000  0.000
    [ 96]   0.000  0.000  1.001  1.000
    [112]   0.000  0.000 -1.001  0.000
    [128]   0.998  0.000  0.056  0.000     view (rotation transposed)
    [176] -197.032 0.236  16.303 1.000            + translation
    [192]   1.119  0.000  0.000  0.000     inverse projection
    [208]   0.000  0.839  0.000  0.000

0.894 is 1.192 / (640/480), and 1.119 and 0.839 are the reciprocals of the
first two, so the projection and its inverse agree. The view-projection
hypothesis is dead.

**Worth noting for later**: the projection is *left-handed with a [0, 1] depth
range* -- Irrlicht's `buildProjectionMatrixPerspectiveFovLH`. Working it
through, `clip.z = 1.001 * v.z - 1.001` and `clip.w = v.z`, so `z_ndc` runs 0
at the near plane to 1.001 at infinity: the Direct3D convention, not GL's
[-1, 1]. This driver applies `nir_lower_clip_halfz` unconditionally, which maps
[-1, 1] onto [0, 1]; fed a depth that is already [0, 1] it squashes everything
into [0.5, 1]. That compresses the depth range and wastes half of it, but it
does not make anything invisible, so it is a latent correctness issue rather
than the current defect. A real GL driver has the same input and simply leaves
the near half of the depth buffer unused.

### What that leaves

Every input to the scene draws is now known good: attributes, their locations,
the matrices, the viewport, the scissor, the depth state and clear, the blend
state, the write mask, the render target. The draws are issued and produce
nothing.

That points at the one thing not yet inspected -- the generated shader itself.
The chain is GLSL -> NIR -> SPIR-V -> Tint -> WGSL, and the UBO lowering in the
middle of it is code changed this session: blocks are reshaped to
`vec4[ceil(size/16)]` and every load is decomposed into `offset / 16` for the
row and `(offset & 15) / 4` for the first component. That is correct for
std140, and `cb3` here is the *default uniform block*, which st/mesa may pack
more tightly than std140 depending on `PackedDriverUniformStorage`. A matrix
read from the wrong row would give exactly this: correct data in the buffer,
garbage in the shader, geometry nowhere.

Dumping the generated WGSL for this shader and reading how it computes
`@builtin(position)` is the next step.

## Reading the scene shader

The generated WGSL for the shader that draws most of the frame, dumped in
chunks past the harness's line truncation:

    @vertex fn main(@location(8) i_origin, @location(9) i_rotation,
                    @location(10) i_scale, @location(0) i_position,
                    @location(1) i_normal, @location(3) i_uv,
                    @location(11) i_texture_trans, @location(2) i_color,
                    @location(12) @interpolate(flat) i_misc_data) -> main_out
      x_44  = i_position.xyz * i_scale.xyz
      x_69  = (x_44 + i_origin.xyz) + 2 * (quaternion rotate by i_rotation)
      x_154 = M[16]*x_69.x + M[17]*x_69.y + M[18]*x_69.z + M[19]
      gl_Position = vec4(x_154.x, x_154.y, (x_154.z + x_154.w) * 0.5, x_154.w)

This is SuperTuxKart's object shader. The position depends entirely on
`i_origin`, `i_rotation` and `i_scale`, and the transform comes from
`field0[16..19]` -- bytes 256..320 of the block, not the first matrix in it.

Everything it reads has now been checked and is correct:

- Nine elements, one buffer, 80-byte stride: `i_position` Float32x3 at 0,
  `i_normal` Float32x4 at 12, `i_color` Unorm8x4 at 28, `i_uv` Float16x2 at 32,
  `i_origin` Float32x3 at 36, `i_rotation` Float32x4 at 48, `i_scale`
  Float16x4 at 64, `i_texture_trans` Snorm16x2 at 72, `i_misc_data` Sint16x2
  at 76. Every offset is 4-aligned and the stride is a multiple of 4, so the
  layout is valid WebGPU, and the format table maps each one correctly.
- The values are real: `i_origin` reads (-188.230, 2.000, 393.250) on one draw
  and `i_rotation` (0.000, 0.301, 0.000, -0.954), a unit quaternion.
- The transform at bytes 256..320 is a well-formed MVP -- projection scale
  0.892 and 1.191 combined with rotation, translation (175.010, 0.806,
  -28.395, -27.367).

### A measurement error worth recording

The per-shape dump deduplicated on the input mask and element count. Two
different shaders share those, so the shader responsible for **53,240 of the
frame's draws** was hidden behind one that draws 1,971 times, and the first
round of attribute dumps described the wrong shader. A `draws by vertex shader`
histogram now runs beside the target one, and the dedup keys on the shader
itself. Both shaders turned out to have the same shape, so the conclusions
survived -- but they were luck rather than method.

### A real fix, currently latent

The vertex-buffer slot map keyed only on the Gallium buffer index. One buffer
can feed both per-vertex and per-instance elements, and a
`GPUVertexBufferLayout` carries a single `stepMode` for all its attributes, so
collapsing them into one slot took the step mode from whichever element came
first and fetched the rest with the wrong index. The map is now keyed on
`(buffer, step mode)` and the stride is taken from an element of the slot's own
step mode. SuperTuxKart's draws all use divisor 0, so this changes nothing for
it today; it is a correctness fix for any genuinely instanced draw, and
splitting costs nothing because WebGPU accepts the same buffer bound twice.

## ARB_instanced_arrays was never advertised, and that was the whole thing

    caps->vertex_element_instance_divisor = false;

st/mesa derives `ARB_instanced_arrays` from that one capability. Left false, the
extension was never exposed, so every `glVertexAttribDivisor` an application
made was rejected with GL_INVALID_OPERATION and quietly did nothing.

SuperTuxKart calls it for locations 8 through 12 -- `i_origin`, `i_rotation`,
`i_scale`, `i_texture_trans`, `i_misc_data` -- and then draws with a shader
that reads those as per-instance data. With the divisors dropped they were
fetched *per vertex*, from a buffer that holds no such data. `i_scale` came
back as zero, and since the shader computes

    x_44 = i_position.xyz * i_scale.xyz
    x_69 = (x_44 + i_origin.xyz) + 2 * (rotate by i_rotation)

every object in the scene collapsed onto a point. Ninety thousand draws a frame
rasterised nothing, and the only things ever visible were the skybox and the
HUD.

Turning it on took two supporting changes, both made first:

- The vertex-buffer slot map keys on `(buffer, step rate)`, so one Gallium
  buffer feeding both per-vertex and per-instance elements becomes two WebGPU
  vertex buffer slots bound to the same buffer. A `GPUVertexBufferLayout`
  carries a single `stepMode`, so this is the only way to express it.
- `create_vertex_elements_state` no longer rejects a state whose elements share
  a buffer at different step rates. It still refuses two elements that share a
  buffer *and* a step rate but disagree on stride, because one layout has one
  `arrayStride` -- and it now says so. That refusal used to be the only silent
  `return NULL` in the function, and a NULL vertex-elements state is not a
  refused draw: st/mesa simply leaves the previous state bound, so the draw
  runs with someone else's layout.

A divisor above 1 has no WebGPU equivalent and is still refused, out loud.

**Result.** The scene draws: the kart, the track, the structures on the
horizon, and the minimap. `gears` still passes and the shared corpus still
fails only on the pre-existing `transform-feedback vertex 1 component 0 = 301,
expected 2`.

**What is wrong now** is a different and much better problem: everything
renders as an unlit silhouette. The geometry, depth and composition are right;
the shading is not.

### Two lessons this cost a week of ticks to learn

A capability left `false` is not a neutral default. It removed a GL extension,
which turned an application's setup call into a no-op, which fed a shader
garbage -- and nothing anywhere reported an error, because every layer was
behaving exactly as it was told.

And `create_vertex_elements_state` returning NULL is not a declined draw. Every
other refusal path in this driver either latches the context or counts a
declined draw; this one silently kept stale state, so the symptom appeared
thousands of draws away from the cause.

## After the instancing fix

**The black silhouette was the race-start fade.** The first frame captured
after the fix was at 00:2x and showed the kart and track as flat black shapes,
which read as "geometry renders, shading does not". At 01:11 the frame is
bright and the kart is a light grey shape. SuperTuxKart fades in from black at
the start of a race, and every capture in this document taken before roughly
00:40 is inside that fade. Captures are now taken at `--wall 110`.

**What is actually wrong is exposure.** Measured on the 01:11 frame, 21% of
sampled pixels are pure white in all three channels and 47% sit in the top
sixth of the range. Snow is bright, but not that bright: the image is clipping.

**The geometry pass is not the cause.** For the shader that draws most of the
frame the colour target is `R8G8B8A8_UNORM` with `srgb=0` -- a linear G-buffer,
which is right -- and its fragment shader declares no uniform blocks at all, so
it is writing attributes rather than shading. Whatever blows the image out
happens in the lighting or composite passes that read that G-buffer.

**The `--disable-*` flags are load-bearing.** Running with SuperTuxKart's
default effect settings latches the context at `gex_wgpu_buffer_map` -- not a
refusal of anything, but a `CALLOC` returning NULL. The wasm heap is exhausted.
So the reduced settings in every command line here are not a convenience and
the default path cannot currently be used as a comparison.

## The exposure question, and why it cannot be answered yet

Three explanations for the blown-out frame checked and all three eliminated:

- **Not a missing tone map.** The target histogram now records each target's
  format, and every one of the seven a race frame uses is
  `r8g8b8a8_unorm` -- there is no HDR accumulation buffer anywhere. On GL 3.1
  with these settings SuperTuxKart runs its legacy LDR renderer, so there is no
  tone-mapping pass to be missing.
- **Not a missing sRGB decode.** Every sampled texture and every render target
  reports `srgb=0`. There are no sRGB textures in the frame to decode, so the
  "sampled as linear, reads too bright" mechanism has nothing to act on.
  (`dest_surface_srgb_control` remains deliberately unset for the separate
  reasons recorded above.)
- **Not a fallback texture.** The sampled textures have real sizes and full mip
  chains -- 512x512 with ten levels, 256x256 with nine -- not the 1x1 defaults
  a failed material load would leave.

**And the honest caveat.** Every command line in this document disables
shadows, SSAO, IBL, bloom, glow, light shafts, depth of field and motion blur,
because SuperTuxKart's default settings exhaust the wasm heap. A snow track
rendered with all of those off is *supposed* to look flat and bright. Whether
the current frame is wrong at all is unproven, and the measurement that would
settle it -- 21% of pixels at pure white -- means nothing without something to
compare against.

**Which makes the reference the blocking problem.** `?gl=virgl` exists to run
the same application, track and camera through virglrenderer, and it still
crashes the page at about 5.4 GB some 41 seconds in, during asset loading.
Dropping the window to 320x240 changes neither the timing nor the peak, so the
memory goes on the guest-side Mesa stack that the virgl path needs and not on
anything resolution-dependent. Until that run survives, "is this frame right?"
cannot be answered, and further guesses at the shading are not worth making.

## GL 3.2, and a route to ground truth

The probe programs under `tests/gl44-*.sh` are the only self-checking ground
truth this project has for the Mesa path: each one runs the same source
natively against Mesa first, asserts the expected result, then runs it through
the browser. They all ran on MobileGL, because the query was hard-coded to
`webgpu=1`. They now take `GEX_GL_QUERY`, so

    GEX_GL_QUERY=gl=mesa ./tests/gl44-sampling-matrix.sh

runs the texture/sampler matrix against st/mesa and gex_wgpu. That is the
comparison SuperTuxKart cannot give -- and it does not need the virgl reference
that still will not survive a run.

The first attempt failed immediately:

    FATAL shader_compile log=0:1(10): error: GLSL 4.40 is not supported.
      Supported versions are: 1.10, 1.20, 1.30, 1.40, 1.00 ES, and 3.00 ES

The context was GL 3.1, and the driver had been saying why in every log since
the beginning: `GL 3.2 wants EXT_vertex_array_bgra`. st/mesa gates that
extension on `PIPE_FORMAT_B8G8R8A8_UNORM` being fetchable as a vertex format,
which was simply missing from the driver's table. WebGPU spells the swizzle
into the format itself -- `Unorm8x4BGRA` -- so it is one case:

    case PIPE_FORMAT_B8G8R8A8_UNORM:
       *size = 4; *alignment = 1; return WGPUVertexFormat_Unorm8x4BGRA;

**The context now negotiates GL 3.2.** `gears` still passes, SuperTuxKart still
renders, and the corpus still fails only on the pre-existing transform-feedback
case.

The driver names the next two blockers itself:

    GL 3.3 wants ARB_blend_func_extended
    GL 3.3 wants ARB_timer_query

`ARB_blend_func_extended` is dual-source blending -- the WebGPU feature this
whole investigation opened with. It needs `dual-source-blending` requested at
device creation, `@blend_src` on the fragment outputs, and the four SRC1 blend
factors that `gex_wgpu_create_blend_state` currently leaves out on purpose.
`ARB_timer_query` wants `PIPE_QUERY_TIMESTAMP` and `PIPE_QUERY_TIME_ELAPSED`;
WebGPU's timestamp queries are an optional feature, so this one needs a
decision about whether a CPU-side answer is honest enough to advertise.

Each version rung unlocks more of the probe suite, and the probes are what turn
"does this look right?" into a pass or a fail.

## Dual-source blending, and what is left before GL 3.3

`ARB_blend_func_extended` is now supported, which closes one of the two gaps
the driver named at GL 3.2. Four pieces:

- `web/src/mesa-gl.js` asks for `dual-source-blending` among the optional
  device features. The device grants it -- the features line now reads
  `dualsrc=1` -- which also settles the question this whole investigation
  opened with, back when the browser appeared not to support it at all. It
  did; nothing had asked.
- The screen records it, like every other optional feature.
- `caps->max_dual_source_render_targets` is 1 when it was granted and 0
  otherwise. st/mesa turns that into a fragment shader with two blend sources
  on one colour target, which `nir_lower_fragcolor` above already special-cases
  through `info.fs.color_is_dual_source`.
- `create_blend_state` accepts the four SRC1 factors, but *only* when the
  feature was granted -- a pipeline naming them without it is a validation
  error, and a validation error poisons the device rather than failing the
  call. `gex_wgpu_blend_factor` maps them to `Src1`, `OneMinusSrc1`,
  `Src1Alpha` and `OneMinusSrc1Alpha`.

`gears` still passes, SuperTuxKart still renders, and the corpus still fails
only on the pre-existing transform-feedback case.

### What GL 3.3 still wants

    GL 3.3 wants ARB_timer_query
    GL 3.3 wants ARB_vertex_type_2_10_10_10_rev
    GL 3.3 wants GLSL 330, have 150

**ARB_timer_query** is gated on `caps->query_timestamp`. WebGPU's timestamp
queries are an optional feature this device has not been asked for, and the
alternative -- answering from `os_time_get_nano()` -- measures wall time
between the commands being *recorded*, not the GPU work, because submission
here is asynchronous. Mesa's software drivers do exactly that and get away with
it because they are synchronous. This one is a judgement call about how honest
an approximate answer has to be, and it is deliberately left unmade rather than
quietly claimed; the same rule already keeps `dest_surface_srgb_control` unset.

**ARB_vertex_type_2_10_10_10_rev** needs the packed 10-10-10-2 vertex formats,
including the *signed* ones. WebGPU has only `Unorm10-10-10-2`, and the driver
already refuses `r10g10b10a2_snorm` on purpose so that `u_vbuf` converts it --
which is the right answer for a draw but not enough for the extension gate,
since st asks the driver directly. Claiming it would mean claiming a fetch the
driver cannot do.

So GL 3.3 is not one more small fix away, and the honest next rung is to keep
raising what the probes can reach rather than the version number itself.

## Two more GL programs pass on the Mesa path

Six of the probes carry no `#version` at all, so they compile as GLSL 1.10 and
need nothing above GL 3.2. Two of those have gates, and both now pass against
st/mesa and gex_wgpu:

    GEX_GL_QUERY=gl=mesa ./tests/gl44-state-query.sh
      STATE fixed=1 dynamic=1 formats=10 error=0x0
      SUMMARY cases=3 passed=3 failed=0

    GEX_GL_QUERY=gl=mesa ./tests/gl44-point-parameter.sh
      SUMMARY cases=10 passed=10 failed=0

`GEX_GL_QUERY` now reaches five of the probe scripts, defaulting to the
MobileGL query they had hard-coded, so nothing changes unless it is set. These
are the first results on this path that are checked rather than looked at.

## Where the version ladder actually stops

`timestamp-query` is granted by the device -- the features line reads
`timestamp=1` -- so `ARB_timer_query` can be implemented with real GPU
timestamps rather than the CPU approximation declined above. That removes the
judgement call.

The other requirement does not resolve so tidily.
`ARB_vertex_type_2_10_10_10_rev` is gated on **eight** formats, and
`st_extension_format_mapping::need_at_least_one` is false for that entry, so
all eight must be supported:

    R10G10B10A2_UNORM   B10G10R10A2_UNORM
    R10G10B10A2_SNORM   B10G10R10A2_SNORM
    R10G10B10A2_USCALED B10G10R10A2_USCALED
    R10G10B10A2_SSCALED B10G10R10A2_SSCALED

WebGPU has `unorm10-10-10-2` and nothing else in that family -- no signed form,
no scaled forms. Answering "supported" for the rest would be claiming a fetch
the hardware will not do, and would also stop `u_vbuf` converting them, which
is the only reason those attributes work in draws today.

`docs/gl-ceiling-on-webgpu.md` predicted this and named the way through:
*"direct (`unorm10-10-10-2`) / shader unpack"*. The unpack route is to declare
the attribute as `Uint32` in the WebGPU vertex layout and expand it in the
vertex shader, which means a shader variant keyed on the vertex format -- real
work, but bounded, and the ceiling analysis already argues it is sound.

So **GL 3.2 is the honest ceiling until that variant exists**, and the ladder
above it is a shader-emulation problem rather than a series of small caps.

## Ground truth finds real bugs immediately

Four probes in `tests/integration/programs` had no gate and no reference from
any script, so they had never been run against anything.
`tests/gl44-probe.sh <name>` is a generic gate for them: it builds the source
natively against Mesa to establish the expected `SUMMARY` line, builds it again
for the guest, and requires the browser run to produce the same line. It reads
the expectation out of the native run rather than hard-coding a case count, and
accepts both `SUMMARY` shapes the probes use.

Run against the Mesa path, the four split two and two:

- `msaa-blit` -- **passes**, 8 of 8.
- `clear` and `state` -- do not pass on *native* Mesa either, so there is
  nothing to compare against and they are not a driver result.
- `msaa-depth-blit` -- **failed**, and behind it were two real driver bugs.

**Multisampled depth was refused outright.** `is_format_supported`'s depth
branch returned false for any `sample_count > 1`, so a 4x depth24 texture came
back with `samples=0` and the framebuffer was
GL_FRAMEBUFFER_INCOMPLETE_ATTACHMENT. WebGPU multisamples a depth attachment
exactly as it does a colour one -- 4x guaranteed, written by rendering, read
through `texture_depth_multisampled_2d` -- so depth was the exception for no
reason. Now allowed at 1x and 4x. This also means an MSAA colour framebuffer
can finally have a depth attachment at all, which was impossible before.

**A depth-only framebuffer took its sample count from colour attachment zero.**
`set_framebuffer_state` seeded `samples` at one and then compared the depth
attachment against it, so a framebuffer with depth and no colour -- which GL
allows, and which a depth blit is -- latched the context. The count now comes
from whichever attachment exists.

With both fixed the probe reaches the comparison with no GL error at all, and
fails on values: 0.000000 for all four quadrants where native Mesa reads 0.125,
0.375, 0.625, 0.875.

**Which is the third bug, and it is now named rather than silent.** A WebGPU
resolve is a side effect of ending a render pass whose *colour* attachment
names a `resolveTarget`; there is no depth equivalent and no `depthResolveMode`
in the API. `gex_wgpu_resolve` built a colour attachment regardless and
returned success, so the blit reported no error and wrote nothing. It now
refuses depth and says why. Doing it properly needs a shader that reads
`texture_depth_multisampled_2d` and writes `frag_depth` -- the same shape as
the depth-upload pipeline this driver already builds.

`gears`, the colour `msaa-blit` probe and the corpus are all unaffected.

## Chasing one probe down to four driver bugs

`gl44-msaa-depth-blit-probe` turned out to be sitting on a stack of them. Each
fix moved the failure one step further along, which is what a probe is for.

1. **Multisampled depth was refused by `is_format_supported`.** Fixed earlier;
   without it the framebuffer was never even complete.
2. **A depth-only framebuffer took its sample count from colour attachment
   zero.** Fixed earlier; without it the context latched.
3. **The depth pipeline slot table covered three formats out of five.**
   `gex_wgpu_depth_upload_slot` knew `Depth24PlusStencil8`, `Depth32Float` and
   `Depth32FloatStencil8`, and returned -1 for `Depth16Unorm` and
   `Depth24Plus` -- silently. This probe's destination is `Depth16Unorm`, so
   the lookup failed and took the whole blit down with no explanation. It also
   meant the *depth upload* path had no pipeline for either format, which is a
   latent bug well beyond multisampling. The table now covers all five and the
   pipeline arrays are sized to match.
4. **There was no multisampled depth resolve at all.** WebGPU resolves colour
   and only colour: a resolve is a side effect of ending a pass whose colour
   attachment names a `resolveTarget`, and there is no depth equivalent and no
   `depthResolveMode`. `gex_wgpu_resolve` handed depth to that colour path and
   returned success, so the blit reported no error and wrote nothing.
   `gex_wgpu_resolve_depth` now does it the only way left -- a pass over the
   destination reading sample zero of the source through
   `texture_depth_multisampled_2d` and writing `@builtin(frag_depth)`, built
   from the same template as the existing depth-upload pipeline. GL's depth
   resolve is not an average, and Mesa's own takes a single sample, so sample
   zero is the right choice rather than a shortcut.

The probe now runs the blit with **no GL error at all** (it reported
GL_FRAMEBUFFER_INCOMPLETE_ATTACHMENT, then GL_OUT_OF_MEMORY, then nothing) and
fails only on values: 0.000000 in all four quadrants where native Mesa reads
0.125, 0.375, 0.625 and 0.875. The remaining question is narrow -- whether the
multisampled depth source is written at all, or whether the resolve reads it
wrongly -- and it is the next thing to settle.

Every other gate is unaffected: `gears`, `msaa-blit` (8/8), `state-query`
(3/3), `point-parameter` (10/10).

### Every silent `return false` cost a rebuild

Four of the five diagnostics added while chasing this were on paths that
returned false without a word, and each one hid the next bug behind the
previous one. The slot lookup was the worst: a `switch` with a `default:
return -1` that no caller reported. This driver's rule about naming refusals
exists because of exactly this shape, and it is worth extending to lookups that
can fail, not just to refusals of application state.

### The resolve is right; depth readback of z16_unorm is not

Making the resolve shader write a constant instead of sampling settles which
half is at fault. With `return 0.5;` in place of the `textureLoad`, the
destination still reads **0.000000**, and the driver reports

    depth resolve ran (z16_unorm 16x16, 4 samples -> 1) ok=1

So the resolve pass is created, executed and submitted without error, writes a
known value, and the value does not come back. The multisampled depth resolve
is not the remaining bug: **reading a `z16_unorm` depth texture back is.**

Two details worth carrying into that:

- st/mesa allocated `z16_unorm` for the probe's `GL_DEPTH_COMPONENT24` request,
  so this is not an exotic path -- it is what a plain depth texture becomes
  here.
- `z16_unorm` is deliberately *not* on the `packed_depth` branch of
  `gex_wgpu_texture_map` (only `depth24plus` and `depth24plus-stencil8` are,
  because their depth aspect has no defined layout). It therefore takes the
  ordinary `CopyTextureToBuffer` path with `WGPUTextureAspect_DepthOnly`, and
  that is where the zeros come from. Colour readback through the same function
  is fine -- `msaa-blit` passes 8 of 8 -- so it is specific to the depth
  aspect, the 16-bit format, or the aspect selector.

The experiment is reverted; the `depth resolve ran` line is kept, because a
one-shot confirmation that a path executed is what turned two candidate
explanations into one.

## `msaa-depth-blit` passes: a depth copy must cover the whole subresource

The last of the five bugs behind this probe, and the one that made the previous
four so hard to see.

Changing the copy aspect from `depth-only` to `all` for a depth-only format did
not fix anything, but it made Dawn say what had been wrong all along:

    Copy origin (x:4, y:4) and size (1x1) does not cover the entire
    subresource (origin: [0,0], size: 16x16)

**WebGPU requires a copy touching a depth or stencil aspect to cover the entire
subresource.** st/mesa reads depth a pixel at a time, so every one of those
copies was invalid, the command buffer was invalid with it, and the readback
buffer kept the zeros it had been created with. That is the whole explanation
for a resolve that could demonstrably write a constant into a texture and a
read of that same texture -- same `pipe_resource`, same `WGPUTexture`,
confirmed by pointer -- returning 0.000000 immediately afterwards.

`gex_wgpu_texture_map`'s read path now copies the whole level when the format
has a depth aspect, and takes the requested box out of the staging buffer on
the CPU. Every other format keeps the exact-box copy: the offsets are zero and
the code is the same.

    DEPTH quadrant=0 value=0.125002 expected=0.125000
    DEPTH quadrant=1 value=0.375006 expected=0.375000
    DEPTH quadrant=2 value=0.624994 expected=0.625000
    DEPTH quadrant=3 value=0.874998 expected=0.875000
    SUMMARY cases=1 failed=0

### The Mesa path's gates, as they now stand

    gears                  PASS
    msaa-blit              cases=8  failed=0
    msaa-depth-blit        cases=1  failed=0
    state-query            cases=3  passed=3 failed=0
    point-parameter        cases=10 passed=10 failed=0
    corpus                 the pre-existing transform-feedback failure, unchanged
    SuperTuxKart           renders

One caution recorded rather than hidden: a SuperTuxKart run in the middle of
this work latched at the `CALLOC` in `gex_wgpu_buffer_map`, which is an
out-of-memory rather than a refusal. Two further runs did not reproduce it, and
the same failure appears with SuperTuxKart's default effect settings, so it is
the wasm heap under pressure and not the depth change -- but the full-level
staging buffer a depth read now allocates is worth remembering if it starts
appearing.

## A sixth gate, and the generic runner earning its keep

`gl44-state-probe` passes on the Mesa path too. It needed a small change to
`tests/gl44-probe.sh`: not every probe prints a `SUMMARY` line -- this one ends
with its own marker, `gl44 state query transport ok` -- so when there is no
SUMMARY the gate now takes the last line the native run produced as the
expectation. That keeps it generic instead of needing a pattern per probe.

Two things went wrong while making that change, both worth recording because
they are the same mistake in different clothes:

- The "did not pass" guard tested the expectation for the substring `fail`.
  `SUMMARY cases=8 failed=0` contains it, so the guard rejected every passing
  probe. It now applies only on the fallback branch, where a SUMMARY has not
  already established `failed=0`.
- The native run was captured with `| tee`, which takes stdout only. A probe
  that fails says so on **stderr**, so the log was empty and the gate reported
  "no output" instead of the actual failure. `2>&1` now precedes the pipe, and
  `clear` reports what is really wrong with it:

      native Mesa run did not pass: clear buffer failed:
        glClearBufferfv/readback error

`gl44-clear-probe` fails on native software Mesa, so it is not a driver result
and is not a gate. Whether that is a probe bug or a surfaceless-Mesa limitation
is unexamined.

### Gates on the Mesa path

    gears                  PASS
    clear                  n/a -- fails on native Mesa
    state                  PASS
    msaa-blit              cases=8  failed=0
    msaa-depth-blit        cases=1  failed=0
    state-query            cases=3  passed=3 failed=0
    point-parameter        cases=10 passed=10 failed=0
    corpus                 pre-existing transform-feedback failure, unchanged
    SuperTuxKart           renders

Everything else in `tests/gl44-*` needs GLSL 330 or above, which needs GL 3.3,
which needs the 10-10-10-2 vertex unpack described earlier.

## The corpus is green: the transform-feedback failure was the test's

`FAIL gex-wgpu transform-feedback vertex 1 component 0 = 301.000000, expected
2.000000` has been the one red gate for a long time, and it is not a driver
bug at all.

The captured buffer came back as

    1.0 100.0 200.0 301.0 301.0 301.0 0.0 ...

where three vertices tightly packed should give
`1 100 200 301 | 2 101 200 301 | 3 102 200 301`. Simulating the writes explains
it exactly: with a stride of one float instead of four, the three vertices
overlap, and whichever wrote last wins each slot. Reverse order gives precisely
that buffer.

The stride came from the corpus itself. It set

    xfb->buffers[0].stride = 4;

next to `nir->info.xfb_stride[0] = 4` and `stream_output.stride[0] = 4`, both
of which are in **dwords**. `nir_xfb_info` is not: `nir_gather_xfb_info.c:450`
fills it in as `nir->info.xfb_stride[i] * 4`, so the field is **bytes** and the
value should have been 16. The driver's own comment at the capture lowering
says so and has been right the whole time -- it even records the earlier
mistake of multiplying again there.

Corrected, the whole corpus passes:

    PASS gex-wgpu-browser rc=0

### Every gate on the Mesa path

    gears                  PASS
    state                  PASS
    msaa-blit              PASS
    msaa-depth-blit        PASS
    state-query            PASS
    point-parameter        PASS
    corpus                 PASS
    SuperTuxKart           renders

That is the first time the shared corpus has been green on this backend during
this work, which matters beyond the one case: it is the safety net every other
change here has been measured against, and it has been running with a known
failure in it.

### The intermittent SuperTuxKart allocation failure

`first refusal at gex_wgpu_buffer_map` on the `CALLOC_STRUCT` -- an
out-of-memory, not a refusal of anything -- has now appeared in two runs out of
roughly five, the rest clean. It does not stop the frame: the run that hit it
still issued 235,247 draws and rendered. The same failure is what
SuperTuxKart's *default* effect settings produce every time, so the wasm heap
is close to its limit at these settings and occasionally crosses it.

It is worth a real fix rather than a note, because it makes every SuperTuxKart
measurement in this document a coin flip. The transfer struct is small; what is
large is everything else the guest is holding by that point.

## The out-of-memory, measured: 99.93% of the buffer shadows are never read

Every buffer resource this driver creates carries a CPU shadow of its full
size. A SuperTuxKart race frame now reports what that costs:

    host memory -- 2026159 KiB of buffer shadows in 2170 buffers,
                   2024772 KiB never read

**1.93 GB of wasm heap, of which 1.4 MB is ever read back.** The total climbs
from about 1 MB at ten seconds to its full size and then plateaus, so it is not
a leak: st/mesa legitimately holds those buffers, and the driver faithfully
mirrors every one of them. Against wasm32's 4 GB ceiling and SuperTuxKart's own
footprint, that is what puts a small `CALLOC` over the edge and produces the
intermittent `first refusal at gex_wgpu_buffer_map`.

The shadow has exactly three readers, and the instrumentation marks a buffer
the first time any of them touches it:

- a CPU map that asked for `PIPE_MAP_READ`,
- the index-buffer scan in the draw path,
- an **unaligned** `buffer_subdata`, which needs the bytes either side of its
  range because `wgpuQueueWriteBuffer` takes a 4-byte-aligned offset and size.

An aligned subdata reads nothing: it memcpys into the shadow and then uploads
that same range straight back out, so `data` would have done.

### What the fix looks like

Allocate the shadow on first need rather than at creation, and populate it then
by reading the buffer back from the GPU -- machinery the driver already has for
the `gpu_written` case in `gex_wgpu_buffer_map`. Aligned `buffer_subdata`
uploads straight from the caller's pointer and never allocates one.

That recovers essentially all 1.93 GB. It is a real change to a load-bearing
invariant -- the shadow is currently *authoritative*, and several paths assume
it is always present and always current -- so it wants its own pass rather than
the tail of this one. The measurement is here so the next reader does not have
to rediscover why it is worth doing.

`gears` and the corpus both still pass with the counters in place.

## The memory is pinned by a transfer leak, not by the shadow design

Four measurements narrowed it, each one cheap and each one changing what the
next should be:

    2026159 KiB of buffer shadows in 2170 buffers
    2024772 KiB never read          -- 99.93%
    2025830 KiB ever mapped         -- 99.98%
    1898373 KiB mapped *now*
    live transfers 20605, buffer maps 1273344 unmaps 1252739

The first two suggested the shadow design was simply expensive, and that a lazy
shadow would recover it. The third killed that: nearly all of it belongs to
buffers that are mapped, so a lazily-allocated shadow would still be allocated.
The fourth explains everything -- **20,605 buffer maps are never unmapped**,
and the leaked transfer count matches that gap exactly.

A `gex_wgpu_transfer` holds `pipe_resource_reference(&transfer->base.resource,
resource)`. A leaked transfer therefore pins its buffer, the buffer is never
destroyed, and its full-size shadow is never freed. 1.6% of maps leaking is
enough to hold 1.9 GB, and against wasm32's 4 GB ceiling that is what makes a
small `CALLOC` fail intermittently and produce `first refusal at
gex_wgpu_buffer_map`.

So the redesign described in the previous section is **not** the fix, and
starting it would have recovered nothing. The fix is to find why one map in
sixty never reaches `buffer_unmap`.

What is known about that: the counters are sound -- every one of the five
`gex_wgpu_transfer` free sites decrements, including
`gex_wgpu_texture_unmap`, which frees through a differently-named variable and
was missed on the first attempt (that miss is what made the count look like
20,817 before it was corrected). `buffer_maps` counts only the shadow-returning
path while `buffer_unmaps` counts every unmap, so the gap is if anything
understated.

`gears` and the corpus both still pass with the counters in place.

## A real leak in u_vbuf, and a correction

Bucketing the open maps named the leak precisely:

    open maps by usage -- 0x1:19937        (PIPE_MAP_READ)
    open maps by bind  -- 0x10:19937       (PIPE_BIND_VERTEX_BUFFER)

Read-only maps of vertex buffers, which is `u_vbuf_translate_begin`. It maps
every source buffer it is about to translate, and unmaps them all in a loop at
the end -- but both of its `u_upload_alloc` failure paths did

    if (!out_buffer)
       return PIPE_ERROR_OUT_OF_MEMORY;

*before* that loop, leaking every map the call had taken. An allocation failure
is exactly when memory must not be leaked, and this fed itself: each leaked
transfer holds `pipe_resource_reference` on its buffer, so the buffer is never
destroyed, so the next allocation is that much more likely to fail. Both
returns now `goto` the unmap loop and return the error after it.

    before:  live transfers 20605, buffer maps 1273344 unmaps 1252739
    after:   live transfers 0,     buffer maps 1342564 unmaps 1342564

**The correction.** The previous section concluded that this leak was what
pinned the 1.9 GB of buffer shadows. It was not. With the leak fixed and the
counters exactly balanced, the shadow total is unchanged:

    2026171 KiB of buffer shadows in 2172 buffers ... 0 KiB mapped now in 0 maps

st/mesa genuinely holds those 2,172 buffers. The leak was a real bug and is
worth having fixed, but it was not the cause of the memory, and saying so was
premature.

**What it does change is the design question.** The reason the lazy-shadow idea
looked dead was that 99.98% of the shadow bytes belonged to *ever-mapped*
buffers. With the leak gone, the number that matters is different: **nothing is
mapped at any given instant**. So a write map can hold a transient block for
the duration of the map instead of the buffer holding a full-size shadow for
its lifetime, and that bounds the cost by concurrent maps rather than by total
buffer bytes. The redesign is back on, and now on a measured footing:

- write map -> transient staging block, uploaded and freed at unmap
- read map, index scan, unaligned `buffer_subdata` -> read the range back from
  the GPU, which `gex_wgpu_buffer_map` already does for `gpu_written` buffers
- aligned `buffer_subdata` -> upload straight from the caller's pointer

All gates pass with the fix in: `gears`, corpus, `msaa-blit`,
`msaa-depth-blit`, `state`.

## The lazy shadow: scaffolding in, and the step that actually matters

The shadow is no longer allocated at `resource_create`. `gex_wgpu_shadow_ensure`
allocates one the first time something needs it, and a write-only map of a
plain vertex buffer whose range is 4-aligned is now *staged*: it gets a block
covering exactly the mapped range, uploaded and freed at unmap, with
`buffer_unmap` and `transfer_flush_region` both taught to upload from it.
Index and constant buffers keep the shadow (the draw path reads an index
buffer's shadow to bound its vertex fetch), as do unaligned maps -- of which
there were ten in 1.3 million.

**It recovers nothing yet, and the counters say exactly why:**

    maps staged 0  shadowed 1252744
    shadow causes: subdata 141433  read 0  bind 1979  unaligned 0
                   existing 1250765

`buffer_subdata` still calls `gex_wgpu_shadow_ensure` unconditionally, so by
the time any map arrives the shadow already exists -- 1,250,765 of the maps
found one and the staged path never ran once. The memory is unchanged at
2,026,158 KiB.

The remaining step is therefore in `buffer_subdata`, not in the map path:

- a 4-byte-aligned `buffer_subdata` should upload straight from the caller's
  pointer and allocate nothing (it currently memcpys into the shadow and
  uploads that same range back out, so the shadow contributes nothing);
- an unaligned one still needs the bytes around its range, so it keeps the
  shadow;
- and once subdata stops creating shadows, a **read** map may find none --
  1.19 million maps are `PIPE_MAP_READ`, so that path must populate a shadow
  from the GPU *once per buffer* and keep it, not per map. The machinery
  exists in `gex_wgpu_buffer_map`'s `gpu_written` branch. The reads are
  concentrated: 1.19 million of them touch only 1.4 MB.

Left as scaffolding rather than pushed further in one sitting, because the next
change is the one that can silently corrupt buffer contents, and it wants to
start against green gates -- which these are: corpus, `gears`, `msaa-blit`,
`msaa-depth-blit`, `state` all pass with the change in.

## The runs were locking the machine, and the harness now stops itself

Two boots ended abruptly mid-run rather than crashing: `amdgpu` flooded

    [drm] *ERROR* Not enough memory for command submission!

965 times, and the second boot then wedged a CPU --

    watchdog: BUG: soft lockup - CPU#10 stuck for 414s! [mesa:24824]

with a stack through `drm_fb_helper_damage_work -> drm_atomic_helper_dirtyfb ->
drm_atomic_commit`. This was not the OOM killer: nothing was killed, `tailscaled`
had unbroken uptime across the whole window at a 137 MB peak, so nothing
restarted and the machine simply stopped answering. It needed a physical reset,
which is not reliably available here.

The cause is this driver meeting a 4 GB card that also drives the desktop. Every
GL buffer is mirrored into a `WGPUBuffer` -- a SuperTuxKart race holds about
1.93 GB of shadows (see the section above) -- and moving Chromium's own GL onto
Vulkan put ANGLE on that same GPU. Repeating 110-second runs on a 10-minute loop
did the rest.

Three layers now make this recoverable and, mostly, avoidable.

**The kernel can recover on its own.** `/etc/sysctl.d/99-lockup-recovery.conf`
sets `kernel.softlockup_panic=1`, `kernel.panic=30`, `kernel.panic_on_oops=1`,
so the lockup above reboots after 30s instead of hanging indefinitely; and
`/etc/systemd/system.conf.d/10-watchdog.conf` arms the SP5100 TCO hardware
watchdog at `RuntimeWatchdogSec=60` for the case where the kernel is too wedged
to panic. Verified live: `SP5100 TCO timer 60 active`.

**The harness refuses to be the cause.** `web/test/drive.mjs` reads
`/sys/class/drm/cardN/device/mem_info_vram_used` -- world-readable and exact --
and both refuses to start when the card is already loaded and aborts a running
demo that crosses `GEX_VRAM_BUDGET_MIB` (default 2600 MiB, against 4080 total).
The margin matters: the abort has to land well before amdgpu's own limit,
because past that point nothing can report anything. Both branches are tested
rather than assumed, via `GEX_VRAM_SYSFS` pointing at a file whose value is
changed mid-run; the in-flight abort ends the run with a `VRAM` verdict and
exit 1, the pre-flight one exits 2 without launching a browser.

**Correctness work does not need the GPU at all.** `GEX_WEBGPU_SOFTWARE=1`
moves *both* ANGLE and Dawn to SwiftShader. Both, because mismatching them is
exactly the bug documented above -- no `SharedImageBackingFactory` can back a
texture for two different backends, and Chromium destroys the device silently.
The whole gate suite passes there: corpus, `gears`, and `state`, `msaa-blit`,
`msaa-depth-blit`, `state-query`, `point-parameter`. VRAM stays at its 217 MiB
idle baseline throughout.

It is not a complete substitute. SwiftShader reports `dualsrc=0` and 10 storage
bindings per stage against the discrete GPU's 16, so dual-source blending and
anything near the binding limit still have to run on real hardware -- as does
SuperTuxKart, which is the memory-hungry case the VRAM guard exists for.

### `gl44-probe.sh` defaults to MobileGL

Worth knowing before reading a probe result: the gate runs MobileGL unless
`GEX_GL_QUERY=gl=mesa` is set. Five probes run without it looked like a
software-adapter regression, and `state` "failing" was just MobileGL being
MobileGL -- its own log says `MobileGL DirectWebGPU` in the banner, which is the
thing to check first when a probe result is surprising.

## MobileGL is now refused, not just labelled

The banner at the top of `web/src/mobilegl.js` and the "(broken)" dropdown label
were documentation, and documentation stopped nothing: every `gl44-*` gate
passed `webgpu=1` by default, which selected MobileGL, and five probe runs went
through it before the banner in the log gave it away. One of them was reported
as a Mesa regression before the backend line was checked. Comments do not
enforce; these do.

**The selection rule is one function.** `MobileGlDevice.requested(query)` in
`web/src/mobilegl.js` is the single definition of "this run asked for MobileGL",
and it accepts both live selectors: `webgpu=1` (the historical one the gates
used) and `gl=mobilegl` (the one that reads like it should work). They used to
mean different things -- `gl=mobilegl` matched neither branch in `demos.js` and
fell through to plain WebGL, so a run that asked for MobileGL got something else
again. Anything that tests for MobileGL now calls this.

**Three gates, in order of how cheaply they fail.**

1. `web/test/drive.mjs` refuses before launching a browser: `--query webgpu=1`
   or `gl=mobilegl` exits 2 with an explanation. A second, not a browser launch
   and a log read carefully enough to notice which backend ran.
2. `createDirectGlDevice` and the `directWebGpu` branch in `web/src/demos.js`
   now fall through to **Mesa**, not MobileGL. This is the inversion that
   matters: the broken backend used to be what you got by default.
3. `MobileGlDevice.create` throws unless unlocked, which catches direct callers
   -- `mobilegl-worker.js` reaches it without passing through `demos.js` at all.

**There is no escape hatch, by design.** No flag, token, query parameter or
environment variable re-enables MobileGL. An override would only ever be
reached by mistake, or by a future agent talking itself into running it "just
as a control" -- and it is not a valid control: it renders nothing useful, so a
comparison against it is worse than no comparison. The page ignores a stale
`?webgpu=1` and runs Mesa with a console warning rather than honouring it.
Re-enabling MobileGL means deleting the throw in `MobileGlDevice.create`, which
is a deliberate act that shows up in a diff.

**Every gate default was flipped to `gl=mesa`** -- ten `gl44-*` scripts,
`gl44-cts-browser.sh` (`GEX_CTS_BACKEND` now defaults to `mesa`), and
`stk-webgpu-fps.sh`, which defaulted to MobileGL underneath a comment saying
MobileGL does not render SuperTuxKart.

Verified: both `webgpu=1` and `gl=mobilegl` exit 2 refused, as does the token
that briefly existed during development; the default run reports
`Mesa st/mesa + gex_wgpu on WebGPU`. The `state` probe, which "failed" only
because it had been running MobileGL, passes on Mesa. `mobilegl-worker.js` is
dead code kept so the module still builds.

## Two corpus expectations sat on a rounding tie

With the gates defaulting to Mesa, the corpus began failing on hardware while
passing on SwiftShader -- deterministically, one channel, off by one:

    FAIL gex-wgpu blend-src-alpha-over pixel 0,0=159,64,96,255   (expected 160)
    FAIL gex-wgpu blend-additive-one-one pixel 0,0=127,127,127,127 (expected 128)

Both land exactly on an 8-bit rounding tie. The clear is `{0.25,0.5,0.75}`,
stored as `{64,128,191}`:

    R = 0.5 + (64/255)*0.5   -> 159.5
    B =       (191/255)*0.5  ->  95.5
    additive: 51/255 + 0.3    -> 127.5 on every channel

In float32 those ties do not all fall the same way, and which way a tie goes is
a property of the GPU: SwiftShader rounds one way, the RX 6500 XT the other.
Both are inside the 1 ULP blending is specified to.

`blend-reverse-subtract` and `blend-constant-color` in the same block had
allowed a tolerance of 1 all along, evidently for this reason -- the two failing
cases were the outliers, so giving them `check_uniform_tolerance(..., 1, ...)`
makes the blend checks consistent rather than loosening the suite. The other 16
expectations remain exact. Corpus is green on hardware again.

### Gate status on the discrete GPU

corpus PASS, `gears` PASS, `state` / `msaa-blit` / `msaa-depth-blit` /
`state-query` / `point-parameter` all PASS, and `stk-webgpu-fps` PASS at
60.1 FPS with 100% fresh presents and 0 rAF misses. A single STK run peaks
around 1350 MiB of VRAM against the 2600 MiB budget; it is accumulation across
repeated runs, not one run, that the guard is there to catch.

## The whole probe corpus, and what actually blocks it

Only five probes had ever been run through a gate. There are eighteen, and
`tests/gl44-probe.sh` is generic, so all of them were swept on Mesa:

- **5 pass**: `state`, `msaa-blit`, `msaa-depth-blit`, `state-query`,
  `point-parameter`.
- **4 fail before the browser is involved**: `clear`, `compute`, `indexed`,
  `indirect` fail in the *native* Mesa run that establishes the expectation
  (`native Mesa run did not pass: ...`). These say nothing about gex_wgpu; the
  probe or software Mesa is at fault and the gate cannot form an expectation.
- **9 fail in the browser** -- and every one of them is the same blocker.

That last point is the finding. They looked like nine bugs and are one:

| probe | needs |
|---|---|
| `multi-draw`, `uniform` | GLSL 330 -> GL 3.3 |
| `reflection` | GLSL 430 |
| `fragment-state-matrix`, `integer-texture`, `sampling-matrix`, `transform-feedback`, `vertex-fetch-matrix` | GLSL 440 |
| `texture-sampler` | `glBindSamplers` -- ARB_multi_bind, GL 4.4 |

`texture-sampler` is worth spelling out because it does not announce itself as
a version problem: it reports `multi-bind returns guest sampler name ...
error=0x500`, and the wire decode for both `glBindSamplers` (opcode 1045) and
`glGetIntegeri_v` (1207) is correct. `glBindSamplers` is ARB_multi_bind, which
a GL 3.2 context does not have, so the bind silently does nothing and the query
correctly returns 0.

### The ceiling, measured

`tests/mesa-wgpu-version.sh` reports it directly. As found:

    context is 3.2 (Core Profile) / GLSL 1.50
    GL 3.3 wants ARB_blend_func_extended
    GL 3.3 wants ARB_timer_query
    GL 3.3 wants ARB_vertex_type_2_10_10_10_rev
    GL 3.3 wants GLSL 330, have 150

Two of those are now gone.

**ARB_timer_query** is `caps.query_timestamp`, and it is now set. The queries
are timed on the CPU with `os_time_get_nano()`, plus
`screen->get_timestamp = u_default_get_timestamp` -- exactly what softpipe
does, which is the precedent for a driver with no GPU timer. `create_query`
accepts `PIPE_QUERY_TIMESTAMP`, `TIME_ELAPSED` and `TIMESTAMP_DISJOINT`; a
timer query deliberately does *not* claim `ctx->active_query`, since GL allows
one to run concurrently with an occlusion query and claiming the slot would
make each refuse the other. `glQueryCounter(GL_TIMESTAMP)` reaches `end_query`
with no `begin_query`, so `end` never assumes `start` was set.

It measures when commands were *recorded*, not when the GPU ran them, so it
under-reports. WebGPU does have a real `timestamp-query` feature -- the driver
already probes it into `screen->timestamp_query` -- and the GPU-accurate
version attaches `timestampWrites` to each pass and resolves the query set the
way the occlusion path does. Worth doing if anything ever profiles against
these numbers; see [[gex-wgpu-instrument-traps]] for why an instrument that
quietly lies is expensive here.

**ARB_blend_func_extended was never actually missing.** It was already
implemented; `web/public/mesa-wgpu-replay.html` had drifted from
`web/src/mesa-gl.js` and did not request `dual-source-blending` (or
`timestamp-query`), so on the replay path the driver saw no dual-source
feature, reported `max_dual_source_render_targets = 0`, and the version script
under-reported the ceiling the real application path reaches. The two lists are
now identical and must stay that way.

### What is left: one blocker

    GL 3.3 wants ARB_vertex_type_2_10_10_10_rev
    GL 3.3 wants GLSL 330, have 150

GLSL 330 follows automatically once the extension list is complete, so this is
a single item. st/mesa requires **all eight** formats -- R/B x
UNORM/SNORM/USCALED/SSCALED -- and WebGPU has exactly one,
`WGPUVertexFormat_Unorm10_10_10_2`, which covers `R10G10B10A2_UNORM`.

`u_vbuf` cannot help: the 10-10-10-2 formats are absent from
`vbuf_format_fallbacks`, so they are passed through to the driver untranslated
rather than converted on the CPU. And CPU translation would not expose the
extension anyway -- `st_extensions.c` asks `screen->is_format_supported`
directly, so a format the driver declines is a format GL does not get.

That leaves shader unpack, which is what `v3d` does: it whitelists all eight in
`is_format_supported` (`v3d_screen.c:481`) and converts in its shader compiler.
Here that means binding the attribute as a raw `Uint32` and unpacking in the
vertex shader, with `B10G10R10A2` additionally swizzled -- which requires
keying vertex shader compilation on the vertex element state. That variant
mechanism is the actual work, and it is the last thing between this driver and
GL 3.3.

### The STK gate is flaky on a second consecutive run

`stk-webgpu-fps.sh` holds tight thresholds (>= 59.8 FPS, >= 99.5% fresh, <= 1
rAF miss). From a clean GPU it passes at 60.10 FPS / 100% fresh / 0 misses. Run
immediately again it drops to ~59.1-59.9 with misses, because `drive.mjs`
reuses the browser between runs and the previous run's VRAM -- around 1 GB --
is never released. This is the same accumulation behind the lockups above.
Benchmark runs want a fresh browser (`--restart-chrome`, or close it first);
a second-run number is not comparable to a first-run one.

## GL 3.3, and the packed vertex formats that were blocking it

The context now reports **3.3 (Core Profile) / GLSL 3.30**, up from 3.2/1.50.
`tests/mesa-wgpu-version.sh core` says "at or above the highest rung listed".

`ARB_vertex_type_2_10_10_10_rev` was the last extension, and it is all-or-
nothing: `st_extensions.c` wants all eight of R/B x UNORM/SNORM/USCALED/SSCALED
before it exposes anything, and WebGPU fetches exactly one of them
(`Unorm10_10_10_2`, which covers `R10G10B10A2_UNORM`).

Nothing upstream could cover the gap. `is_format_supported(...,
PIPE_BIND_VERTEX_BUFFER)` has exactly **one** consumer in st/mesa -- the
extension check itself -- and the 10-10-10-2 formats are absent from
`u_vbuf`'s `vbuf_format_fallbacks`, so `format_translation` leaves them alone
and the element is marked compatible and passed straight to the driver. The
driver comment claiming u_vbuf "converts it to something expressible" was
wrong about these formats. What actually happened before was simpler: without
the extension, GL rejected `GL_INT_2_10_10_10_REV` outright, so nothing ever
asked for one.

That is also why this change carries real risk: exposing the extension makes
applications *start* using these formats. SuperTuxKart packs its normals as
`r10g10b10a2_snorm`.

**How it works.** The seven non-native formats are fetched as
`WGPUVertexFormat_Uint32` -- one raw dword -- and unpacked in the vertex
shader, which is what `v3d` does (`v3d_screen.c:481`). `gex_wgpu_vertex_unpack_kind`
maps a format to a conversion plus a BGR flag;
`gex_wgpu_lower_vertex_attrib_unpack` rewrites the shader.

The inputs are still variables and derefs at that point rather than
`load_input` intrinsics, so three things have to change together: the
variable's type, the deref's type, and the load's component count and bit
size. Leaving any one of them at vec4 makes ntv emit a module whose declared
`@location` type disagrees with the pipeline's vertex format -- a WebGPU
validation error, which poisons the device for every later command.

The module now depends on the bound vertex elements, so it needs a variant.
`gex_wgpu_interp_variant_get` already existed for the interpolation and
viewport-folding variants and already builds both y-flip forms, so the unpack
signature joins its key. One consequence worth noting: when an unpack is
needed, the un-varied `plain` module is no longer a legal fallback -- it would
read the raw dword as if it were the finished value -- so the failure paths
return NULL rather than `plain`.

`caps->glsl_feature_level` then goes 150 -> 330. The note above it asked that
the level be "derived from the families that actually pass rather than chosen
to make a context appear", and it is: what 330 adds over 1.50 is almost
entirely front-end (explicit locations, bit-encoding and pack/unpack builtins)
which the GLSL compiler and NIR handle, and the one part needing the driver is
the vertex type above.

### Verified, not assumed

`tests/integration/programs/gl44-vertex-2101010-probe.c` is new, because
neither the corpus nor SuperTuxKart exercises these formats in a checkable way
-- STK passing proves only that nothing broke. It draws one flat quad per
format family whose colour *is* the fetched attribute and reads the middle
pixel back, and it is built to make a wrong unpack loud rather than subtle:

- the signed cases put a negative in green, so a missing sign extension reads
  as 255 rather than 0 -- not a rounding difference;
- the BGRA case expects `255,0,127`, where ignoring the swizzle gives
  `127,0,255`.

All five families return byte-identical results on native Mesa and on
gex_wgpu.

Two probes that had never compiled now pass on their own merits: `multi-draw`
and `uniform`, both of which refuse anything below GLSL 330. Eight of the
eighteen probes now pass, against five before. Unchanged by the bump: corpus
PASS, `state`, `msaa-blit`, `msaa-depth-blit`, `state-query`,
`point-parameter`, and SuperTuxKart at 60.10 FPS with 100% fresh presents and
0 rAF misses -- the same numbers as before the change, from an idle GPU.

## The four probes that had never run, and the two bugs they were hiding

`clear`, `compute`, `indexed` and `indirect` failed in the *native* Mesa run
that establishes the gate's expectation, so they had never been compared
against anything on either path. Three of the four shared one cause.

**`eglChooseConfig(display, NULL, ...)`.** A NULL attribute list defaults to
requiring `EGL_SURFACE_TYPE=EGL_WINDOW_BIT`, which matches nothing under
`EGL_PLATFORM=surfaceless`. It returned **zero configs**; none of the probes
checked `config_count`, so `config` stayed NULL, `eglCreatePbufferSurface`
failed with `EGL_BAD_CONFIG` -- and `eglMakeCurrent` then *succeeded* anyway,
because a surfaceless context may be made current with no surface. The result
was a context with no default framebuffer, so every clear and readback
returned `GL_INVALID_FRAMEBUFFER_OPERATION` (0x506). These probes could not
have passed on any driver. All four now ask for `EGL_PBUFFER_BIT` explicitly
and check the count.

**A second trap in the same area**: Mesa gives a *single-buffered* pbuffer a
`GL_BACK` draw buffer and a `GL_FRONT` read buffer, so clears and readbacks
addressed different buffers and every pixel came back zero even after the
config was fixed. `gl44-clear-probe` now renders to an FBO, which has one
unambiguous attachment and matches what the rest of the probes here do.

**`gl44-indexed` was a gate bug, not a probe bug.** The probe asserts that GL
object names come from one global space -- true in the guest, false on native
Mesa, which has a namespace per object type -- and it already guards that check
with `GEX_NATIVE_REFERENCE`. `tests/gl44-probe.sh` never set it, so the
reference run failed on a check that was never meant to apply to it. The gate
now sets it.

### Two real gaps, previously invisible

With the probes able to run, two genuine gex_wgpu failures surfaced. Both are
reported honestly as red gates rather than papered over.

**Integer render targets returned zero -- fixed.**
`gex_wgpu_clear_render_target` read `color->f[]` unconditionally, but
`pipe_color_union` is a union and st/mesa fills `i`/`ui` for a pure-integer
target. An integer clear was therefore reinterpreting the integer's bit pattern
as a float: `glClearBufferiv` with 1 became the denormal 1.4e-45, which rounds
to zero, so every integer clear produced `0,0,0,0`. It now picks the arm with
`util_format_is_pure_sint/uint`, and `glClearBufferiv`/`uiv` return `1,0,1,1`
and `0,1,1,1`, matching native Mesa.

The probe's old expectations here were themselves unsound -- it aimed integer
clears at the normalized `RGBA8` attachment, where the spec makes the result
undefined, and the expected values were guesses at that undefined behaviour.
Each clear now targets a buffer of its own type, which is what makes the answer
defined at all.

This fix was briefly and wrongly written off, which is worth recording because
the cause was environmental rather than technical -- see the staleness note
below.

**`ARB_draw_indirect` is not exposed.** `gl44-indirect` passes natively and
returns `GL_INVALID_ENUM` here. The guest marshalling and the host decode
(opcode 1129) are both correct; the driver simply never sets
`caps->draw_indirect`, so host Mesa refuses the entry point. WebGPU has native
indirect draw support, and an extension may be exposed below its core version,
so this does not have to wait for GL 4.0 -- it needs
`pipe_draw_indirect_info` handled in the draw path.

`compute` remains failing in its native run at an SSBO readback, and
`indexed` now gets much further and stops at `glMultiDrawElements` with a
zero-count first sub-draw, which native Mesa itself renders black. Neither is
a gex_wgpu result yet.

### `gl44-probe.sh` was testing a stale driver

`?gl=mesa` loads the host Mesa module from `/gex/gex-wgpu-replay.*`, and the
probe gate copied only the *guest* probe binary into place. Whatever host
driver some other script had last staged is what actually ran. So a driver fix
could be built, run, and appear to change nothing.

That is exactly what happened above: the integer-clear fix was built, the probe
still failed, and diagnostic traces added to the driver produced no output at
all -- including one in `set_framebuffer_state`, which cannot go uncalled in a
run that renders. Traces that are *missing* rather than wrong are the tell; the
module under test was 20 minutes older than the build. With the freshly built
module staged, the probe passes.

The gate now stages `gex-wgpu-replay.js`/`.wasm` itself when a build is
present, and warns when there is none rather than silently testing whatever is
lying around.

### Where the probes stand

Nine now pass -- `clear`, `state`, `msaa-blit`, `msaa-depth-blit`,
`state-query`, `point-parameter`, `multi-draw`, `uniform`, `vertex-2101010` --
against five before this work. Corpus PASS. SuperTuxKart 60.0 FPS, 100% fresh,
from an idle GPU.

## ARB_draw_indirect, and three refusals that were not refusals

`gl44-indirect` passes. Getting there needed one feature and three fixes to
draw refusals that were rejecting legal GL.

**The feature.** `caps->draw_indirect` is set, and an indirect draw is resolved
on the CPU by `util_draw_indirect` -- an official Mesa helper that maps the
argument buffer, extracts each command and re-issues an ordinary draw, which
lands back in `draw_vbo` with `indirect` NULL. That re-entry is what
`ctx->draw_depth` bounds, and it is one of the two levels its comment allows
for. `PIPE_BIND_COMMAND_ARGS_BUFFER` joins the allowed buffer binds (the name
is not `PIPE_BIND_DRAW_INDIRECT`); no `WGPUBufferUsage_Indirect` is needed
because nothing is handed to WebGPU. `count_from_stream_output` is a different
feature that the helper asserts against, so it stays refused.

WebGPU could take these natively -- `drawIndirect` and `drawIndexedIndirect`
expect exactly GL's `DrawArraysIndirectCommand` and
`DrawElementsIndirectCommand` layouts, field for field -- which would remove
the readback. It also needs the buffer to carry `WGPUBufferUsage_Indirect`, and
a non-zero `baseInstance` needs the `indirect-first-instance` feature, so the
CPU form went in first.

**A zero-count draw is legal and draws nothing.** `count == 0` and
`instance_count == 0` were both refusals, and a refusal latches the context --
so one empty sub-draw poisoned everything after it. `glMultiDraw*` routinely
carries a zero-count entry, and this is what turned a whole
`glMultiDrawArraysIndirect` into `GL_OUT_OF_MEMORY`. They are now early
returns.

**A non-zero `drawid_offset` only matters if the shader reads `gl_DrawID`.**
`util_draw_indirect` gives each sub-draw of a multi-draw its own index there,
so refusing every non-zero offset refused multi-draw indirect outright. The
shader's `info.system_values_read` says whether `SYSTEM_VALUE_DRAW_ID` is read;
only then is the offset refused.

### A regression I reported that was not there

Enabling the cap appeared to cost SuperTuxKart real frame rate -- 57.0-59.1 FPS
against a 60.1 baseline -- and disabling it appeared to restore 60.10/100%
exactly. That was two single samples either side of noise.

Counting the calls settled it: **SuperTuxKart issues zero indirect draws**, so
the cap cannot affect it at runtime. Repeated runs with the cap enabled give
58.5, 58.7, 60.0 and 60.1 FPS, and VRAM before the run does not explain the
spread (the 525 MiB run was the fastest of them). The STK gate's thresholds
(>= 59.8 FPS, >= 99.5% fresh, <= 1 miss) sit inside that spread, so **a single
STK run is not a regression signal** -- it takes several, and a clean/dirty GPU
comparison is not enough on its own.

Ten probes now pass: `clear`, `indirect`, `state`, `msaa-blit`,
`msaa-depth-blit`, `state-query`, `point-parameter`, `multi-draw`, `uniform`,
`vertex-2101010`. Corpus PASS.

`gl44-indexed` is still not gatable, and the reason is on the native side: its
`glMultiDrawElements` case passes counts `{0, 3}`, and llvmpipe renders nothing
for it. Changing the counts to `{3, 3}` makes the same probe pass natively, so
the zero-count first entry is the trigger -- a native Mesa behaviour, not a
gex_wgpu one.

## The first conformance landscape on the Mesa path

With the context at GL 3.3, `KHR-GL33` is now a module the backend can actually
be measured against -- the sweep's own note warns that running KHR-GL44 against
a 3.2 context measures context creation, not conformance. Run as:

    GEX_CTS_BACKEND=mesa GEX_CTS_MODULE=KHR-GL33 \
    GEX_CTS_MUSTPASS=.../khronos_mustpass/main/gl33-main.txt \
    GEX_CTS_SWEEP_OUT=web/.work/gl33-sweep-mesa tests/gl44-cts-sweep.sh [family...]

needing a persistent browser (`drive.mjs ... --keep`). What the families do:

| family | result |
|---|---|
| `shaders30` | **651 / 651** |
| `texture_repeat_mode` | **162 / 162** |
| `texture_swizzle` | 2 / 19 |
| `transform_feedback` | 15 / 21 |
| `packed_depth_stencil` | 21 / 25 |
| `info`, `buffer_objects` | 6 / 6, 5 / 5 |
| `pixelstoragemodes` | 0 / 896 |
| `cull_distance` | 0 / 218 |
| `ext_texture_shadow_lod` | 0 / 16 |
| `texture_filter_minmax_tests` | 0 / 5 |

`shaders30` passing whole is the real confirmation that the GLSL 330 bump was
sound rather than merely accepted. The three zero families at the bottom are
extensions above GL 3.3 (`ARB_cull_distance` is 4.5), so they are expected to
fail and are not defects.

`packed_pixels` (4732 cases) and `shaders` (2397) have not been swept yet.

### `texture_swizzle`: vertex-stage integer sampling

Not a swizzle bug at all. The failures are `Failed to link program`, and
underneath:

    Tint rejected SPIR-V: error: sampled image must have float component type

A shader sampling an integer texture (`isampler2D`/`usampler2D`) produces
SPIR-V that Tint will not take. The driver already knows about this: it detects
`integer_sampled_slots` and, for a **fragment** shader, defers the module
entirely and builds it at draw time through
`gex_wgpu_lower_texture_variant`, which has the sampler state and views it
needs to lower the sampling away.

The vertex stage has no such path, so it compiles eagerly and Tint rejects it.
The rejection now names the stage -- `vertex shader rejected (int_sampled=0x1)`
-- because a shader rejection reaches the application as a link error with no
indication of which stage was at fault, and the two stages fail here for
different reasons.

Fixing it means giving the vertex stage the same draw-time treatment. The
lowering is genuinely state-dependent -- it reads sampler filters, wrap modes
and view swizzles -- so this is not a matter of running it eagerly; the vertex
variant key in `gex_wgpu_interp_variant_get` would have to grow sampler state
alongside the interpolation, viewport and attribute-unpack keys it already
carries.

### `pixelstoragemodes`: 896 plain mismatches

No refusals, no rejected shaders, no traps -- 736 recorded `Fail (Fail)` with
the rest of the family never reached. This family varies `glPixelStorei` pack
and unpack parameters (row length, image height, skip pixels/rows, alignment)
across texture upload and readback, so a uniform failure points at those
parameters being ignored somewhere on the path rather than at 896 separate
bugs. It is the single largest concentration of failures found so far and has
not been investigated.

## `pixelstoragemodes` is not what it looks like, and a hang underneath it

The 896 failures do not come from the pixel-store modes. Reducing the family to
its simplest case -- `teximage2d.r8.0_0`, the default store state and the
plainest format the family has -- gives
`tests/integration/programs/gl44-pixelstore-probe.c`, which uploads an R8
texture and reads every texel back through `texelFetch` exactly as the CTS
does. On gex_wgpu all three of its cases return byte-identical results to
native Mesa:

- default state (`GL_UNPACK_ALIGNMENT` 4, a 4-byte row that needs no padding);
- `GL_UNPACK_ALIGNMENT` 1, which must not change the answer;
- `GL_UNPACK_ROW_LENGTH` 8, uploading the first 4 bytes of each 8-byte row.

So R8 upload, the unpack parameters and `texelFetch` all work, and whatever the
CTS family is failing on is further in -- more formats (`r8snorm` and the
integer ones), 3D and array targets, compressed uploads, or the shape of its
own comparison. The reduction did not reproduce it, which is a real result:
896 failures are *not* explained by the simple path.

**What the reduction did find is a hang, in the first draw.** (An earlier
version of this note said the probe passed all three cases and then hung at
process exit. That was wrong, and the mistake is worth recording: the gate
prints the *native* reference run's output before it starts the browser, and
the `STEP`/`CASE`/`SUMMARY` lines being read as the browser's results were the
native run's. The browser produced no guest output at all. Split the log at
the `goal: --want` line before attributing anything to a side.)

The host wedges inside `glDrawArrays`. Tracing every opcode the host decoder
handled showed the last one to arrive is 112 -- `DRAW_ARRAYS` -- after which no
driver diagnostic of any kind appears and the guest sits in the `ioctl` inside
`flush()` until the runtime gives up:

    the gex runtime trapped inside the shim (the GL device did not answer in
    thirty seconds, ... inside syscall 29)

with `lr` in `libGLESv2.so.2` -- that is `flush`, the guest's one ioctl. The
gate reports `TIMEOUT`.

What it is not: not the render-target size (4x4 and 8x8 wedge identically), not
the repeated `glTexImage2D` re-specification (one case is enough), and not
anything after `main` (it never gets that far). The draw that wedges is the one
that samples the R8 texture through `texelFetch`, and the driver prints nothing
before stopping -- no refusal, no shader rejection -- so it is stuck rather than
rejecting.

### Narrowed: the fragment sampler variant never finishes compiling

Tracing the draw path stage by stage puts it exactly here. The draw gets past
its refusal checks, calls `gex_wgpu_fragment_module`, clones the NIR, runs
`gex_wgpu_lower_texture_variant` to completion -- and then never returns from
`gex_wgpu_compile_nir`. Nothing after that point is reached: no pipeline, no
bind group, no render pass.

The trigger is the **swizzle**, confirmed by changing one thing. With the
texture as `GL_R8` the host wedges; with the same probe uploading `GL_RGBA8`
instead, the draw runs to completion -- bind groups, pass, `CommandEncoderFinish`
-- and there is no hang at all.

R8 samples as `(r, 0, 0, 1)`, a non-identity swizzle, which is what makes
`needs_sampler_variant` true and sends the shader down the variant path. And
because the shader only ever `texelFetch`es its `sampler2D`, the lowering takes
its no-sampler branch -- "Read by texelFetch only. There is no sampler state to
model, but the view's swizzle still has to be applied." So: **texelFetch plus a
non-identity view swizzle produces NIR that the shader translation chain
(ntv -> Tint -> createShaderModule) does not finish compiling.**

That matters well beyond this probe. Single- and dual-channel textures are
everywhere in real applications -- masks, font atlases, luminance data -- and
every one of them carries a non-identity swizzle.

Worth noting what does *not* reproduce it: the corpus has explicit
`swizzle-r8-rg8` cases and passes them, because it drives the driver directly
rather than through Mesa's GL frontend, so its shaders are a different shape.
SuperTuxKart is unaffected as well. The combination needed is the GL frontend's
lowered `texelFetch` against a swizzled view.

### Narrowed again: `ntv_shader_prepare` does not return

Tracing inside `gex_wgpu_compile_nir` puts it on one call. The variant reaches
the top of the function, clears `nir_lower_tex`, `nir_lower_fragcolor` and
`nir_lower_discard_if` -- and never reaches the trace placed immediately before
`nir_to_spirv`. The only substantial thing between those two points is

    ntv_shader_prepare(nir);

everything else being a struct literal, a bitcount and a clock read. So the
hang is **inside Mesa's NIR-to-SPIR-V preparation**, not in ntv proper, not in
Tint, and not in the browser's module build -- all three of which were the
obvious suspects and all three of which are exonerated: the three shaders that
compile before this one go through them in 2-51 ms.

### Found and fixed: self-referential NIR from the swizzle lowering

Tracing each pass inside `ntv_shader_prepare` and its `optimize_nir` loop --
which runs its ~15 passes until none reports progress -- shows the ninth
iteration entering `nir_lower_vars_to_ssa`, then `nir_opt_copy_prop`, and never
leaving it. The caps on those traces were nowhere near exhausted (nine hits on
the busiest site), so this is one pass failing to terminate, not passes
oscillating.

The reason is in this driver, not in Mesa. `gex_wgpu_lower_view_swizzle_instr`
ended like this:

    channels[i] = nir_channel(b, &tex->def, lower->swizzle[i]);   // reads tex->def
    nir_def_replace(&tex->def, nir_vec4(b, channels[0], ...));    // rewrites ALL uses

`nir_def_replace` rewrites *every* use of `tex->def` -- including the channel
extractions that were just built from it. The new vec4's components become
channels of the vec4 itself. That is not a wrong value, it is malformed NIR,
and copy propagation walks it forever.

The fix is `nir_def_rewrite_uses_after`, which leaves the extractions pointing
at the original definition. The sibling nearest-border pass twenty lines above
already did it that way for exactly this reason; the `tg4` path may keep
`nir_def_replace` because its replacement is an immediate that never reads
`tex->def`.

Why it stayed hidden: this pass only sees what the nearest-border pass
explicitly excludes -- ordinary 2D float sampling is handled there -- so
`texelFetch` is its main customer. Sampling an R8 texture normally was always
fine; fetching from it hung.

`gl44-pixelstore-probe` now passes, and it is the eleventh probe to do so.

### It did not fix the CTS family it came from

`pixelstoragemodes` is still 0/896 and `texture_swizzle` still 2/19 after the
fix. The hang and those failures are separate problems, which the evidence
said from the start and is worth restating: the CTS reports 736 individual
`Fail (Fail)` results, and a family that hung would have died instead. The
reduction found a real bug that the family does not depend on.

The whole path from symptom to here, for anyone repeating it: opcode trace in
the host decoder gives `DRAW_ARRAYS`; stage traces in the draw path give
`gex_wgpu_fragment_module`; traces inside that give `gex_wgpu_compile_nir`;
traces inside *that* give `ntv_shader_prepare`. Each step was one rebuild.

Worth knowing: **no other currently-passing probe samples a texture in a
shader** -- `clear`, `state`, `msaa-blit`, `msaa-depth-blit`, `state-query`,
`point-parameter`, `multi-draw`, `uniform`, `vertex-2101010` all avoid it. The
corpus and SuperTuxKart sample textures constantly and are fine, but neither
runs through the `glclient` probe path or exits the guest process the way a
probe does. That combination -- guest process exit after texture sampling on
the glclient path -- is the thing to look at, and it is a plausible reason for
whole CTS families to come back failing regardless of what they test.

`gl44-pixelstore-probe` is kept as the reproducer: it passes on native Mesa in
milliseconds and wedges the host in its first draw on gex_wgpu. Every other
probe still passes, so whatever this is, it is narrow.

## `pixelstoragemodes`: 0/896 to zero failures, and two driver bugs

The family now reports **736 passed, 0 failed, 160 not supported** -- the 160
are `GL_ARB_compressed_texture_pixel_storage`, an extension this driver does
not have, which is a legitimate skip rather than a defect. Two separate bugs
had to be fixed, and neither was in the pixel-store code the family is named
after.

### 1. Vector comparison reductions trap the SPIR-V emitter

This was the one that mattered. The family's fragment shader ends with

    if (colour.rgb == refcolour.rgb)

and `a.rgb == b.rgb` is a single NIR `ball_fequal3`. Zink's SPIR-V emitter --
which this driver borrows whole -- has **no case for `ball_*` or `bany_*` at
all**; they fall out of `emit_alu`'s switch into `unreachable`, which in wasm
is a trap. `gex_wgpu_create_fs_state` died mid-call, so `glLinkProgram`
reported failure with an **empty info log and no driver diagnostic**, and every
case in the family failed for want of a program.

The fix is one pass zink runs and this driver did not:

    NIR_PASS(_, nir, nir_lower_alu_width, gex_wgpu_alu_target_width, NULL);

which expands the reduction into per-component `feq` plus `iand`. The callback
returns 4, matching `zink_compiler.c:5682` -- the reduction cases in that pass
run *ahead* of its width check, so this lowers what the emitter cannot take
while leaving ordinary vec4 ALU alone.

Worth remembering how this presented: a link failure with no message anywhere.
The stack only appeared in the page's captured console errors --
`emit_alu -> nir_to_spirv -> gex_wgpu_compile_nir -> gex_wgpu_create_fs_state`
-- and that is where to look when a program will not link and nothing says why.

### 2. gl_FragCoord.y is inverted on the window framebuffer

Found while reducing the family, though it turned out not to be its cause.

st/mesa signals framebuffer orientation through the viewport's y scale:
negative for the window framebuffer, positive for an FBO. The draw path
already flips clip-space y for the positive case, which leaves framebuffer 0
rendering straight into WebGPU's top-down framebuffer while GL promises a
bottom-up `gl_FragCoord`. Measured: the bottom row of a 240-row default
framebuffer reported `gl_FragCoord.y = 239.5` where GL requires 0.5.

Anything indexing by `gl_FragCoord` therefore addressed off the end of what it
was reading -- `texelFetch(tex, ivec2(gl_FragCoord.xy), 0)` on an 8x8 texture
fetched y=232..239, out of range, which returns zero, so the draw came out
black. `gex_wgpu_lower_frag_coord_y_instr` rewrites y as `H - y`, and the
correction bakes the height in, so it joins the fragment variant's key
alongside the sampler state and colour output type.

### 3. The sweep counted "not supported" as "failing"

`tests/gl44-cts-sweep.sh` derived its note from `total - passed`, so a family
the CTS had skipped read as broken: `pixelstoragemodes` was recorded as "896
failing" while the CTS's own totals said 0 failed and 160 not supported. That
is a different thing to go and investigate, and it sent this work down the
wrong road more than once. The sweep now reads `Failed:` and `Not supported:`
from the CTS totals and only says `fail` when something actually failed.

Corrected picture of the families swept so far:

| family | status |
|---|---|
| `shaders30` | ok, 651/651 |
| `texture_repeat_mode` | ok, 162/162 |
| `pixelstoragemodes` | **ok**, 736 passed, 160 not supported |
| `packed_depth_stencil` | 2 failing, 2 not supported |
| `transform_feedback` | 6 failing |
| `texture_swizzle` | 13 failing, 4 not supported |

Verified unchanged by both driver fixes: all eleven probes, the corpus, and
SuperTuxKart at 59.8 FPS with 99.5% fresh presents and 0 rAF misses.

## Vertex-stage texture sampling

The driver used to refuse it outright -- `gex_wgpu_set_sampler_views` failed
the context on any real view outside the fragment stage, and
`bind_sampler_states` did the same for samplers, both saying so in comments.
`KHR-GL33.texture_swizzle` needs it, and the shape of that need is worth
recording because the first attempt made things *worse*.

Vertex integer sampling first showed up as a link failure: `vertex shader
rejected (int_sampled=0x1): Tint rejected SPIR-V: sampled image must have float
component type`. The fragment stage has always handled integer sampling by
*deferring* its module and building it at draw time against the real sampler
state; extending that deferral to the vertex stage made the link succeed -- and
then the application bound a vertex sampler view, which latched the context and
took the whole family down after three cases. Fixing the symptom exposed the
actual missing feature.

**What it took.**

- `ctx->vertex_sampler_views` / `ctx->vertex_samplers`, separate from the
  fragment stage's. They cannot be shared: each shader numbers its bindings
  from zero.
- Descriptor set 3 for vertex textures and samplers, assigned in the
  combined-sampler split. Sets 1 and 2 are already the two stages' uniform
  blocks and set 0 is the fragment stage's textures, so the vertex stage needed
  one of its own; the shader-scan validation now expects the stage's set rather
  than hard-coding zero.
- A fourth bind group and layout, entries marked `WGPUShaderStage_Vertex`, with
  its samplers declared non-filtering -- a vertex fetch has no derivatives, and
  WebGPU wants the sampler to say so. `group_layouts` and `bind_groups` grew
  from three to four, as did the loops that bind and release them; the array
  sizes are the easy thing to miss, and overflowing them is silent.
- The vertex variant in `gex_wgpu_interp_variant_get` now carries a sampler key
  -- mask, sampler states and view swizzles -- beside the interpolation,
  viewport and attribute-unpack keys it already had, because
  `gex_wgpu_lower_texture_variant` reads all of that and bakes it into the
  module.

**Result.** `texture_swizzle` stops dying: it ran 19 cases before and runs 626
now, 43 passing. Unchanged by the change: all eleven probes, the corpus, and
SuperTuxKart at 60.0 FPS with 100% fresh presents.

### What the 626 cases then showed

262 failures, and they are value mismatches -- no refusals, no declined
shaders, no driver diagnostics at all. The distribution says where to look:

- The failures are **not** target-specific. Every texture target shows the same
  49 fail / 5 pass / 9 unsupported split, so the cube/array/3D handling is not
  the variable.
- They are **format**-specific, and the five formats that pass are exactly the
  floating-point ones: `R16F`, `RG16F`, `RGB16F`, `RGBA16F`,
  `R11F_G11F_B10F`. Every fixed-point and integer format fails --
  `R8`, `R8_SNORM`, `R16`, `RGB8`, `RGBA8`, `R8UI`, `RG16UI` and the rest.

That is a swizzle-correctness problem across the non-float formats rather than
262 separate defects, and it is newly visible: before this feature the family
never got far enough to run these cases at all.

## `texture_swizzle`: 262 failures down to 29

Two bugs, and the second is the one worth remembering, because five separate
reductions of it all passed before the shape of the CTS's own shader gave it
away.

### 1. The fragment variant cache handed back the wrong module

`sampler_mask |= 1u << slot` sat inside the `if (uses_sampler)` branch, so a
texture read purely by `texelFetch` -- which has no sampler -- left the mask at
**zero**, while `needs_sampler_variant` was still set by the
"`!uses_sampler && swizzled`" branch a few lines above. A zero mask makes the
match loop `continue` past every slot, so `match` keeps whatever it held before
the loop and the *first* cached variant answers for any later shader.

One R8 texelFetch with a reversed swizzle then answered for every texelFetch
after it, whatever its texture or swizzle. The wrong values said so exactly:
`255,0,0,X` is what that first variant computes, and it came back for an RGBA8
texture and for a completely different swizzle.

The same loop also had `match = memcmp(...) && memcmp(...)`, an assignment,
which threw away the `sampler_mask`, colour-output and `gl_FragCoord` checks
made before the loop as well as every slot compared before this one. Both are
fixed: every specialized slot joins the mask, the compare accumulates with
`match &&`, and a slot with no sampler is tolerated rather than dereferenced.

### 2. Vertex-stage sampling ignored the view swizzle

The vertex texture support added earlier only ran its lowering when
`integer_sampled_slots != 0` -- the case that cannot compile at all. A float
`sampler2D` with a non-identity swizzle compiles perfectly and comes out
**wrong**, because nothing applies the swizzle.

That is what the CTS was hitting, and it is why every reduction of it passed:
the functional matrix samples in the **vertex shader**.

    #version 330 core
    uniform sampler2D sampler;
    flat out float result;
    void main() {
        result = texelFetch(sampler, ivec2(0, 0), int(0)).r;
        switch (gl_VertexID) { ... }
    }

Six reductions -- `texture()` and `texelFetch`, the individual
`GL_TEXTURE_SWIZZLE_R/G/B/A` setters and the combined `GL_TEXTURE_SWIZZLE_RGBA`
one, R8/RG8/RGBA8/R16F, fresh textures and re-swizzled ones, RGBA8 and R8
render targets, `glReadPixels` and `glGetTexImage` -- all sampled in the
*fragment* stage, and all passed. Printing the CTS's own shader source ended
it in one step. When a reduction keeps passing, the thing to question is the
assumption the reduction was built on, not the reduction.

The vertex path now uses the same conditions the fragment stage does: integer
sampling, a non-identity swizzle, `CLAMP_TO_BORDER` wrap, or a non-zero LOD
bias.

**Result.** `texture_swizzle` goes from 43 passed / 262 failed to **276 passed
/ 29 failed** (321 not supported). Verified unchanged: all twelve probes, the
corpus, and SuperTuxKart at 60.0 FPS with 100% fresh presents.

`tests/gl44-probe.sh swizzle` is the new gate -- eighteen cases covering both
access routines, both swizzle entry points, four format classes and both
readback paths.

## `texture_swizzle`: 29 failures down to 21, and a log that stalled the run

### textureGrad on an integer texture

`gex_wgpu_lower_integer_sample_instr` emits only `tex` and `txl`; every other
op sets `table->failed` and takes the whole lowering with it. `textureGrad` and
`textureGradOffset` on a `usampler`/`isampler` are `txd`, so eight smoke cases
died there.

An integer texture is never filtered, so the gradient only ever picks a mip
level -- which is exactly what `nir_lower_tex`'s txd lowering computes. The
fix is its `lower_txd_cb` callback, returning true only for an integer result
type. Float `textureGrad` is deliberately left alone: the nearest-border pass
already handles it, and lowering it would change shaders that work.

Those eight now pass, and the family goes from 276/29 to **284 passed, 21
failed, 321 not supported**.

### The remaining 21 are two things, and one of them is the platform

- **20 are `glClear` returning `GL_INVALID_FRAMEBUFFER_OPERATION`** on a SNORM
  output texture (`R8_SNORM` through `RGBA8_SNORM`). WebGPU does not make snorm
  formats render-attachment capable at all, so the FBO is legitimately
  incomplete; the CTS assumes it is renderable because desktop Mesa allows it.
  Not a driver defect, and not fixable without emulating the attachment in
  another format.
- **1 is `TexImage1D` on `GL_DEPTH_COMPONENT16`** returning `GL_OUT_OF_MEMORY`
  -- a 1D depth texture, uninvestigated.

### Per-shader logging stalled long conformance runs

Unrelated to correctness and worth its own note. The driver printed one
`shader compiled in ...` line per shader. A `texture_swizzle` sweep compiles
thousands, the harness accumulates every console line, and the log reached
**45 MB** -- at which point the run stopped making progress entirely and hit
the wall with no totals, twice, at two different wall settings. It looked like
a hang in the driver and was not.

The line is now capped at the first sixteen compiles plus anything taking
100 ms or more, which keeps what diagnoses a slow shader path and drops the
rest. Same sweep: **8.8 MB, and it completes** in 2239s.

Worth remembering as a shape: a run that stalls with no output, at any wall you
give it, may be drowning in its own diagnostics rather than stuck.

### Measurement note

SuperTuxKart is unaffected by any of the vertex-texture work, and that was
established by counting rather than by frame rate: instrumenting the two new
paths showed **zero** draws through either in a full STK run. Its gate readings
this pass -- 57.0, 57.2, 59.5, 59.6, 59.7, 59.9, 60.0 -- sit inside the
harness's own spread, which straddles the 59.8 FPS threshold, so individual
runs pass or fail either way. Count the calls; do not read the frame rate.

## `packed_pixels` measured, and a family the mustpass names but the build lacks

`packed_pixels` is the largest runnable GL 3.3 family -- 4732 cases in three
sub-groups -- and it had never been swept. At roughly two to four seconds a
case the whole family does not fit in one run, so the sub-groups were sampled
individually with `GEX_CTS_CASE`.

| sub-group | result |
|---|---|
| `packed_pixels.rectangle` | **1578 / 1578** |
| `packed_pixels.varied_rectangle` | **1577 / 1577** |
| `packed_pixels.pbo_rectangle` | **1577 / 1577** |

**4732 / 4732.** That is the biggest block of GL 3.3 conformance measured on
this path, and it passes outright. Worth knowing when reading the earlier note about
`pixelstoragemodes`: the pixel-storage machinery those two families share is in
good shape.

**`shaders` is not in this glcts build.** The GL 3.3 mustpass names 2397
`KHR-GL33.shaders.*` cases and `--deqp-case=KHR-GL33.shaders.*` matches
**zero** of them: the binary contains `shaders30`, `shaders42`, `shaders44` and
`shaders45` and no plain `shaders` group, so those entries come from a package
this build does not include. The sweep reported that as `error`, which reads as
a driver problem; it now reports `absent` with "no such family in this glcts
build". Do not go looking for a bug behind it.

### Transform feedback with a geometry shader captures nothing

All six remaining `transform_feedback` failures are the geometry cases --
`capture_geometry_interleaved`, `capture_geometry_separate`,
`query_geometry_interleaved`, `query_geometry_separate`, `discard_geometry`.
The cause is structural rather than subtle: the draw path does

    if (ctx->gs) { gex_wgpu_gs_draw(...); return; }

and the transform-feedback block sits *below* that return, so nothing captures.
The block would not have helped anyway -- it runs `ctx->vs->capture_nir`, the
vertex shader, and GL captures the last pre-rasterization stage, which is the
geometry shader.

The driver now says so once per context rather than leaving the buffers
silently unchanged. Rendering is deliberately left working: the draw itself is
correct, and refusing it would cost an application its image as well as its
capture.

Doing it properly means capturing from the geometry pass, which already runs as
compute and could write the buffers directly. Two things are missing and
neither is small: the emitted-vertex count, which is dynamic where the vertex
stage's is derived from the draw's topology, and a primitives-emitted count for
the `query_geometry_*` cases -- the occlusion/statistics query path currently
counts primitives on the CPU from the draw and its own comment says "a geometry
shader may emit a different number and this does not model that".

## The two remaining defects, specified rather than built

The GL 3.3 landscape is complete now: every family in the mustpass has been
swept, and what is left is 29 failing cases in three groups. Twenty are the
WebGPU snorm-renderable limit and one is a 1D depth texture, both noted above.
The other eight are these two, and both were investigated to the point of a
concrete implementation and then deliberately not built. That is a judgment
call about scale, not about difficulty, and the reasoning belongs here.

### Transform feedback from a geometry shader (6 cases)

The mechanism is understood end to end. The blocker is packing.

GL captures the vertices of the primitives the geometry shader emits, tightly
packed and in primitive order. The geometry pass writes its vertices at a
**fixed stride** -- `invocation * max_vertices + emitted` -- so the buffer is
not packed, and how many a given invocation emitted is not recoverable
afterwards: an unemitted index slot holds a degenerate triangle, which a
legitimately degenerate emitted one is indistinguishable from.

The one piece that makes it tractable is already in the shader and currently
discarded. Mesa emits `nir_intrinsic_set_vertex_and_primitive_count` at the end
of a geometry shader carrying the counts it actually reached; the lowering
drops it. Recording it per invocation gives the per-primitive counts a prefix
sum needs. That step was written and verified not to regress, then reverted --
an unused buffer and binding on every geometry draw is not worth carrying for a
feature that is not finished.

What remains after it is a pipeline stage, not a fix: a prefix over the
per-primitive counts, a gather that walks the index buffer in order and writes
each captured varying into the stream-output buffers in the xfb layout, and a
primitives-emitted count for the `query_geometry_*` cases -- the query path
counts primitives on the CPU from the draw and its own comment admits it does
not model a geometry shader. The routes that would reuse the existing capture
machinery (compacting the index buffer, then capturing the passthrough vertex
shader over it) all need the emitted total on the CPU, which means a readback
per geometry draw -- slowing every geometry draw to serve a rare feature.

The driver now says once per context that the capture did not happen, rather
than leaving the buffers silently unchanged. Rendering is left working.

### Multisample depth-stencil blit drops stencil (2 cases)

`KHR-GL33.packed_depth_stencil.blit` loops sample counts and blits
`DEPTH|STENCIL` both ways between multisampled and single-sampled buffers, then
draws with `glStencilFunc(GL_EQUAL, 1)` and checks the colours.

`gex_wgpu_resolve_depth` resolves depth by rendering it -- a pipeline that
samples `texture_depth_multisampled_2d` and writes `frag_depth`. Stencil has no
equivalent: WebGPU has no stencil resolve, and a shader cannot write an
arbitrary stencil value at all, only apply stencil ops against a reference, so
reconstructing an 8-bit pattern takes eight passes of bit tests against eight
reference values. The single-to-single case goes through the ordinary transfer
path and does carry both aspects; only the multisample directions lose stencil.

The driver now prints `multisample resolve of <format> keeps depth and drops
stencil` once, and that line was confirmed to appear on exactly the failing
case.

## Extensions above the core version (2026-09-01)

GL 4.0 as a *version* is out of reach: `tests/mesa-wgpu-version.sh core` lists
eleven requirements between 3.3 and 4.0, and two of them -- `ARB_gpu_shader_fp64`
and `ARB_tessellation_shader` -- have no WebGPU counterpart at all. What is
reachable is the individual extensions, exposed below their core version the way
`ARB_draw_indirect` and `ARB_timer_query` already were.

Two landed, both one-cap changes because the driver had already been built for
them and only st's cap was missing:

- **`ARB_texture_gather`** -- `caps->max_texture_gather_components = 4`, with the
  offset range set to `[-8, 7]`, which is simultaneously WebGPU's const-offset
  range for `textureGather` and GL's required minimum. zink's ntv already emits
  `nir_texop_tg4`; the view-swizzle lowering already had its tg4 case, where a
  swizzle is a component *remap* rather than a shuffle of the result, because a
  gather selects one channel; and the LOD-bias pass already left gather alone,
  as Gallium requires. Verified by `tests/gl44-probe.sh gather`, byte-for-byte
  against native Mesa including the swizzle interaction.
- **`ARB_texture_cube_map_array`** -- `caps->cube_map_array = true`.
  `PIPE_TEXTURE_CUBE_ARRAY` was already routed through resource creation, view
  creation (`WGPUTextureViewDimension_CubeArray`) and the sampler-view path.
  `tests/gl44-probe.sh cubearray` checks all twelve face-layer slices, so a
  lookup that transposes face and layer fails rather than passing by luck.

`ARB_texture_query_lod` is not a candidate: WGSL has no `textureQueryLod`, so
Tint would reject the SPIR-V. `ARB_transform_feedback3` needs more than one
vertex stream, which the geometry lowering refuses. `ARB_sample_shading` has no
WebGPU control for the minimum sample rate.

One thing to know before adding another: the 3-argument
`textureGather(sampler, coord, comp)` belongs to `ARB_gpu_shader5`, not to
`ARB_texture_gather`. At GLSL 330 only the 2-argument red-channel form compiles,
which is why the probe reaches the green channel through a view swizzle instead
-- and that turned out to be the better test, since it exercises the driver's
own tg4 branch.

## The guest hid every extension (2026-09-01)

None of the above would have reached an application. `guest/gl/gexgl.c` answered
`glGetStringi(GL_EXTENSIONS)` from a hardcoded three-name array and
`GL_NUM_EXTENSIONS` from the literal `3`, and never asked the host, which reports
135. **An extension nothing can detect is an extension nothing can use**, so
every cap added to the driver was invisible.

Forwarding the host's list wholesale is not safe either -- an extension promises
entry points as well as behaviour, and advertising one whose commands the guest
does not export turns an application's graceful fallback into a null call. So
`tools/gen_gl_extensions.py` derives the answer from the Khronos registry and
from the library's own `nm` output, and emits two generated headers:

- the allowlist of extensions whose registry-required commands are all exported,
  fail-closed on anything the registry does not describe; and
- 449 `__attribute__((alias))` definitions. Most of what the first cut withheld
  was a spelling rather than a capability: `ARB_vertex_buffer_object` requires
  `glBufferDataARB`, which *is* `glBufferData`, and ten extensions applications
  routinely test for -- vertex buffer objects, instancing,
  `EXT_framebuffer_object`, `EXT_texture_storage` -- were being withheld over a
  suffix. Guest exports went 525 to 974.

Visible extensions went 3 to 109. The 26 still withheld are honest: direct state
access, debug output, program binary, shader subroutines, and the two commands
(`glCreateShaderProgramv`, `glGetProgramPipelineInfoLog`) that
`ARB_separate_shader_objects` is missing.

Two traps worth keeping:

- The generator must skip `<require profile="compatibility">`. The guest presents
  a core profile, and `ARB_vertex_type_2_10_10_10_rev` -- which this driver
  implements -- asks for 38 commands of which 30 are the compatibility
  `glColorP3ui` family. Without the filter a working extension reads as
  unclaimable.
- `guest/gl/build.sh` links twice. The allowlist comes from the object's own
  exports, so it cannot be generated before a link, and the aliases it adds
  change that export set. The generator prints `changed` and the script relinks
  on that; in steady state the second link never runs.

To see what is being withheld:

    GEX_GUEST_ENV=GEXGL_EXTENSION_TRACE=1 tests/gl44-probe.sh cubearray

`?glenv=KEY=VALUE` on the demo page carries environment variables into the guest
process; before this, nothing between the harness and the guest carried an
environment at all. Pick a probe that actually enumerates extensions -- `gather`
never calls `glGetStringi`, so the trace stays silent and looks broken.

## SuperTuxKart's scene was upside down (2026-09-01)

The application gate reported `presented fresh=600/600` run after run, and its
own closing line -- "correctness needs the screenshot" -- had never been acted
on. The screenshot showed the 3D scene vertically mirrored, sky at the bottom
and the kart inverted, underneath a correctly-oriented HUD.

The split between the two is the whole diagnosis. This driver sets
`fs_coord_origin_upper_left` and clears `fs_coord_origin_lower_left`, which
tells st/mesa that fragment coordinates arrive with WebGPU's top-down origin;
st answers by running `nir_lower_wpos_ytransform` itself against a state
variable it updates with the bound framebuffer's orientation. `gl_FragCoord`
was therefore **already correct** in the NIR handed to `create_fs_state`.

`gex_wgpu_fragment_module` applied a second correction, `y' = H - y`, whenever
the viewport y scale was negative. A fragment variant is only built for a
shader that has a texture or a retyped output, so the double flip reached
textured shaders and not textureless ones: `gl_FragCoord` depended on whether
the shader happened to sample anything. SuperTuxKart's scene composite
addresses its render target by `gl_FragCoord` and its HUD does not, which is
exactly the image that came out.

The pass, its entry in the variant key, and the struct field are gone. STK now
renders upright; it is still underlit, which is a separate open problem.

`tests/gl44-probe.sh fbo-orientation` holds the line, with seven cases: a
render target composited onto framebuffer 0 three ways -- a textured quad,
`glBlitFramebuffer`, and a `gl_FragCoord`-addressed quad -- each compared
against the same pattern drawn straight to framebuffer 0, plus `gl_FragCoord.y`
read back as a colour from two shaders differing only in whether they sample.

Two things that probe is built around, and that any successor needs:

- **Framebuffer 0 cannot be read reliably on native Mesa** under
  `EGL_PLATFORM=surfaceless`. It reports draw=BACK/read=FRONT and changes its
  answer if `GL_READ_BUFFER` is merely queried first, so it cannot serve as an
  absolute reference. Every comparison here is between two framebuffer-0
  readbacks and the quirk cancels.
- **The guest's EGL ignores the requested pbuffer size.** The probe asks for
  4x4 and gets a 320x240 framebuffer with the viewport at `vp=0,236`. A probe
  sized to its own framebuffer would have hidden this: it is the tall
  framebuffer that turns a wrong `gl_FragCoord.y` into a saturated value
  instead of a plausible one.

The general lesson: before adding a coordinate correction, check the driver's
`fs_coord_*` caps to see whether st is already doing the transform, and read
the coordinate back from **two shaders that differ only in whether they
sample**. Correcting at the variant site fixes half the shaders and silently
breaks the other half.

## SuperTuxKart renders correctly (2026-09-01)

With the orientation fix above, the scene came out upright but almost entirely
black -- kart, track and walls solid, only the snow and the sky right. That
turned out not to be a rendering bug at all.

`tests/mesa-wgpu-app.sh` staged the Mesa host module into `web/{public,dist}/gex`
and nothing else, so the application kept loading whatever guest GL marshaller
`web/build-assets.sh` had last produced. It was six days old, predating the
change that lets the guest report the host's extension list, so SuperTuxKart
could not see `EXT_texture_compression_s3tc` and every DXT texture in the track
decoded to nothing. Every diagnostic in the log described the current host
driver, truthfully, while the guest half was stale.

Two things make that trap easy to fall into again:

- **The demo loads the glibc build**, from `/fs/glibc/`, not the musl pair at
  `/fs/` -- `directGlAbi: 'glibc'` in `web/src/demos.js`. Refreshing only the
  musl one changes nothing and looks like it should have.
- **The application's own capability log is the tell.** `grep "GLDriver:"` on
  the drive log: two lines means the guest is reporting almost no extensions.
  Three, including S3TC, is the correct state at GL 3.3.

The gate now builds and stages both ABIs of the guest driver itself.

### The alias generator was not idempotent

Found while fixing the above: `tools/gen_gl_extensions.py` computed the
aliases to emit as `wanted - exported`, and read `exported` from `nm` on the
linked library -- which already contained the previous run's aliases. So the
second build emitted none, and the relink dropped all 449. The musl and glibc
builds of identical source came out with 974 and 525 exported functions.

It now subtracts the registry's alias names from the symbol set before
deciding, so what it emits cannot depend on what it emitted last time. The
check is three consecutive `guest/gl/build.sh` runs producing the same
counts. Any generator that adds symbols to the artifact it measures needs
this.

### The gate checks the picture

`tools/frame-stats.py` reports a frame's mean luminance, luminance spread, the
fraction of near-black pixels, and how many cells of a 4x4x4 colour cube it
touches. `tests/mesa-wgpu-app.sh` fails on `dark > 0.20`, `stdev < 0.08` or
`distinct < 10` -- bounds that describe "a rendered 3D scene" rather than this
particular frame. A good STK frame measures `mean=0.62 stdev=0.21 dark=0.001
distinct=29`; a fully black one measures `dark=1.000 stdev=0.000 distinct=1`.

A frame counter cannot see any of this. STK presented 600 of 600 frames
throughout.

## The gears demo was never being displayed (2026-09-01)

With SuperTuxKart rendering correctly, the other application on this path --
the `gears` demo, whose entire point is to show a picture -- turned out to have
a black canvas, and to have had one for as long as it ran on Mesa. Two
instruments were lying about it at once.

**Rendering and presentation are separate steps.** The backend renders into an
offscreen texture and copies it to the canvas surface only when the page calls
`display()`; the guest's `eglSwapBuffers` only marks a frame *produced*. The
wayland demo drives that from its own animation loop, which is why STK -- which
borrows `waylandGears.enter` -- presented fine. The gears demo had no loop, so
`display_backend()` never ran once.

Everything else was green throughout: the context came up, shaders compiled,
buffers uploaded, draws issued, the guest's own readback reported `covered 15%`,
the terminal said `frames 200`, and `tests/mesa-wgpu-live.sh` matched on that
text and passed.

The tell in the log is an *absence*. `display_backend()` prints
`first present -- canvas resize to WxH` the first time through, and prints
`display copies nothing (...)` when it cannot copy. Neither line appearing at
all means nothing ever asked to present. Grep for both before looking for a
rendering fault.

**The screenshot was framing the wrong region.** `drive.mjs` clips its capture
to the canvas -- correct -- but built the clip from `getBoundingClientRect()`,
which is in viewport coordinates, and passed it with `captureBeyondViewport:
true`, which clips against the page. Its own `scrollIntoView({block:'center'})`
is what makes the two disagree, so the captured rectangle sat `scrollY` above
the canvas: every screenshot of a demo whose canvas is below the fold showed
the page heading and a strip of the canvas's top edge. Fixed by adding
`window.scrollX/scrollY`, and by selecting `.canvas-wrap canvas:not(.wl-offscreen)`
instead of the document's first canvas.

**Both gates read the canvas now.** `tests/mesa-wgpu-live.sh` fails on
`dark > 0.60` or `distinct < 4`; the three gears measure `dark=0.000
stdev=0.115 distinct=8` and a canvas nothing reached measures `dark=0.983
stdev=0.120 distinct=2`, so the bound is well clear of both. `tests/mesa-wgpu-app.sh`
keeps its stricter scene bounds, and no longer crops -- with the clip fixed the
screenshot is the canvas and nothing else.

The general point, and it is the third time in two days it has cost something:
**a counter that advances is not a picture.** Frames produced, frames
presented, triangles drawn and guest-side coverage were all healthy for a demo
that displayed nothing.

## A second application: EGL on X11 (2026-09-01)

With SuperTuxKart and the gears demo both rendering, the next question was
what else runs. The Wayland demo can launch a dozen clients, and `?gl=mesa`
routes any of them through this driver -- but three separate faults stood in
the way, none of them in the driver.

**`renderer.commands()` returns null on the Mesa backend**, and both call
sites did `renderer.commands().split('|')` unguarded, so every completing
`?gl=mesa` run of the virgl and Wayland demos ended in "Cannot read properties
of null" instead of its own summary. SuperTuxKart was unaffected only because
the harness kills it before the report runs.

**`?wlclient=` was taken literally** even when it named a listed app.
`pickInitialApp` resolves the id one screen earlier -- and its `mode` was used
-- but the launch line then re-read the raw query value, so
`?wlclient=es2gears` ran a program called `es2gears`: "No such file or
directory". Half-resolved.

**The Wayland demo injected the wrong-ABI marshaller.** Its clients are all
Debian aarch64 binaries, and the demo declared no `directGlAbi`, so `?gl=mesa`
mounted the *musl* `libGLESv2.so.2` into a glibc program. The failure reads as
`libc.so: cannot open shared object file` -- musl's soname, reported against a
glibc client, which looks like a missing library rather than the wrong ABI.
SuperTuxKart borrows this demo's `enter` but declares its own
`directGlAbi: 'glibc'`, so it never hit this and the gap stayed hidden.

### What that uncovered in EGL

With the client finally launching, `es2gears_x11` exited immediately:

    EGLUT: failed to get an visual of id 0x0

`eglGetConfigAttrib` had no case for **`EGL_NATIVE_VISUAL_ID`**, so it fell
through to 0. EGL on X11 has to name a visual the server actually has, because
the application passes it to `XCreateWindow`; eglut takes it straight to
`XGetVisualInfo`, finds nothing and exits. **Every EGL-on-X11 program failed
this way.** It now answers with the X server's default visual, found by
`dlopen`ing `libX11.so.6` with `RTLD_NOLOAD` and opening a short-lived
connection of our own.

`RTLD_NOLOAD` is doing two jobs there: a Wayland-only client must not acquire
an X dependency by asking EGL a question, and its failing is how we know the
display is not an X one. The connection is ours rather than the caller's
because what `eglGetDisplay()` was handed is a `Display*` for an X11 client and
a `wl_display*` for a Wayland one, and nothing in the pointer distinguishes
them.

`EGL_SURFACE_TYPE` also claimed only `EGL_PBUFFER_BIT`, though
`eglCreateWindowSurface` is implemented and is the path SDL takes. Now
`EGL_WINDOW_BIT | EGL_PBUFFER_BIT`.

SuperTuxKart hit neither, because SDL keeps it away from EGL's config
attributes. A program that asks EGL directly exercises a path STK does not --
worth remembering before concluding "STK works, so the stack works".

`es2gears_x11` now presents ~1000 frames a run and matches the virgl reference
picture. It is a listed app (`es2gears-x11`) and a gate:
`tests/mesa-wgpu-app.sh es2gears 30`, with its own frame bounds because gears
is three shapes on black and most of the frame is legitimately dark.

### What is still out of reach: native Wayland EGL

`es2gears_wayland` cannot run on this path, and this is a missing feature
rather than a bug to hunt. `guest/gl/gexgl.c` has no Wayland platform: no
`wl_egl_window`, no `wl_buffer`, no attach/damage/commit, no frame-callback
handling. The client renders exactly one frame -- the log shows
`frame 1 -- issued 3, declined 0` and a completed `first present -- done` --
and then blocks forever waiting for a frame callback the compositor cannot
send, because it was never handed a buffer. Under virgl the same binary runs
at 217 frames, because real Mesa implements the platform.

Building it means wl_egl_window/wl_buffer plumbing in the marshaller plus a
per-frame readback out of WebGPU into `wl_shm`. Not attempted. The X11 build of
the same program is the way to run it today.

## Client-side vertex arrays (2026-09-01)

With EGL-on-X11 working, the next program down -- `es2tri`, the smallest real
GL application in the rootfs -- got as far as its clear colour and drew no
triangle. The driver log said `frame 1 -- issued 1, declined 0`: the draw went
out, nothing was refused, and no GL error was raised anywhere.

`glVertexAttribPointer(..., verts)` with no `GL_ARRAY_BUFFER` bound points at
the application's own memory. GLES 2, GLES 3 and desktop compatibility all
allow it, and it is how every mesa-demos program and most tutorial code feeds
geometry. A guest pointer means nothing to the host, so the value was
travelling as if it were a buffer offset, and the triangle was assembled from
whatever the host found at address 0x7ffd...

The comment beside that line said the case was "rejected loudly". Nothing
rejected anything. **A comment describing behaviour the code does not have is
worse than no comment** -- it answers the question you came to ask. Check the
branch exists.

The fix is entirely on the guest side and needs no protocol or host change:
track each attribute's size, type, stride and pointer along with whether a
buffer was bound *when it was set*, and before a draw copy every enabled
client array into one scratch buffer per attribute, using the
`glGenBuffers`/`glBufferData` commands the protocol already has. Sending the
vertices inline -- the way `glDrawElements` already sends client indices --
would need a new wire shape and host support for it, to no advantage.

Three decisions in it worth keeping:

- **The copy mirrors the array from element zero**, not from `first`. Its
  layout then matches the original, so the draw command, its `first`, and any
  index values need no adjustment.
- **The last element is given its own size, not a whole stride.** An
  application may end its array exactly there, and reading a stride past it
  faults on a tight allocation.
- **`glDrawElements` scans its indices** for the highest, to upload exactly as
  far as the draw reads. That is possible only because client indices already
  arrive on this side; with the indices in a buffer there is nothing to scan,
  and that combination is left to the host as before.

Cost when unused is a single comparison -- `vertex_attrib_client_count` stays
zero for buffer-backed applications and is tested first. SuperTuxKart, the
gears demo, the Gallium corpus and thirteen probes are unchanged by it.

`es2tri` is now a listed app and a gate: `tests/mesa-wgpu-app.sh es2tri 25`.
`distinct` is what carries that one -- the grey clear alone reaches 4 cells of
the colour cube and the triangle takes it to 19, so a regression shows up as a
number rather than as a look.

## Swap pacing: es2gears at 17 FPS was not a rendering problem (2026-09-01)

`es2gears_x11` rendered correctly but ran at 17 FPS, against 60 for the same
program on virgl. `GEXGL_SUBMIT_TRACE=1` -- a tally of submits by the command
that preceded them, added for this -- answered it in one line:

    gexgl: 20000 submits, by the command that preceded them
    gexgl:   opcode 4: 19992 (100.0%)

Opcode 4 is `PRESENT`. The program was issuing **400 swaps a second to put 15
frames on screen**. `eglSwapInterval` was a no-op and `eglSwapBuffers` returned
immediately, so the application free-ran; each swap is a blocking round trip of
about 0.3 ms, and at that rate the round trips alone took 45% of its wall time
and starved the page's animation loop -- the very loop it was waiting for,
since the backend copies a frame to the canvas only when `display()` is called.

**EGL's default swap interval is 1.** A program that never calls
`eglSwapInterval` is asking to be paced, and most never call it.

`PRESENT` now replies with the number of frames the *page* has shown --
`displayed`, not `produced` -- and the guest sleeps while it is further ahead
than that. The change is additive: `reply_u32()` yields 0 from a decoder that
predates the reply and nothing acts on zero, so a stale host degrades to the
old behaviour instead of misparsing. Both decoders answer it, the WebGPU one in
`web/mobilegl/gexgl-mobilegl.cpp` and the WebGL one in `web/src/gl.js`.

On the same program, over 50 seconds:

| | before | after |
|---|---|---|
| FPS | 17.0 | **60.0** |
| frames shown | 752 | 2258 |
| submits | 75,633 | 2,268 |
| bytes to the host | 85 MB | 2.6 MB |

Two details that had to be right:

- **One frame in flight is normal.** The page displays a frame *after* the swap
  that produced it, so `ahead` sits at 1 for a well-behaved application and
  touches 2 whenever the display loop misses a beat. Pacing at `ahead > 1` cost
  SuperTuxKart a fifth of its frame rate -- 60 down to 48.8 -- for nothing. The
  threshold is `interval + 1`.
- **`shown == 0` must not pace at all.** That is either a decoder without the
  reply or a page with no display loop, and the second was the gears demo until
  the fix described above it.

16.7 ms is the feedback loop's *step size*, not an assumed refresh rate: being
one frame over the slack sleeps about that long, and the next reply says
whether that was too much. A page displaying faster keeps `ahead` small and
barely sleeps; a slower one lets it grow.

Unchanged by it: the CTS at 860/870 with the same per-family timings (glcts uses
pbuffers and does not swap per case), the Gallium corpus, twelve probes, and
SuperTuxKart at 60 FPS.

### Also fixed here

`eglQueryString` had `EGL_VENDOR` and `EGL_VERSION` the wrong way round, and
the comments beside them named the wrong constants, so `eglinfo` printed
`EGL vendor string: 1.5` and `EGL version string: gex`. The version string is
now `"1.5 gex"`, the shape EGL requires and that applications parse.

`eglinfo` and `es2_info` both run to completion on this path.

## The unmeasured CTS families (2026-09-01)

Eighteen families in the GL33 mustpass had never been run -- the sweeps had
only ever covered the big ones. Sweeping them gives **876/1121** and, more
usefully, a map: fifteen clean, four above the version ceiling, four real
failures, and two traps that turn out to be one bug.

### The traps are one bug, and it is not a GL bug

`framebuffer_blit.multisampled_to_singlesampled_blit_color_config_test` and
`texture_lod_bias.texture_lod_bias_all` die with an *identical* signature:

    guest pc=0x4a61eb8, in [image] +0x61eb8
    memory access out of bounds ... after syscall 29

`.text` in that binary starts at 0x305dc0, so offset 0x61eb8 is **before any
code** -- it lands in the image's dynamic symbol table. x30 is 0 and x16/x17,
the aarch64 PLT scratch pair, hold values that make no sense as a GOT slot and
a target. The guest branched to a non-code address. Both crashes happen
immediately after the test case is announced and before any GL work reaches the
log, so this is a **gex runtime / dynamic-loader defect**, not a driver one, and
belongs to whoever owns `crates/gex-ld` and the shim.

Two things ruled out along the way. The client-side vertex array upload is not
responsible -- disabling it entirely leaves both traps unchanged. And the
`linear memory grown to 658 MiB` line that precedes them is printed by the trap
*reporter* on every trap; it is context, not cause.

### Four real failures, diagnosed

- **`frag_coord_conventions.multisample`** needs `GL_ARB_sample_shading`, which
  is not exposed. Its vertex shader `require`s the extension and the build
  fails. Version ceiling.
- **`clip_distance.coverage`** -- a fragment shader *reading* `gl_ClipDistance`
  fails to link:

      Tint rejected SPIR-V: no matching call to 'bitcast<f32>(array<f32, 8>)'

  WGSL has `clip_distances` as a vertex output only, with no way to read it in a
  fragment shader. SPIR-V *does* allow the fragment input, so ntv emits one and
  Tint refuses the module. Writing it works -- `clip_distance.functional` and
  `.negative` pass, which is the use that matters, and they only pass because
  `compact_arrays` keeps the array scalar instead of letting the GLSL linker
  pack it into the vec4[2] `gl_ClipDistanceMESA` that ntv then decorates as
  BuiltIn ClipDistance.

  Fixing the read is a feature, not a fix: the clip array has to be duplicated
  into spare varying locations by a vertex-stage pass, the fragment input
  rewritten to read them, and the two coordinated across independently compiled
  stages against a 16-location budget. `tests/gl44-probe.sh clipdistance`
  guards the working half and carries the reproduction shader in its header.
- **`primitive_restart.restart_mode`** -- restart itself is implemented, on the
  CPU: `util_draw_vbo_without_prim_restart` splits the draw, because WebGPU has
  restart only for strip topologies and only at a fixed index. What
  `restart_mode` checks beyond that is uninvestigated.
- **`texture_size_promotion.functional`** -- `InternalError`, "Texture Size
  Promotion Test have approached error". Uninvestigated.

`transform_feedback3`'s three failures need more than one vertex stream, which
the geometry lowering refuses; `cull_distance` (218), `gpu_shader5_gl`,
`texture_storage` and `transform_feedback2` are all reported by the CTS as *not
supported*, which is the version ceiling and not a defect.

### Also fixed here

`eglQueryString` had `EGL_VENDOR` and `EGL_VERSION` swapped, with comments
naming the wrong constants, so `eglinfo` printed `EGL vendor string: 1.5` and
`EGL version string: gex`. The version string is now `"1.5 gex"`, the shape EGL
requires. `eglinfo` and `es2_info` both run to completion on this path.

## Framebuffer 0 was always the canvas size (2026-09-01)

Chasing `KHR-GL33.primitive_restart.restart_mode`, the first step was to find
out whether primitive restart worked at all. `tests/gl44-probe.sh primrestart`
answers that without a golden image: a restarting draw must produce exactly
what the same indices produce as separate draws with the restart taken out, so
each topology is rendered both ways and the framebuffers compared. Eleven
topologies -- points, lines, both line strips, the loop, triangles, strip, fan,
and all four adjacency forms -- and all eleven match, natively and in the
browser. **Restart was never the problem.**

What the CTS was actually catching is in `testApply`: it reads **framebuffer 0**
at `m_context.getRenderTarget()`'s size and checks spot positions. The CTS runs
with `--deqp-surface-width=64 --deqp-surface-height=64`, `eglQuerySurface` --
answered on the guest side from what was requested -- duly told it 64x64, and
the driver's own draw log said `fb=320x240`.

The Mesa host's handler was:

    case SURFACE_RESIZE: { a.i32(); a.i32(); break; }

It read the width and height and threw them away. The WebGL decoder in
`web/src/gl.js` implements the same opcode, so only the Mesa/WebGPU path was
affected -- and only framebuffer 0, which is why so much else looked right.
Every probe and every CTS case had been running against a 320x240 default
framebuffer whatever it asked for.

Three things have to move together on a resize, and any one missing leaves it
half-applied: the cached `mesa_drawable.attachments` are the old size and must
be dropped so `drawable_validate` rebuilds them; st only asks for them again
when the drawable's **stamp** changes, so that is bumped; and the canvas surface
is configured for the old size, so it is released and
`configure_present_surface` redoes it. `resize_drawable` sits inside
`#if defined(GEX_MESA_WGPU)` while the command dispatch is shared, so the other
build configuration needs its own no-op.

The framebuffer is now the size that was asked for -- the orientation probe
reports `fb=4x4` where it used to report `fb=320x240`, and its header comment,
which documented the old behaviour as a trap, has been corrected.

Re-measured after it: 15 probes, the Gallium corpus, SuperTuxKart, es2gears,
`mesa-wgpu-live`, and 16 CTS families, all unchanged. `primitive_restart` still
fails, but now only on POINTS, LINES and LINE_STRIP spot positions -- the
triangle topologies started passing.

**The residual cause is not known, and five explanations are ruled out.** That
list is the useful part, so nobody spends the time again:

- Restart itself is right on eleven topologies and on **both** an FBO and
  framebuffer 0 -- `tests/gl44-probe.sh primrestart`, 22 cases.
- The framebuffer is the size the test asked for and the viewport matches it:
  `fb=64x64 vp=0,0 64x64` in the driver's own draw line.
- Point and line rasterisation is **pixel-exact against native Mesa**.
  `tests/gl44-probe.sh raster-position` places single points at exact pixel
  centres and draws a horizontal line; both backends light the same pixels,
  including the five that GL's end-point rule calls for on the line. The
  "thin-primitive rasterisation" guess recorded earlier is wrong.
- Framebuffer 0 behaves as an FBO does, for this workload.

What is left is inside the test's own setup -- its vertex data, its spot
coordinates, or which buffer `ReadScreen` reads -- and has not been looked at.

## texture_size_promotion: multisampled array textures (2026-09-01)

The last undiagnosed failure from the sweep. `KHR-GL33.texture_size_promotion`
walks eight texture targets and throws on the first GL error, logging only
"Texture Size Promotion Test have approached error" -- which says nothing about
where, and the exception message never reaches the log.

`tests/gl44-probe.sh texture-targets` says where. It allocates each of the same
eight targets the way that test does and records the error code:

    texture_1d, texture_2d, texture_1d_array, texture_rectangle,
    texture_2d_array, texture_3d, texture_2d_multisample     alloc=0x0000
    texture_2d_multisample_array                             alloc=0x0502

`GL_TEXTURE_2D_MULTISAMPLE_ARRAY` fails with `GL_INVALID_OPERATION`. WebGPU
requires a texture with `sampleCount > 1` to be `2d` with exactly one layer, so
a multisampled array has nothing to be made of. GL 3.2 makes the target
mandatory, so this is a ceiling the backend cannot reach rather than a defect
to fix -- the same category as the twenty `texture_swizzle` cases that want
renderable snorm.

The probe grades the seven that work and *reports* the eighth without grading
it, because native Mesa allocates it and this backend cannot: grading it would
make the two sides disagree by design. What it does assert about the eighth is
that it fails **cleanly** -- the multisample array is allocated first, so the
seven graded cases after it are also a check that its failure did not latch the
context.

That closes the accounting on the sweep. Every failure now has a cause:
`primitive_restart` was the default framebuffer size (fixed; residual is
thin-primitive rasterisation), `frag_coord_conventions` wants
`ARB_sample_shading`, `clip_distance.coverage` wants a fragment-stage
`gl_ClipDistance` read that WGSL has no form for, `texture_size_promotion`
wants multisampled arrays, `transform_feedback3` wants multiple vertex streams,
and the two traps are one dynamic-loader bug outside this driver.

## GLX, and glmark2 (2026-09-01)

GLX -- not EGL -- is how a traditional Linux program asks X for a GL context,
and this project shipped no `libGL.so.1` at all, so that whole class could not
start. `guest/gl/gexgl-glx.inc` now implements 30 GLX entry points over the EGL
in the same object, and `libGL.so.1` is another copy of that object, the way
`libEGL.so.1` already was.

**`glxinfo` and `glmark2` both run.** glmark2 -- thirty scenes of shader-based
rendering -- reports ~60 FPS and a score of 59, and is the heaviest workload
this driver has faced. It found two driver bugs immediately; see below.

Five things had to be right, and every one of them looked like GLX not
working:

1. **Xwayland must not see our libGL.** It links libGL for glamor and wants the
   GL 1.x entry points this library has never had. Put ours on the shared
   library path and Xwayland dies at load -- `undefined symbol:
   glConvolutionParameterfv` -- taking the X server and every client with it.
   So libGL is staged in `/usr/lib/gex-glx/`, deliberately off
   `LD_LIBRARY_PATH`, and only the client's own command line prepends it.
   `demos.js` had warned about exactly this.
2. **GLX has to initialise EGL itself.** A GLX program calls no EGL at all, so
   `egl_ready` stayed false and `eglMakeCurrent` refused every context. glxinfo
   reported "glXMakeCurrent failed" with the visual, the config and the context
   all correct.
3. **`GLX_VISUAL_ID` of 0 means "no usable visual."** A program that scores
   configs is entitled to reject that, and glmark2 did -- it found one matching
   config and refused it. It now answers with the server's default visual. Four
   configs are offered instead of one, alpha and stencil present or absent,
   because a driver offering exactly one config gives a scoring program nothing
   to choose between.
4. **A resize has to survive context creation.** `glXMakeCurrent` learns the
   window's geometry, so `SURFACE_RESIZE` arrives *before* `context_create` --
   which rebuilt `mesa_drawable` from `backend_width/height` and discarded it.
   glmark2 asked for 800x600, got 300x300, and rendered its scene outside the
   framebuffer.
5. **A GLX context is a compatibility context**, and the difference that
   matters is not the shader dialect -- legacy GLSL already compiled -- but the
   **default vertex array object**. Core has none, GL2-era code never creates
   one, and st drops every draw before the driver is called. glmark2 swapped
   1910 frames and issued *exactly zero* draws. The profile now travels on the
   wire (`CONTEXT_CREATE` gained a fourth, optional word, read back defensively
   so a stale guest still gets a context) and `eglCreateContext` honours
   `EGL_CONTEXT_OPENGL_PROFILE_MASK`, which it had been ignoring. A
   compatibility context also exposes **212** host extensions where core
   exposes 135.

### Two bugs glmark2 found

**`GL_EXT_framebuffer_object` was being withheld**, so four scenes -- desktop,
terrain, shadow, refract -- reported "requires GL framebuffer support" and were
skipped. glmark2 checks for the EXT extension, not the ARB one. The host
advertises it; this library withheld it for want of `glBindFramebufferEXT` and
`glBindRenderbufferEXT`, which the registry does not mark as aliases because
EXT lets an application bind a name it never generated and core does not. They
are now in `CURATED_ALIASES` in `tools/gen_gl_extensions.py`, with that one
difference written down as the price.

**A NIR deref used outside its block becomes a pointer WGSL cannot hold.** The
`loop` scenes failed to link with `Tint rejected SPIR-V: function-scope 'var'
must have a constructible type`, on a shader that is a `mat4` uniform
multiplying a value a loop computed. Neither half fails alone. Full account in
the section below; the fix is `nir_rematerialize_derefs_in_use_blocks_impl`
after `ntv_shader_prepare`.

### Reading a module Tint will not read

Tint's diagnostic names a construct without saying which instruction, and there
is no WGSL to look at because the failure is in the *reader*.
`gex_wgpu_tint.cpp` now prints the rejected module as hex, once per process:

    grep -oE 'SPIRVHEX [0-9a-f]+' log | sed 's/SPIRVHEX //' | tr -d '\n' \
      | xxd -r -p > m.spv && spirv-dis m.spv

`spirv-dis` and `spirv-val` are both installed on this machine. The answer was
immediate: the module is *valid* SPIR-V, its only two Function-scope variables
are `float` and `uint` and perfectly constructible, and an `OpAccessChain`
inside the loop body had its result used in four instructions after the loop.
A pointer, live across basic blocks. Reach for this before theorising about
Tint.

Also fixed: `glGetString(GL_SHADING_LANGUAGE_VERSION)` was hardcoded to
`"4.40 MobileGL WebGPU"` -- a backend that is gone, a version the context does
not have, and an invitation for an application to write `#version 440` shaders
that cannot compile. It comes from the host now, which says 3.30. glxinfo
prints it beside the real GL version, which is where the disagreement showed.

### glmark2 scene coverage

**29 of 30 scenes run.** The four that reported "requires GL framebuffer
support" -- desktop, terrain, shadow, refract -- were skipped only because
`GL_EXT_framebuffer_object` was withheld. With that fixed, `desktop`, `shadow`
(58) and `refract` (57) render.

`terrain` times out, and **not because of this driver**: it hangs identically
on the **virgl** path, where real Mesa runs in the guest and gex_wgpu is not
involved at all. It is glmark2's heaviest scene being too slow in this
environment. The shape of it, so nobody chases it: the browser grows to about
5.5 GB RSS, the guest goes quiet at its event loop, and the last thing the
driver sees is a handful of 72-byte buffer uploads for the post-processing
quads. Running the whole suite in one go therefore stalls there; run scenes
individually or pick a subset.

Gate: `tests/mesa-wgpu-app.sh glmark2 30`, which runs the `build` scene and
checks the canvas.

## Running the driver natively, on Dawn

The driver never depended on a browser. It takes a `WGPUDevice` and speaks
`webgpu.h`; everything emscripten-shaped lives in the page glue. Given a
device and somewhere to put a finished frame, an ordinary Linux GL program can
run on it, and that is what
`src/gallium/drivers/gex_wgpu/gex_wgpu_native.c` supplies. It is compiled only
when `-Dgex-dawn-native-dir=` is set, so the wasm build is untouched.

```
~/.cache/gex-dawn-native/build.sh     # Dawn, clang -- gcc fails on -Winvalid-constexpr
~/.cache/gex-dawn-native/package.sh   # ~100 archives concatenated into one libtint.a
web/build-mesa-native.sh              # -> /tmp/gex-mesa-native/inst
```

```
LD_LIBRARY_PATH=/tmp/gex-mesa-native/inst/lib:~/.cache/gex-dawn-native/pkg/lib \
LIBGL_DRIVERS_PATH=/tmp/gex-mesa-native/inst/lib/dri \
LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=gex_wgpu <program>
```

`softpipe` is built alongside on purpose. Swapping `GALLIUM_DRIVER` runs *the
same application through the same Mesa* against a reference implementation,
which is the only cheap way to separate "the driver is wrong" from "the
application asked for something odd". `GALLIUM_TRACE=file.xml` records the
exact st/mesa call stream, and a crash is a symbolised backtrace instead of a
wasm frame number.

That paid for itself immediately. Everything in the next section was found in
one afternoon, and every one of them had been sitting in the browser build
unnoticed.

### What the native path found

**Fixed-function shaders reached an emitter that could not spell them.**
`glxgears` aborted in `nir_to_spirv`'s `emit_alu` on
`UNREACHABLE("should already be lowered away")`. The opcodes were `seq`,
`sne`, `slt`, `sge` -- the ARB_vertex_program comparisons, which is what
Mesa's *fixed-function* vertex program is written in. The driver set
`lower_fdph` but not `lower_scmp`.

The emitter is zink's, so the general rule is: **an op zink lowers away is an
op the emitter cannot emit.** Comparing the two option blocks found seven
missing flags, and checking each against `nir_to_spirv` confirmed none of them
has a `case` at all: `lower_scmp`, `lower_hadd`, `lower_iadd_sat`,
`lower_uadd_sat`, `lower_usub_sat`, `lower_mul_high`, `lower_fisnormal`. The
one deliberate difference is `lower_fsat`, which the emitter does handle.

**Display-list buffers had no vertex usage.** Mesa's `vbo_save` tags its
buffers `PIPE_BIND_VERTEX_STATE` only, then binds them through
`set_vertex_buffers` when the driver has no `draw_vertex_state` -- which this
one does not. The buffer got neither `WGPUBufferUsage_Vertex` nor the
storage usage, and `set_vertex_buffers` refused it outright. A vertex state
*is* a vertex buffer with a layout, so both places now accept either bind.

**`caps->alpha_test` defaults to true.** The cap table's default says the
driver implements GL's alpha test; `gex_wgpu_create_dsa_state` refuses any
state that enables it. So st/mesa sent the state, the refusal left `ctx->dsa`
NULL, and the draw ran with the wrong depth and stencil configuration and no
alpha test either. Setting the cap false makes st lower the test to a
`discard` in the fragment shader, which is both what WebGPU can run and what
GL specifies. Minecraft found this; core profile removed alpha test, so
nothing in the browser had reached it.

**A sampled unit with nothing bound lost the whole draw.** GL allows this and
defines it -- an incomplete texture samples as `(0,0,0,1)` -- and a GL 1.x
renderer leaves units enabled as a matter of course. WebGPU requires every
declared binding to be present, so the driver declined. It now binds a 1x1
opaque-black stand-in, cached per (view dimension, sample type) so it matches
whatever the module declared; `gex_wgpu_default_texture_view` and
`gex_wgpu_default_sampler`. This was costing Minecraft the single draw that
composites its framebuffer onto the screen -- the entire game, every frame.

**`texture_written` was only set on the non-flipping copy path.** Every
`glCopyTexSubImage2D` from a window is flipped, so the row-by-row branch left
its destination looking untouched and the "samples a texture nothing ever
wrote" diagnostic pointed at a texture that had in fact been written. A
misleading diagnostic costs more than none; the mark is now above the branch.

**Window-system binds were refused.** `PIPE_BIND_DISPLAY_TARGET`,
`SCANOUT` and `SHARED` mean "a frontend will present this", not anything
WebGPU has an opinion about. A browser build never sees them. Refusing them
left drisw with no usable visual: "failed to create drisw screen", before a
single GL call.

**The native device must ask for what the page asks for.** Left at the
defaults it reported **GL 2.1**: `ARB_depth_buffer_float` and
`EXT_packed_float` are GL 3.0 requirements and each is one optional WebGPU
feature. Requesting the same nine features and the same raised limits the
browser does gives GL 3.3 core, matching it exactly.

One bug was in the new file rather than the driver, and it is worth knowing
the shape of: `displaytarget_create` hands back **the winsys's own row
stride**, which is not `width * 4` because X11 pads. Recomputing it sheared
the image one row at a time. The tell was a picture with exactly the right
pixel histogram -- mean 0.073 against softpipe's 0.074, the same 10 distinct
colours -- and completely the wrong arrangement.

### Minecraft 1.8.8

Renders correctly: main menu with the blurred panorama, world selection,
in-world terrain and water, the block-selection outline, the hotbar with item
icons and counts, the held item, chat, and the F3 overlay (which reports
`gex WebGPU (bring-up)` and `3.3 (Compatibility Profile)`).

Driving it headless needs a window manager -- `Xvfb` plus `openbox`. Without
one, `xdotool windowactivate --sync` never returns and the game never receives
keyboard focus. `scratchpad/mc-play.sh` clicks through to the world and runs
chat commands. Note `pkill` in this environment returns a status that kills the
calling tool chain; read `/proc` instead.

### The performance ceiling, measured

`GEX_WGPU_TIMING=1` (off by default) splits a draw's wall clock. glxgears at
600x600:

```
496 us/draw: bounds 0, fs-module 0, layouts+bindgroups 10,
             pipeline 83, views 6, record 307, submit 89, rest 1
```

`record` is `BeginRenderPass` through `wgpuCommandEncoderFinish`. Almost all of
the 496 us is the cost of the pattern -- **one render pass, one command buffer
and one queue submit per draw** -- and none of it is the driver's own
bookkeeping. That caps everything at roughly 2000 draws per second: glxgears
at 12 draws a frame gets 120 fps, Minecraft at ~1400 gets 1.

It is not fill rate. 300x300 to 900x700 is seven times the pixels for 14% less
throughput.

The fix is to batch consecutive draws sharing a framebuffer into one pass and
one command buffer, re-beginning only when something forces it: a framebuffer
change, a clear, a copy or blit, a resource map, a query result, or a present.
That list is the whole risk -- getting it wrong reorders work rather than
merely slowing it -- so it is a project, not a patch. Caching the pipeline
alone buys 17% and is not worth doing on its own.

## Three of the GL 4.0 extensions, and a smaller rung than it looked (2026-09-02)

GL 4.0 was recorded here as seven missing extensions, which made it read as
seven pieces of work. Two of them are not work at all in that sense, and
finding that out changed what the rung costs.

**`ARB_gpu_shader5` is not gated on a driver cap.** `st_extensions.c:1257` is
the whole gate:

```c
if (GLSLVersion >= 400 && !options->disable_arb_gpu_shader5)
   extensions->ARB_gpu_shader5 = GL_TRUE;
```

**`ARB_gpu_shader_fp64` cannot be enabled before that either**, which explains
an earlier failed attempt. Mesa builds the software fp64 library only under

```c
if (_mesa_is_desktop_gl(st->ctx) && st->ctx->Const.GLSLVersion >= 400)
   st->ctx->SoftFP64 = glsl_float64_funcs_to_nir(st->ctx, options);
```

(`st_glsl_to_nir.cpp:496`), and `nir_lower_doubles` asserts the library exists
(`nir_lower_double_ops.c:690`). Setting `caps.doubles` at GLSL 3.30 therefore
does not produce fp64, it produces that assert. `glsl_feature_level` is the
driver's own cap so this is not a true circle, but fp64 arrives *with* the
rung and never before it. Raising `glsl_feature_level` early would advertise
every GLSL 4.00 feature, `interpolateAtOffset` included, whether or not it
works -- so it is the last thing to do, not the first.

That leaves three pieces of real driver work for GL 4.0:
`ARB_tessellation_shader`, `ARB_texture_buffer_object` (+`_rgb32`), and the
transform feedback pair. The pair, and `ARB_texture_query_lod`, are done.

### `ARB_texture_query_lod`: computed, because Tint refuses to be asked

WGSL has no `textureQueryLod`, and SPIR-V's `OpImageQueryLod` is one of the
few opcodes Tint rejects outright:

```
error: WGSL does not support querying the level of detail of an image:
%28 = OpImageQueryLod %6 %27 %20
```

`nir_get_texture_lod` is no help -- it builds the same query rather than
computing one. So `gex_wgpu_lower_query_lod_instr` computes it the way the
specification defines it: scale the coordinate by the texture size, take
`ddx`/`ddy`, and the larger of the two lengths is the scale factor whose
`log2` is the level. `.x` is that clamped into the mip chain and `.y` is the
raw value, which is the only reason the query returns two components.

The instructive part was *where* the pass goes. It started in
`gex_wgpu_lower_texture_variant`, which only runs for shaders that need a
sampler variant -- so a shader whose only unusual feature was the query went
straight to SPIR-V and Tint refused it, with the pass never running. Nothing
in the lowering depends on the bound view, so it belongs in the base compile
next to the other shader-wide lowerings.

`gl44-query-lod-probe` checks it against arithmetic rather than a flag. A
full-screen quad over an NxN viewport moves a 0..1 coordinate by `1/N` per
pixel, which across an S-texel texture is `S/N` texels per pixel, so the
answer is `log2(S/N)` and the viewport chooses it. The cases are picked to
separate a correct lowering from the plausible wrong ones: one that forgets
the texture size fails all of them, one that returns the clamped value for
both components passes the minification cases and fails the magnified one
where the two genuinely differ, and one that clamps at the wrong end fails it
too. Results are read from an `RGBA32F` attachment, because at 8 bits a level
is a few codes wide -- an early 8-bit version of this test read 2.008 for an
answer of exactly 2, which is the encoding and not the driver.

### `ARB_transform_feedback2`: one number, three features

Pause/resume, the explicit append offset, and `glDrawTransformFeedback` are
the same state seen from three directions: how far into each bound buffer the
last capture got. Gallium passes it through the `offsets` argument of
`set_stream_output_targets` -- a byte count starts a capture there, `~0` means
carry on -- and `_mesa_PauseTransformFeedback` simply unbinds every target
while resume rebinds with `~0`. So the running total lives on
`gex_wgpu_so_target`, not on the context, or pausing would lose it.

`glDrawTransformFeedback` is where this driver gets something for free. On
real hardware the recorded count lives on the GPU, because only the GPU knows
how much fitted, and drawing from it needs a counter buffer or a stall. Here
the capture is a compute dispatch whose size the driver computes on the CPU
before recording it -- `writable_prims`, already clamped to what the buffer
had room for -- so the count is just `written / stride` and there is nothing
to read back.

### `ARB_transform_feedback3`, and the bug it uncovered

`gl_NextBuffer` splits one interleaved capture across several buffers. The
linker expresses that entirely in the xfb info the capture pass already
walked, so the extension itself needed no new machinery -- but turning it on
failed immediately, with buffer 0 correct and buffer 1 untouched.

The cause is that two `float` varyings do not get a varying slot each. The
linker packs them into components of one, so `a` and `b` both arrive as
location 32 and are told apart only by `location_frac`:

```
gex-xfb: so_mask=0x1 outputs=2 written=0x3
gex-xfb:   out[0] loc=32 buffer=0 offset=0 mask=0x1 stride=4
gex-xfb:   out[1] loc=32 buffer=1 offset=0 mask=0x2 stride=4
```

`gex_wgpu_capture_store` matched an output to a store by location alone and
stopped at the first hit, so both stores found `out[0]` and buffer 1 was never
even claimed -- note `so_mask=0x1`. It now matches on the component mask as
well, keeps going through every output the variable overlaps, and maps a
component to its byte offset by its rank among the set bits below it rather
than by its index within the variable. Those are the same number only when
the variable starts at component zero and the output covers all of it.

The plain interleaved case -- the same two varyings captured into one buffer
-- was *not* affected, which is worth recording because it is the natural
assumption and it is wrong. Reverting the fix and re-running confirmed it:
`xfb3_next_buffer` fails, `xfb3_packed_interleaved` still passes. Those two
varyings keep separate slots when nothing splits them, so the first-match
shortcut happened to be right. The interleaved case is in the probe as a
control against a fix to the packed path that breaks the ordinary one.

`GEX_WGPU_XFB_DEBUG=1` prints the block above, which is what found this.

### How the three were checked

Each extension got a probe that checks arithmetic rather than a flag, on the
principle established with sample shading: an extension that is advertised but
does not work is worse than one that is absent. All three probes were run
first against native Mesa (llvmpipe), whose answers are the reference, then
against `gex_wgpu` on Dawn, then through `tests/gl44-probe.sh` in the browser,
which builds the same source for the guest and requires the same `SUMMARY`
line. Every case matched llvmpipe exactly, including the two that separate a
correct implementation from a plausible one -- the magnified LOD case, where
the clamped and raw values differ, and `gl_NextBuffer`, where one buffer stays
empty if the output matching is wrong.

Two full `KHR-GL33` sweeps were run against the recorded baseline, one with
`texture_query_lod` alone and one with the transform feedback pair added:
**zero regressions both times**, 32 shared families and 1612 passing cases
unchanged. `caps->texture_query_lod` was deliberately left off until the
lowering was verified, for the same reason.

## The bind group cache never hit (2026-09-02)

`GEX_WGPU_TIMING=1` on Minecraft with a crowd reported:

```
cache hit rate -- bind groups 0.0% (46987974), pipelines 99.4% (90743)
```

Not a low hit rate: zero, across forty-seven million lookups. Every draw built
a bind group Dawn had already been given.

The cause is that `cache_epoch` is global and every slot is validated against
it -- a hit needs `slot->epoch == screen->cache_epoch` -- while the *eviction*
path incremented it. So evicting any one of the 512 slots invalidated all the
others, and a workload that evicted at all could never hit again. The pipeline
cache's eviction path bumped it too, which was pure collateral damage: that
cache does not consult the epoch at all, so the bump could only harm the bind
group cache it shared the counter with.

The epoch is still needed, but only where it was actually earned. A key names
its buffers, views and samplers by address, and Dawn hands a freed address
straight back, so a stale key could match the wrong object. What makes that
impossible between neighbours is that each slot holds a reference to its own
group, which holds one to everything its key names -- so nothing a live slot
names can be freed underneath it, whatever happens to the slot next door. The
one thing that outruns the reference is a driver-level sampler or view being
destroyed, and those two paths still bump the epoch. Eviction never needed to.

`KHR-GL33` confirms the guard is intact: zero regressions across 32 families
and 1612 cases, with `texture_repeat_mode` -- the family that fell from 162 to
158 when this class of bug last appeared -- back at a full 162/162.

### What it was worth, measured rather than assumed

Matched runs of the same scene at the same 1429 draws/frame, the fix being the
only difference:

| | eviction bump present | removed |
|---|---|---|
| frames/s | 54.4 -- 56.2 | 57.6 -- 62.4 |
| draws/s | 77.7k -- 80.2k | 83.9k -- 92.8k |
| bind group hits | 0.0% | 99.6% |

About 8%. Worth having, and much less than it looked like it should be --
Dawn builds bind groups cheaply enough that forty-seven million redundant
creations cost single digits.

The measurement matters as much as the fix. An earlier reading of "19,319
draws/s at 645 draws/frame" was taken from the same script while the scene was
still loading, and comparing the fixed build against *that* suggested a 4.5x
improvement. It was not a baseline, only a different scene. Any claim about
this driver's frame rate needs the draws/frame printed next to it, or it is
comparing two different pictures.

## SuperTuxKart's empty scene, and three smaller faults (2026-09-02)

STK's race scene rendered as a HUD floating over nothing: the timer, the kart
icons and the speedometer ring drew, and the world did not. Behind it were
254,456 instances of

```
[Invalid RenderPipeline (unlabeled)] is invalid.
```

and six of

```
The component count (4) of the vertex output at location 0 is different from
the component count (3) of the fragment input at location 0.
```

### Varying widths have to match, and in GLSL they need not

GLSL is content for a fragment shader to read `vec3 v` from a vertex shader
that writes `vec4 v`; the fourth component is simply never read. WGSL requires
the two sides of an interstage variable to be the same type, and Dawn rejects
the entire pipeline when they differ. That is a validation error rather than a
failed call, so the draw is dropped in silence -- six shader pairs were enough
to erase the world.

The driver already had the machinery for this shape of problem, built for
interpolation qualifiers: the fragment stage is authoritative, and a vertex
variant is compiled to agree with it. `varying_components` now rides alongside
`varying_interp` in exactly the same way, and `gex_wgpu_fit_varyings` reshapes
the clone's outputs -- narrowing drops components nothing can read, widening
pads with zero, which is a value GL leaves undefined anyway.

One trap in doing it. A `store_deref` carries its own `num_components`, and
`nir_src_rewrite` does not update it, so trimming the value left a store
claiming four components while holding three. `nir_validate` catches that and
aborts; a release build would not have.

### gl_PointSize took the driver down with it

Found while writing an unrelated probe. `gex_wgpu_strip_point_size` drops the
store -- WGSL has no point size and Tint accepts only a constant 1.0 -- and
then unlinks the variable, but the deref that addressed it survived:

```
%3 = deref_var &gl_PointSize (shader_out highp float)
     error: entry (nir_validate.c:323)
```

A variable removed from under a live deref is a dangling reference, and that
aborts the process rather than mis-compiling. DCE before removing the variable
takes the deref with the store. Any shader that set `gl_PointSize` used to
crash the driver.

### flatshade_first was refused backwards

The rasterizer allowlist turned away `flatshade_first`, and a refused
rasterizer state binds as NULL -- no culling, no scissor, `front_ccw = 0`.
That is the wrong way round: WGSL's flat interpolation takes the *first*
vertex, so GL's FIRST_VERTEX_CONVENTION is the case that can be honoured
exactly, while GL's default of the last vertex is the one that cannot and was
being accepted quietly. Now accepted.

### What STK still gets wrong, and how to look at it

The scene renders -- terrain, sky, mountains, the banner -- but the kart, the
minimap, a rock face and the track borders do not, and the speedometer digit
comes out a flat red block.

**Do not compare against llvmpipe as it comes.** llvmpipe reports GL 4.6, and
STK reads the version and picks a different renderer: compute shaders, SSBOs,
image load/store, and TBO skinning. Against that, half the difference on
screen is STK running a different code path, not this driver rendering wrongly.
Hold it to the same rung:

```
MESA_GL_VERSION_OVERRIDE=3.3 MESA_GLSL_VERSION_OVERRIDE=330 \
LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe supertuxkart ...
```

llvmpipe at GL 3.3 renders the scene correctly and completely, so GL 3.3 is
sufficient and the remaining fault is this driver's.

The useful fact for whoever picks this up: **both drivers are handed the same
work.** `GALLIUM_TRACE` on llvmpipe at 3.3 gives 216,911 `draw_vbo` calls over
5,305 `flush_frontbuffer`, or 40.9 draws a frame; this driver's own
`GEX_WGPU_TIMING=1` reports 41 draws a frame on the same scene. Nothing is
being culled or dropped before the driver -- 41 draws arrive and render less
than they should, which is a much smaller search than it first looked. The
frame is 32 indexed draws of 6 to 6258 vertices and 9 non-indexed draws of
three or four, into a single framebuffer that is set three times in the whole
run.

Four hypotheses, each plausible and each wrong, recorded so they are not paid
for twice:

* **Backface culling.** Forcing `WGPUCullMode_None` changes nothing.
* **Texture buffer objects.** STK logs "ARB Texture Buffer Object Present" and
  skins through one on llvmpipe, and `samplerBuffer` shaders *are* rejected
  here ("invalid or duplicate texture binding") -- but STK takes its
  `SKINNING_TBO_DISABLED` path against this driver and no shader is rejected
  in the whole run. Real gap, wrong culprit.
* **RGBA32F `texelFetch`**, which the fallback skinning path reads its bone
  matrices from. Works, via both `glTexImage2D` and `glTexSubImage2D`.
* **Instancing.** The trace's instance counts are 1 to 4; STK at this rung
  barely instances at all.

Also worth knowing: the driver dumps the first 24 distinct vertex shaders it
sees, unconditionally, with each one's input mask and `missing` mask. For STK
every shape reports `missing 0x0`, so no draw is collapsing for want of an
attribute. STK uses eleven vertex shaders in all.

### Narrowing it with a draw bisection

`GEX_WGPU_DRAW_LIMIT=n` keeps the first n draws of every frame and
`GEX_WGPU_DRAW_ONLY=n` keeps only draw n, counted at the outermost `draw_vbo`
and reset at each present. With a frame of a few dozen draws that turns "which
draw puts nothing on screen" into a bisection, which is the only way to ask it
here: the draws are submitted, their attributes are all supplied, no state is
refused and no validation error is raised, and they still produce no pixels.

What it says about STK:

* `GEX_WGPU_DRAW_LIMIT=4` and `=12` are **blank** -- not "missing an object",
  entirely empty. The first twelve draws of the frame contribute nothing.
* Those are the large meshes: from the trace, draws 0-11 carry 12, 6258, 2772,
  3558, 72, 96, 120, 210, 4683, 45, 18 and 1200 vertices.
* `GEX_WGPU_DRAW_ONLY=1` and `=17` are blank on a cleared framebuffer, with
  nothing else drawn that could hide them. So the geometry genuinely
  rasterises nothing; it is not being painted over, which also rules out the
  no-depth-test skybox pass covering the scene.
* `=41` is the full broken image with the HUD cut off, so the HUD is issued
  after draw 41 and the scene occupies the draws before it.

The conclusion is that **everything visible in STK's frame is the skybox and
the HUD**, and every mesh in the scene is being transformed out of the frame
or otherwise producing no coverage. That is the same signature the comment
beside the constant-buffer dump already describes: *"if it arrives wrong every
scene vertex leaves the frustum while the skybox -- one attribute, no depth
test, its own matrix path -- still lands."* The object shader reads vertex-stage
UBO slots 0 and 3.

Two more hypotheses eliminated while chasing that:

* **A stale UBO under batching.** `glBufferSubData` to a uniform buffer while
  draws sit recorded-but-unsubmitted would let every batched draw read the
  last matrix written. It does not happen: `gex_wgpu_buffer_subdata` flushes
  the batch through `gex_wgpu_batch_conflicts_usage`, and both stages' UBOs
  are registered with `gex_wgpu_batch_touch`.
* **A rejected UBO binding.** `set_constant_buffer` refuses an offset that is
  not 256-byte aligned, but the driver advertises
  `constant_buffer_offset_alignment = 256`, so st/mesa never hands it one --
  and the refusal is loud, while STK's run logs none.

The obvious next step is to see the matrix. That is harder than it looks:
STK's UBO has no CPU shadow, because it is only ever written through
`gex_wgpu_buffer_subdata`, whose aligned fast path sends bytes straight to the
queue and deliberately keeps no copy. Excluding `PIPE_BIND_CONSTANT_BUFFER`
from the staged-map path does not help -- the buffer is never mapped at all.
Reading it back on the GPU inside the dump, or teaching `buffer_subdata` to
keep a shadow behind a debug flag, is what it will take.

(The comment on the staged-map condition claims "Index and constant buffers
keep the shadow", and the condition below it only excludes index buffers. The
comment in `gex_wgpu_buffer_subdata` is the accurate one: nothing reads a
constant buffer's shadow, and its pages are where the memory was. The
staged-map comment is stale, not the code.)

### The scene was the staged write-map path

`GEX_WGPU_NO_STAGED_MAP=1` -- added to answer a different question -- renders
STK's scene completely and correctly, matching llvmpipe at GL 3.3. So the
fault is in the staged write map, and the bisection above is explained: every
mesh in the scene comes from one buffer written that way, while the skybox and
the HUD do not.

STK maps it like this, and this is the only staged map in the whole run:

```
staged map usage=0xa2 WRITE DISCARD_WHOLE UNSYNC bind=0x10 range=0+180640
```

A whole 180 KB vertex buffer, rewritten per object. `PIPE_MAP_DISCARD_WHOLE_-
RESOURCE` is how an application asks for **buffer renaming**: give me this
buffer again and do not make me wait for whatever is still reading the old
one. A driver honours it by handing over a fresh allocation while the previous
one lives on for the work that still references it. This driver does not
rename, and serving the map from a staged block instead is where the two part
company.

So the staged path is now taken only for `PIPE_MAP_DISCARD_RANGE`, which is
the pattern it was written for -- u_upload_mgr hands out a fresh region of a
large buffer and fills it, and a fresh block to fill is exactly right for
that. STK renders correctly, and Minecraft's crowd scene is unaffected: 60-67
fps at 1429-1538 draws a frame, 192 draws a submit.

**The mechanism is still not understood, and the fix is scoped rather than
diagnosed.** What was ruled out, with measurements:

* **Lost bytes.** Instrumented against the buffer's previous contents, the
  caller leaves nothing behind that it needed: 0 bytes of 180640 are zero in
  the staged block where the old buffer held something. The block is 80-99.9%
  non-zero and the last byte written is at the very end of the range.
* **Write-versus-recorded-draw ordering.** A flush at unmap whenever the
  pending batch reads the buffer never fires once in an STK run -- at each
  unmap the batch holds no draw reading it. Worth knowing anyway: that flush
  ignores `PIPE_MAP_UNSYNCHRONIZED`, and on Minecraft it fires constantly and
  costs half the frame rate (33 fps, 3.5 draws a submit, against 66-70 and
  187). It was reverted.
* **An unconditional submit at map time**, which is the side effect
  `NO_STAGED_MAP` has through `gex_wgpu_shadow_for_read`. Does not help.
* **The shadow's mere existence.** Forcing a shadow *and* keeping the staged
  path leaves the scene broken, so it is not something else reading the
  shadow.

Two real bugs were found while eliminating those, and both are kept:

* `gex_wgpu_batch_conflicts` consulted a per-resource `in_batch` flag that
  `gex_wgpu_batch_touch` stops setting once its array is full, and the
  overflow flag it set was never read -- so in a frame busy enough to
  overflow, a write to a resource the batch really does read reported no
  conflict. Overflow now means "assume everything conflicts", and the array
  is sized to a full batch (4096) so overflow is the backstop it was meant to
  be: at 256 it overflowed constantly, and the conservative path then halved
  the frame rate.
* The staged path uploads the whole mapped range out of a zeroed block, so it
  needs the caller to have undertaken to write all of it. A plain
  `PIPE_MAP_WRITE` promises nothing of the kind.

### Chasing the mechanism further, and where it now stands

`GEX_WGPU_STAGE_WHOLE=1` puts the old behaviour back, so both the working and
the failing configuration are reachable from one build and can be compared
directly. An objective check beats looking at screenshots: count reddish
pixels in the lower middle third, where the kart is. Roughly **1580 means the
scene rendered and 555 means it did not** -- the broken frame still scores
because the timer and speedometer are red.

`GEX_WGPU_UPLOAD_LOG=<skip>` checksums every buffer upload, skipping the first
`<skip>` of them. The skip matters: the first attempt logged only the first
sixty uploads, which are all loading, concluded the two paths were identical,
and had never looked at a single per-frame rewrite. Sampling from 3000 shows
what actually differs -- the staged buffers in the failing configuration are
**44-byte per-instance blocks**, which is STK's `stride=44 divisor=1`
attributes: `i_origin`, `i_rotation`, `i_scale`. The shader builds position
from those, so a wrong `i_scale` collapses an object to a point, which is
exactly the symptom.

They are not wrong. Dumped as floats, both configurations upload the same sane
values -- origin around (-198, 0.05, 31.7), a unit quaternion, scale 0.008.
And reading the buffers back off the GPU at draw time gives **byte-identical
checksums across all eight slots in both configurations**, with the same
instance values.

So: the same bytes are in the same buffers when the draw runs, the bindings are
the same, and the pixels differ. Eleven mechanisms are now eliminated with
measurements -- the four from before, plus:

* **Source lifetime.** Keeping the staged block alive instead of freeing it
  after `wgpuQueueWriteBuffer` changes nothing, so the copy really has been
  taken by the time that returns.
* **Writing past the block.** The shadow path hands out a pointer into the
  resource's whole allocation, so a caller overrunning its mapped range would
  corrupt the heap only in the staged case. Over-allocating the block by 4 KB
  changes nothing.
* **Buffer renaming.** There is none: `DISCARD_WHOLE_RESOURCE` appears nowhere
  else in the driver, so `gex->buffer` is stable across the map.
* **Redundant-bind elision.** `SetVertexBuffer` is skipped when the handle and
  offset match what the pass already bound, which is the same
  recycled-address shape as the bind group cache bug. Forcing every bind
  changes nothing.
* **Synchronisation.** A full round trip to the GPU after the write -- which is
  what the shadow path gets for free on its first map, through
  `gex_wgpu_shadow_for_read` -- changes nothing. Nor does submitting the
  pending batch at map time.

That last one is the important negative, because it also indicts the
measurements: **every CPU-side probe that observes the GPU's state also
synchronises it**, and the one that read the buffers back is therefore not
observing the failing state. The readback says the bytes are right; it cannot
say they were right before it forced a sync.

So the next step is not more instrumentation inside the driver. It is a
non-perturbing capture of what Dawn is actually told -- its own API trace or a
capture layer -- run once in each configuration and diffed. Everything
reachable from inside the driver has been tried.
