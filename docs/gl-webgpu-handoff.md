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

> **Superseded.** The scoping described here was the right first move but it
> was not the cause. See "The mechanism, found" at the end of this section --
> it is a one-line invariant violation, and with it fixed the optimisation
> below is restored in full.

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

That conclusion was wrong, and usefully so: it confused "every probe that
reads GPU state synchronises" with "nothing inside the driver can see this".
What had never been written was a log of the *commands* -- which is pure
`fprintf` and perturbs nothing at all. That is what found it.

### The mechanism, found

`gex_wgpu_shadow_for_read` only reads a buffer back off the GPU when it
believes the buffer holds something:

```c
const bool needs_contents = gex->bytes_written != 0 || gex->gpu_written;
```

Two paths in this driver write a buffer while bypassing its shadow.
`gex_wgpu_buffer_subdata`'s direct path keeps the invariant that makes the
line above true -- it counts the bytes it wrote. **The staged map upload did
not.** So a buffer written only through staged maps looked empty, and the next
thing to need a shadow was handed a freshly *zeroed* one instead of the
buffer's real contents.

`transfer_flush_region` then uploads **from** the shadow -- 291,926 times in a
SuperTuxKart run, because that buffer is how STK streams its scene meshes. It
wrote those zeros back over the mesh. Every vertex collapsed onto the origin,
so the scene did not rasterise, while the skybox and the HUD -- on other
buffers -- still did.

The fix is one line in each of the two staged upload sites:

```c
gex->bytes_written += (uint64_t)transfer->box.width;
```

With it, `PIPE_MAP_DISCARD_WHOLE_RESOURCE` is staged again and the memory
optimisation is back in full. STK renders correctly and Minecraft's crowd
scene runs at 64.6-69.2 fps, better than with the scoping workaround.

### Why it took so long, which is the transferable part

Nothing about the upload was ever wrong. That is why checksumming every
upload, reading the buffers back off the GPU at draw time, and diffing the
entire command stream all showed two identical runs -- the corruption happened
*later*, from a different code path, sourcing from a copy that had quietly
been declared empty.

Three of the instruments were actively misleading, and each in a way worth
recognising again:

* **A window that misses the event.** The first upload log covered the first
  sixty uploads, which are all loading, and concluded the two paths were
  identical without having looked at a single per-frame write.
* **Normalisation that erases the signal.** Diffing two runs meant mapping
  pointers to symbolic ids -- and `sed 's/0x[0-9a-f]*/P/g'` collapses *every*
  pointer to one token, so "a different buffer is bound" and "the same buffer
  is bound" look identical. Comparing by stable identity (a buffer's
  `width0`, a texture's size and format, a dynamic offset's value) is what
  makes a cross-run diff mean anything.
* **A probe that changes what it measures.** Reading a buffer back requires a
  submit and a wait, so it reports the state *after* forcing a
  synchronisation, not the state the failing draw saw.

The bisection that actually cracked it asked a question none of those could:
staged with **no** shadow fails, staged with a **stale** shadow fails, staged
with a **current** shadow works. That isolates "something reads the shadow and
needs it up to date" in three runs, and from there the reader is one grep
away.

## Sweeping the 336 installed GL programs (2026-09-02)

With SuperTuxKart and Minecraft working, "and other GL apps" needed something
broader than the two applications to hand. There are **336 GL programs
installed** on this machine -- the mesa-demos and piglit-style suites, plus
aseprite, cemu, dolphin-emu, moonlight and mpv. Individually they are trivial.
Together they are the widest statement available about whether ordinary GL
works here, and they cost seconds each.

`tests/../scratchpad/gl-app-sweep.sh` runs each one for eight seconds and
classifies it by what the *driver* did, not by whether the program was happy:
CRASH, REFUSED (a state or shader this driver turned away), WGPU-ERROR (a
WebGPU validation failure), LATCHED, or RAN. Format probing --
"render target refused" for the dozens of formats st/mesa asks about at
startup -- is routine and deliberately not counted.

The first sweep:

```
RAN 192   REFUSED 60   CRASH 32   EXIT 22   WGPU-ERROR 14   OK 16
```

### Every crash was a consequence, not a cause

All 27 crash logs contained "already failed", and 14 ended in
`texture_map declined (context already failed)`. The shape is always the same:
something is refused, the context latches, `texture_map` returns NULL, and the
application dereferences it. So the crashes were not 27 bugs -- they were the
visible end of a smaller number of refusals.

Grouping the *first* refusal in each crashing log made that concrete: **17 of
27 began with a rejected SPIR-V**, and of the 30,661 Tint rejections across
all the logs, one message accounted for 30,656 of them.

### Vertex input locations collided

```
[VUID-StandaloneSpirv-OpEntryPoint-08721] Entry-point has conflicting input
location assignment at location 0, component 0
  OpEntryPoint Vertex %main "main" %VERT_ATTRIB_POS %VERT_ATTRIB_COLOR0
                                   %VERT_ATTRIB_GENERIC0 ...
```

`ntv_shader_prepare` rewrites every vertex input's `driver_location` -- which
becomes the module's `@location` -- as

```c
location >= VERT_ATTRIB_GENERIC0 ? location - VERT_ATTRIB_GENERIC0 : location
```

and those two ranges overlap. `VERT_ATTRIB_POS` is 0 and
`VERT_ATTRIB_GENERIC0` is 16, so a shader using a fixed-function attribute
together with a generic one declares two inputs at `@location(0)` and the
SPIR-V is invalid before Tint sees it. st/mesa builds exactly that shape for
its own fixed-function work, so `glDrawPixels`, `glCopyPixels` and the FBO
clear paths all produced a rejected vertex shader.

st/mesa's own numbering does not collide: it is dense from zero, and it is
what Gallium's vertex elements are indexed by -- so it is also the numbering
the pipeline's `shaderLocation` uses. Keeping it across
`ntv_shader_prepare` makes both sides agree by construction instead of by a
formula the driver had to mirror (and whose collision it inherited).

**21 programs fixed by that one change, no regressions**: arbocclude,
arbocclude2, bump, clear-fbo, clear-fbo-scissor, clear-fbo-tex, copypix,
drawpix, engine, fbo_firecube, fire, fogcoord, ipers, lodbias, pixeltest, ray,
readpixels, teapot, terrain, texenv, tri-fbo.

With glPolygonOffset below, **26 in total**, and the sweep moves to:

```
RAN 217   REFUSED 54   CRASH 17   EXIT 22   WGPU-ERROR 10   OK 16
```

`moonlight` looks like a regression in the tally and is not: its log already
contained thirteen "Failed to create ... context" lines before the change. It
fails to create a Qt RHI context either way, and only changed verdict because
on a quiet machine it now reaches its own abort inside the eight-second
window.

### The rasterizer refusals, decoded

28 of the 60 refusals were `rasterizer state refused`, which reports the
differing byte and value rather than a field name. Decoding those against the
bitfield layout of `pipe_rasterizer_state` gives the real list:

| refused | programs | representable in WebGPU? |
|---|---|---|
| `fill_front`/`fill_back` (polygon mode LINE/POINT) | 10 | no polygon mode; would need triangles rewritten to lines |
| `offset_tri` (glPolygonOffset) | 6 | **yes -- depthBias, done** |
| `light_twoside` | 3 | fixed-function two-sided lighting |
| `line_smooth`, `line_stipple`, `poly_stipple` | 4 | no |

### glPolygonOffset

WebGPU takes it directly, so this was a refusal of something fully
representable -- and a refused rasterizer state binds as NULL, costing culling
and the scissor along with it. GL adds `factor * m + units * r`, WebGPU's
`depthBias` counts in units of r and `depthBiasSlopeScale` multiplies the
slope, and Gallium halves units unless `offset_units_unscaled` says otherwise,
so it is doubled back -- the same correction zink applies.

One WebGPU constraint to respect: **depthBias must be zero for a line or point
topology**, and a pipeline that sets it anyway is rejected outright. That
matches GL, where polygon offset applies to polygon rasterization; but an
application that leaves `GL_POLYGON_OFFSET_FILL` enabled while drawing lines is
ordinary, and `offset` does exactly that. The bias is applied only for
triangle topologies.

`gl44-polygon-offset-probe` checks it by arithmetic rather than by acceptance:
two coplanar quads under `GL_LESS`, where the second can only win if it was
pulled towards the viewer and can only lose if it was pushed away. The third
case is the control -- offset disabled, equal depth, second quad rejected --
without which a driver that ignored the offset entirely would still pass the
first case.

### The crashes that were left, and gl_FrontFacing

After the location fix, 17 crashes remained. Triaging them the same way --
what did the driver refuse first -- said **15 had no driver refusal at all**,
and reading their own output explains why: `weston-simple-*` and
`weston-subsurfaces` want a Wayland display, `hyprland` and
`looking-glass-client` are not GL demos, `eglkms` wants KMS, `glxpixmap`,
`MaterialXView` and `moonlight` fail GLX/EGL context creation, and `glsync`
prints "GLX_SGI_video_sync not supported, exiting" and leaves. None of those
is this driver's.

Two were. `twoside` died inside the borrowed SPIR-V emitter:

```
nir_to_spirv.c:1821: get_alu_src: Assertion `!"bool should have bit-size 1"' failed
```

**A wide boolean reaching get_alu_src.** NIR's own booleans are 1-bit, but a
TGSI-derived shader carries 32-bit ones and narrows them with `b2b1` -- and
`emit_alu`'s case for that op already knows how to read a wide source,
comparing it against zero. It never got the chance, because `get_alu_src`
runs first and aborted. It now hands the raw value back and lets the op
convert, which is the same trade the `b2b1` case was written for: a shader
Tint refuses fails one program, an abort takes the whole application down.

**gl_FrontFacing was declared uint.** Past that abort, the same shader could
still not link:

```
[VUID-FrontFacing-FrontFacing-04231] According to the Vulkan spec BuiltIn
FrontFacing variable needs to be a bool scalar.
  %gl_FrontFacing = OpVariable %_ptr_Input_uint Input
```

`tgsi_to_nir` represents `TGSI_SEMANTIC_FACE` either as `nir_load_front_face`
or as an ordinary fragment input variable, and the choice is
`screen->caps.fs_face_is_integer_sysval`. Left false, the emitter decorated
that input variable `BuiltIn FrontFacing` while giving it the variable's own
uint type, which SPIR-V does not allow. zink sets the cap true, and this
driver borrows zink's emitter, so it should have too. With it set the shader
reaches `emit_load_front_face`, which builds the builtin as a bool.

st/mesa builds two-sided lighting through ureg, so this was the whole of
`twoside`. It also removes the `light_twoside` rasterizer refusals: with
FrontFacing available as a system value, st/mesa resolves two-sided lighting
in the shader and stops asking the rasterizer for it.

The cap also sets `GLSLFrontFacingIsSysVal`, so it changes the GLSL path as
well as the TGSI one -- which is why it is worth more than the one program it
was found by, and why it wants the CTS sweep rather than a spot check.

### Where the sweep stands, and one trap in the harness itself

Across the 336 programs, over the four fixes above:

| | start | after locations + polygon offset | after gl_FrontFacing |
|---|---|---|---|
| RAN | 192 | 217 | 219 |
| CRASH | 32 | 17 | **15** |
| REFUSED | 60 | 54 | 54 |
| WGPU-ERROR | 14 | 10 | 10 |

**27 programs** moved out of a failing verdict. Note what the last column does
and does not claim: the FrontFacing pass genuinely fixed `twoside`, and
`moonlight` changed verdict without being fixed at all -- it still cannot
create a Qt RHI context, it merely no longer reaches its own abort inside eight
seconds. A program that fails for its own reasons will drift between verdicts
run to run; only a change of *reason* is a fix.

The harness itself produced a false result worth recording. Moving it from a
scratch directory into `tests/` broke its path to the driver environment, every
program exited 127, and because "exited non-zero" is not one of the driver
verdicts, the comparison reported **81 programs fixed** -- a total failure
reading as the best result of the day. It is now self-contained (it exports the
driver environment itself and refuses to start without a driver and a display),
and exit 127 has its own `HARNESS` verdict so it can never again be mistaken
for progress. Three programs legitimately land there: `dotewm` wants a
libGLEW this system does not have, and `qs`/`quickshell` have unresolved Qt
Wayland symbols.

Of the 15 remaining crashes, 13 are not this driver's -- Wayland compositors,
KMS, GLX context creation -- leaving `dlist-degenerate` (crashes before any GL
work) and one shader that still cannot split a combined sampler. The 54
refusals are dominated by polygon mode LINE/POINT (10 programs, which would
need triangles rewritten to line topology) plus line and polygon stipple and
line smoothing, none of which WebGPU has.

## glPolygonMode: the polygon rewritten as its edges (2026-09-02)

The largest remaining group in the sweep was `fill_front`/`fill_back` -- ten
programs asking for `glPolygonMode(GL_LINE)` or `(GL_POINT)`. WebGPU has no
polygon mode, and refusing the state bound **no rasterizer state at all**, so
those programs lost culling and the scissor as well as the wireframe: they drew
filled *and* wrong.

Mesa already has the rewrite. `u_unfilled_translator` and
`u_unfilled_generator` turn a polygon into the lines or points it would be
drawn as, and using them rather than open-coding it is what makes quads and
polygons come out right: taking each triangle's three edges after converting a
quad into two triangles would draw a diagonal across it that GL does not draw.
So the conversion runs **before** the topology rewrite, on the primitive the
application asked for, and comes back through `draw_vbo` as LINES or POINTS --
which are not polygons, so it does not recurse.

`gl44-polygon-mode-probe` checks the result by coverage rather than by
acceptance, because a driver that took the state and drew the triangle filled
anyway would look like it worked. Filled and unfilled agree with llvmpipe
exactly: **1770 of 4096 pixels lit filled, 183 unfilled**, same numbers on both.

Two limitations, both honest and still refused:

* **Front and back differing** (`fill=0/1`, which `pgon-mode` asks for). GL
  allows a different mode per face and nothing here could express it.
* **Line smoothing** (`smooth=0/1`, `tri-unfilled-smooth`). WebGPU has no
  antialiased lines.

And one fidelity gap that is accepted rather than refused: GL culls before
applying polygon mode, so a culled back face contributes no edges, while lines
carry no winding -- a wireframe drawn with culling enabled shows back-facing
polygons' edges too. That pairing is uncommon and the alternative is culling on
the CPU.

### A stale field made meaningful

The rewrite turns a non-indexed draw into an indexed one, and `index_bias`
means nothing on a non-indexed draw -- st/mesa leaves it set from an earlier
one. Copying the draw wholesale therefore handed a stale base vertex to a draw
where it counts, which the driver refuses outright ("draw refused (index
bias)") and which cost the whole context. A genuine base vertex on an indexed
source still has to apply, and the translator copies the source indices without
it, so it is folded into the generated indices instead -- widened to 32 bits
first, since a biased index need not fit the width the generator chose -- and
the field cleared.

### An external measurement trap

glxgears read 481 fps after this change against 835 before it, which looks like
a 40% regression and is not one. Another agent session was running headless
Chromium in a different checkout, holding the load average at 13-15 on twelve
cores. Under that load glxgears alone varied between 481 and 596 fps run to
run.

`GEX_WGPU_NO_UNFILLED=1` skips the new branch, which makes the question
answerable without a quiet machine: **on 655.9 and 651.6 fps, off 647.9 and
645.2**, interleaved under the same load. The branch is not a cost. Checking
`/proc/loadavg` and who owns the load is worth doing before believing any
frame rate here -- and the load may not be this session's to kill.

### Where the sweep stands after five fixes

| | start of the day | now |
|---|---|---|
| RAN | 192 | **235** |
| CRASH | 32 | **15** |
| REFUSED | 60 | **38** |
| WGPU-ERROR | 14 | **10** |

**43 of the 336 programs moved out of a failing verdict, and none moved into
one.** The RAN and CRASH columns are somewhat load-sensitive -- a program under
contention gets less far inside its eight seconds -- but REFUSED is a statement
about what the driver turned away and is deterministic, and that is 60 to 38.

Of the 15 remaining crashes, 13 are not this driver's (Wayland compositors,
KMS, GLX and EGL context creation), leaving `dlist-degenerate`, which crashes
before any GL work, and one shader that cannot split a combined sampler. The
38 remaining refusals are line and polygon stipple, line smoothing, logic ops,
front and back polygon modes differing, and 8-bit indices -- of which only
8-bit indices is straightforwardly implementable (convert to 16-bit), the rest
having no WebGPU equivalent at all.

## Four more from the sweep: byte indices, MSAA copies, point sprites (2026-09-02)

### 8-bit indices

`glDrawElements` with `GL_UNSIGNED_BYTE` is ordinary GL and WebGPU has no
index width for it, so the draw was refused and the context latched. They are
now widened to 16 bits before anything else looks at them, so the unfilled
rewrite, the restart split and the draw all see a width WebGPU takes. A
multi-draw is left alone and converted when the loop re-issues each of its
draws.

`gl44-byte-indices-probe` checks that the *right bytes* are widened, which a
test drawing triangle 0,1,2 would not: the buffer holds eight vertices, the
first five collapsed onto a point where they cover nothing, and the triangle
that covers anything is 5,6,7. It also asks for the same triangle through
`glDrawElementsBaseVertex` with indices 0,1,2 and a base vertex of 5, which
exercises the other half -- the base vertex has to be folded into the indices,
because a biased byte index need not fit sixteen bits and the driver refuses to
be handed one separately.

### A copy cannot change the sample count

```
Source [...] sample count (1) and destination [...] sample count (4) does not
match.
```

`gex_wgpu_blit` takes a copy fast path when the formats and sizes agree, and
sample count was not among the things it checked. Multisample to single was
already handled by resolving; this is the other direction, a single-sampled
image replicated into a multisampled target, which only a draw can do. It now
goes to `u_blitter` with the rest of the non-copy cases. Emitting the copy
anyway produced an invalid command buffer, and that is not one lost blit --
every later submit fails with it. `glpuzzle`, `pointblast` and `spriteblast`
all did this.

### gl_PointCoord, GL_COORD_REPLACE, and inputs nobody writes

Three related failures behind `The fragment input at location 4 doesn't have a
corresponding vertex output`, which is fatal in WebGPU and merely undefined in
GL.

`gl_PointCoord` first: ntv emits `SpvBuiltInPointCoord`, which WGSL has no
equivalent of. WebGPU rasterises a point as a single pixel -- something this
driver already accepts, since `gex_wgpu_strip_point_size` drops `gl_PointSize`
for the same reason -- and over one fragment the sprite coordinate is that
fragment's centre. So `nir_load_point_coord` lowers to `(0.5, 0.5)`, which is
not an approximation of a wider point but the exact answer for the point
WebGPU draws. `caps->fs_point_is_sysval` then asks st/mesa for the system value
rather than a varying with no producer, exactly as
`fs_face_is_integer_sysval` does for FrontFacing.

`GL_COORD_REPLACE` is the harder half. The rasteriser feeds a texcoord from
the point sprite, so the vertex shader does not write it -- and whether it
should be substituted depends on the *draw*, not on shader state: the same
rasterizer state stays bound across triangle draws that read the same texcoord
from the vertex shader for real, and substituting there would replace their
texture coordinates with a constant. So the fragment variant is keyed on the
sprite mask *and* on whether this draw is points.

`nir_lower_texcoord_replace` is not usable for it. That pass expects the
io-lowered form, and the NIR at this stage still addresses its inputs through
derefs: it reported progress while leaving both the loads and the variable in
place. The variable is what matters -- the emitter declares an input for every
`shader_in` variable there is, so the pipeline stayed invalid even once the
loads were gone. Chasing that took three rounds of "lowered=1, inputs
unchanged" before printing the variant's inputs rather than trusting the
return value.

What replaced it is one pass over the form actually present, doing both
substitutions: a coord-replaced texcoord gets `(0.5, 0.5, 0, 1)`, and an input
with no producer at all gets zero -- GL leaves such a varying undefined, so
zero is a conformant answer. Then DCE, then `nir_remove_dead_variables`, which
is the step that actually removes the input. Removing fragment inputs is safe
in this direction: WebGPU requires every fragment input to have a vertex
output, not the reverse, so an unread vertex output is harmless while an
unwritten fragment input is fatal.

That last part is general rather than sprite-specific, and it is the reason
`point-sprite` and `spriteblast` needed it: they read a texcoord before
enabling `GL_COORD_REPLACE`, so nothing produces it and nothing is meant to.

### The sweep, after nine fixes

| | start of the day | now |
|---|---|---|
| RAN | 192 | **241** |
| CRASH | 32 | **15** |
| REFUSED | 60 | **37** |
| WGPU-ERROR | 14 | **5** |

**49 of the 336 programs moved out of a failing verdict, and none moved into
one.**

What is left is mostly not implementable. The 15 crashes are 13 that are not
this driver's -- Wayland compositors, KMS, GLX and EGL context creation -- plus
`dlist-degenerate`, which crashes before any GL work, and one shader that
cannot split a combined sampler. The 37 refusals are line and polygon stipple,
line smoothing, logic ops, and front and back polygon modes differing: WebGPU
has no equivalent for any of them, and each is refused with a named reason
rather than silently mis-drawn. The 5 remaining WebGPU errors are a depth
texture view missing a usage flag (`dissolve`, `tri-z-eq`), a shader reading
more of a uniform buffer than its layout declares (`blinking-teapot`), a
filtering sampler bound where the layout says non-filtering (`vert-tex`), and
an overlapping copy (`readpix`) -- four distinct bugs, all still ours.

## The last two validation errors, and a correction about the refusals (2026-09-02)

### A texture copied onto itself

```
Copy source and destination are overlapping layer ranges ([0, 1) and [0, 1))
of [Texture (unlabeled 750x250 px, TextureFormat::BGRA8Unorm)] mip level 0
 - While encoding [CommandEncoder].CopyTextureToTexture(...)
```

WebGPU refuses a `CopyTextureToTexture` whose source and destination
subresources are the same, whether or not the regions actually overlap, and it
refuses it by invalidating the whole command buffer -- so the next `Submit`
fails too, and so does every one after it. GL has no such rule, and st/mesa's
`ReadPixels` staging blit asks for one; `readpix` reached it on a 750x250
window.

The copy now goes through a scratch texture of the requested size, both halves
recorded in the same encoder so they stay ordered. The y-flipping path needed
its row range rebased as well: a flipped box runs *upward* from `box->y`, so
its lowest row is `box->y - |height|`, and the loop reads `box->y - 1 - i`,
which means the rebased origin is the row count rather than zero.

Worth knowing for the next such bug: the error text names both textures by
size, so a copy that has already been routed through a scratch says so -- the
source appears with the scratch's dimensions. That is how it became clear the
first attempt had not taken effect at all, which was an install rather than a
code problem. `web/build-mesa-native.sh` builds; it does not install, and the
sweep and every run script read `/tmp/gex-mesa-native/inst`. A rebuild without
`meson install` measures the previous driver, silently and with every
appearance of having worked.

### A uniform block bigger than the buffer bound to it

```
The shader uses more bytes of the buffer (192) than the layout's
minBindingSize (80).
```

`blinking-teapot`'s fragment shader declares

```glsl
layout(std140) uniform colors0 {
    float DiffuseCool; float DiffuseWarm;
    vec3 SurfaceColor; vec3 WarmColor; vec3 CoolColor;
    vec4 some[8];
};
```

which std140 lays out at 192 bytes. It never reads `some`, and it uploads 80.
GL allows that -- `glBindBufferRange` may bind less than the block, and reads
past the end are undefined rather than an error.

The first fix was to relax `minBindingSize` to zero for that binding, which
moved the failure rather than removing it:

```
[Buffer] bound with size 80 at group 2, binding 1 is too small. The pipeline
requires a buffer binding which is at least 192 bytes.
```

WebGPU checks the binding against the size the *module* declares in two
places, at pipeline creation and again at draw time against the pipeline, so
the binding genuinely has to be big enough -- which means the buffer does. The
buffer now grows once to hold the whole block, carrying its old contents
across with a `CopyBufferToBuffer` submitted ahead of anything already
recorded, and the binding names the full size. The bytes past the end read as
zero, which is one of the values GL's "undefined" allows.

The resource keeps the size st/mesa gave it: uploads, maps and the shadow all
still measure it by `allocation_size`, and only the WGPU buffer behind it
changes, tracked separately as `gpu_size`. Growing `allocation_size` instead
would have been a buffer overrun -- the shadow is allocated at that size and
the upload path reads that many bytes out of it.

Each shader records what its blocks need at compile time, after
`ntv_shader_prepare`, which is what puts the variables into the word-array
shape the emitter measures. **A UBO variable arrives as `GLSL_TYPE_INTERFACE`,
for which `glsl_type_is_struct()` answers no.** Reading the size through it
returned zero, the relaxation never fired, and the error was unchanged through
a build and run cycle that looked like the fix simply not working. Use
`glsl_type_is_struct_or_ifc`.

### Correction: fourteen of the 37 refusals were implementable

The previous section said the 37 refusals were "line and polygon stipple, line
smoothing, logic ops, and front and back polygon modes differing: WebGPU has no
equivalent for any of them". That was wrong, and reading the refusal messages
by byte offset is what showed it. `create_rasterizer_state` reports the first
byte where the requested state and the state it can honour differ, and the
`struct pipe_rasterizer_state` bitfield order turns that into a field name:

| byte | bit | field | programs |
|---|---|---|---|
| 0 | 0x02 | `light_twoside` | 4 |
| 0 | 0x80 | `fill_front` low bit (front != back) | 6 |
| 1 | 0x02 | `fill_back` (front != back) | 1 |
| 2 | 0x01 | `poly_stipple_enable` | 4 |
| 3 | 0x04 | `line_stipple_enable` | 6 |
| 3 | 0x02 | `line_smooth` | 2 |
| 6 | 0x40 | `clip_plane_enable` bit 0 | 7 |

Three of those groups are now closed, fourteen programs between them.

**User clip planes (7 programs).** `caps->clip_planes = 0` has st/mesa run
`nir_lower_clip_vs`, so the vertex shader writes `gl_ClipDistance` for each
enabled plane and WebGPU's `clip_distances` does the clipping -- the same path
`clip_distance`'s own CTS cases already took. st sets `clip_plane_enable`
either way, because the field also means "which of a shader's own clip
distances are live", so refusing it refused the whole rasterizer state and
those seven programs lost culling and the scissor over a plane the shader was
already honouring. `tri-userclip` now matches llvmpipe exactly.

**Two-sided lighting (4 programs).** `caps->two_sided_color = false` has st
lower it into the fragment shader with `nir_lower_two_sided_color` and stop
setting `light_twoside` at all. The cap was left on because the lowering used
to reach an ALU opcode zink's SPIR-V emitter did not implement; that gap -- the
vector comparison reductions -- was closed with `nir_lower_alu_width`, and
nothing had gone back to check. `morph3d` matches llvmpipe.

**3D textures (3 programs, and this one was a plain bug).** Every 3D texture
there is was refused:

```
sampler view refused (shape or format) format=r8g8b8a8_unorm target=3
levels=0..0 layers=0..0
```

The gate demanded `last_layer == depth0 - 1`. A 3D texture's depth is not a
layer range: st/mesa's `last_layer()` returns `array_size - 1`, and a 3D
texture's `array_size` is 1, so the view arrives as 0..0. The view creation
code was already correct -- it skips `baseArrayLayer`/`arrayLayerCount` for
`PIPE_TEXTURE_3D` -- so only the gate had to change. `tri-tex-3d`,
`quad-tex-3d` and `stex3d` all match llvmpipe.

What is left of the refusals is line stipple (6), front and back polygon modes
differing (7), polygon stipple (4), line smoothing (2), logic ops (2) and three
singletons. Of those, **polygon stipple has a lowering waiting in Mesa** --
`nir_lower_pstipple_fs` in `gallium/auxiliary/nir/nir_draw_helpers.c`, which
d3d12 uses: it samples a 32x32 stipple texture at `gl_FragCoord/32` and
discards where the alpha is zero, and `util_pstipple_create_stipple_texture`
builds the texture from the pattern. Two things to know before starting it:

- The fragment stage's texture bindings are decided at **create** time, from
  `fs->texture_count` and `fs->texture_slots`, and the bind group for group 0
  is built from those. A pass that adds a sampler in a *variant* therefore
  needs the binding path to follow the variant, not the shader -- which is the
  bulk of the work. The alternative is to bake the 32 pattern words into the
  shader as immediates (a five-level `bcsel` tree selects the row) and key the
  variant on the pattern, which needs no binding at all; applications set the
  pattern once, so it costs one variant.
- **The pattern's row index is not `gl_FragCoord.y & 31` here.** GL defines
  the stipple against the window origin, which is the bottom, and this driver
  advertises `fs_coord_origin_upper_left` -- st/mesa runs
  `nir_lower_wpos_ytransform` for it, so `gl_FragCoord.y` arrives measured
  from the top. The row is `(height - 1 - y) & 31`, and because `height & 31`
  is part of that, the framebuffer height belongs in the variant key. Sampling
  it the way `u_pstipple` does (`pattern[y & 31] & (0x80000000u >> (x & 31))`,
  which is what llvmpipe and d3d12 get) would come out flipped, and flipped by
  a phase that depends on the window height -- which is exactly the kind of
  wrong that looks right in a 32-pixel-tall test.

Line stipple and line smoothing have only `nir_lower_aaline_fs`, which needs a
companion vertex stage that the `draw` module supplies and this driver does
not.


## Shadow mapping: five bugs behind one missing shadow (2026-09-02)

`shadowtex` was the app sweep's one remaining shader rejection. Behind it were
five separate defects, and the last two were only visible because the first
three had been fixed -- each one hid the next.

### 1. Old-style shadow sampling returns a vec4

`shadow2D` and `shadow2DProj` are GLSL 1.20 functions returning a vec4, where
`texture(sampler2DShadow, ...)` returns a scalar. NIR carries the difference as
`is_new_style_shadow`. SPIR-V has only the scalar form, so a vec4 result type
on `OpImageSampleProjDrefImplicitLod` is invalid:

```
error: line:73: Expected Result Type to be int or float scalar type
```

That rejected the whole fragment shader. `gex_wgpu_lower_old_style_shadow`
samples as new-style and expands the scalar to (r, r, r, 1) -- the same answer
zink gives (`lower_zs_swizzle_tex`).

Reading the rejected module is what found it: the driver prints the SPIR-V as
hex, so `spirv-dis` on the reassembled words names the instruction and the
line the error mentions. That is much faster than bisecting a shader.

### 2. The projective depth reference was never divided

SPIR-V's `OpImageSample*Proj*Dref*` projects the *coordinate* -- the divisor is
its last component -- and leaves `D_ref` alone, while GL's `shadow2DProj`
compares against `coord.p / coord.q`. zink's emitter passes the reference
straight through, so every projective shadow comparison used an undivided
reference. For a shadow map that reads as "behind everything", i.e. fully
shadowed, and it looks like a depth-range or bias problem rather than a
missing divide.

`nir_lower_tex`'s `project_src` divides the coordinate *and* the comparator, so
`.lower_txp = ~0u` removes the question. Non-shadow projection is lowered with
it: WGSL has no projective sampling either, so Tint was undoing it again, and
one `frcp` plus a multiply is the whole cost.

### 3. A depth texture read as an ordinary one, with a filter

WebGPU's pair for a shader that declared `texture_2d<f32>` against a
depth-format view is `unfilterable-float` with a non-filtering sampler, so a
filtering sampler could not be honoured -- and the driver declined the draw,
which is a black frame with one line in the log. Reading a depth attachment as
an ordinary texture is not rare (a depth prepass feeding SSAO, soft particles,
depth of field), so the filter is now dropped to nearest and the draw
proceeds. A nearest-filtered depth read is a small error; a missing one is the
whole surface.

The same relaxation had to accept **no sampler state at all**. st/mesa binds a
sampler view for a texture it reads through a display path without binding
sampler state for that unit, and requiring a bound sampler kept the draw
declined. The decline message now prints the shader's sample type, the view
format, the compare mode and the three filters, which is what turned "sample
type differs from the shader's slot=0" into "there is no sampler here at all".

### 4. A swizzled view with no sampler state got no swizzle

`needs_sampler_variant |= swizzled` sat inside `if (uses_sampler && view &&
sampler)`, so a slot whose sampler state was unbound never got the swizzle
variant -- and a swizzle belongs to the *view*. WebGPU reads a depth aspect as
(d, 0, 0, 1) while GL's `GL_DEPTH_TEXTURE_MODE` default of `GL_LUMINANCE` wants
(d, d, d, 1), which is exactly a view swizzle, so `shadowtex`'s depth-texture
display came out red instead of grey.

**This one also fixed SuperTuxKart's HUD.** Its position indicator drew a red
"1" where llvmpipe draws a white one; matching llvmpipe took the HUD crop from
RMSE 0.13 to 0.016. Nothing had connected the two, and nothing would have --
the digit looked like a deliberate colour.

### 5. The default comparison sampler compared with `Always`

A comparison sampler needs some function, and `Always` looks like the harmless
choice -- until a shadow comparison lands on it, which answers "in front of
everything" for every fragment and lights the whole scene. With st binding no
sampler state for `shadowtex`'s shadow map (see 3), every comparison went
through that stand-in, so the scene rendered correctly and cast no shadow at
all. GL's default `GL_TEXTURE_COMPARE_FUNC` is `GL_LEQUAL`, which is also what
a shadow map wants.

### The probe, and one thing it must not assert

`tests/gl44-probe.sh shadow` covers all of it: a 2x2 depth texture with four
distinct depths sampled at four points; both spellings of the lookup; a depth
map *rendered* into an FBO as well as uploaded, because that is a different
path through the driver and every real shadow map takes it; and the same
texture read as an ordinary one.

Two traps in writing it:

- **Green and blue are a profile difference, not a driver one.**
  `GL_DEPTH_TEXTURE_MODE` exists only in a compatibility profile, where its
  default makes a depth lookup (d, d, d, 1); a core profile has no such state
  and reads (d, 0, 0, 1). Native Mesa answers an empty EGL context attribute
  list with 4.6 Compatibility and **the guest's EGL with 3.3 Core**, so the
  probe sees both. It asserts red and alpha only -- alpha still being the
  channel that separates an (r, r, r, 1) expansion from an (r, r, r, r) one.
- **A core profile has no default vertex array object**, so `glDrawArrays`
  without one is `GL_INVALID_OPERATION`. Every case came back as the clear
  colour with `err=0x0502` in the guest and passed natively until the probe
  bound a VAO.

The guest's EGL handing out a core profile where the same driver natively
hands out compatibility is worth knowing about on its own: it is why the probe
carries both shader spellings, and any compatibility-only construct an
application uses will meet it.


## Three smaller things the shadow work turned up (2026-09-02)

Each of these is the same shape as one of the five above: a stand-in or a gate
that was stricter than GL, refusing something GL defines.

**A vertex-stage sampled slot with no sampler state declined the draw.**
`gex_wgpu_interp_variant_get` returned NULL for `!view || (uses_sampler &&
!sampler)`, while the fragment stage has always served an unbound sampler
state with a default sampler -- and st/mesa really does bind a view without
sampler state for a unit it reads through a display path, which is what the
depth-as-float relaxation had to learn. A missing *view* still declines, since
there is nothing to sample. The vertex texture bind group also serves the
default non-filtering sampler now instead of binding NULL, and
`bind_groups[3]` joined the `bind_failed` check: it was left out, so a
vertex-texture bind group that failed to build was bound as NULL and the draw
went ahead reading nothing.

**A viewport with no area failed the context.** `glViewport(x, y, 0, h)` is
legal GL and draws nothing; `wincopy` asks for it, arriving as
`scale=0,-0 translate=0,300` -- which the all-zero "no viewport yet" case does
not catch, because the translate is not zero. `create_viewport_state` refused
it and `gex_fail`ed the context, costing every later call in it. Zero is now
accepted on either axis and the draw path declines the draw, which is the
whole of what the viewport means. The draw path already had a degenerate-
viewport check, but only inside the branch for a viewport reaching *outside*
the framebuffer, so a zero-area one that fits inside went to `setViewport`
instead; it is now checked first.

**An application that names no GL version gets a compatibility profile.**
EGL's profile mask only means anything for a requested version of 3.2 or
later, and `EGL_CONTEXT_MAJOR_VERSION` itself defaults to 1 -- so an empty
attribute list is a request for GL 1.0, which is why native Mesa answers one
with 4.6 Compatibility. The guest's EGL defaulted to core instead, and the
difference is not academic: a core profile has no default vertex array object
and no legacy GLSL, so GL2-era code that names no version has `shadow2D`
rejected and every draw dropped before the driver sees it. An application that
does name 3.2 or later, or that names a profile mask, is unchanged.

This is also why `tests/gl44-probe.sh shadow` carries both shader spellings:
until this, the same probe saw Compatibility natively and Core in the browser.


## Two crashes from the sweep, and a bug my own shadow pass introduced (2026-09-02)

**`samplers_array`: an array of combined samplers.** `uniform sampler2D
tex[8]` is ordinary GLSL and `gex_wgpu_split_combined_samplers` refused it
outright -- `if (var->type != type ...) return false` -- which failed the link
and then tripped the demo's own `assert(loc >= 0)` on the uniform location it
could not find. A plain one-dimensional array is now split into one
texture/sampler pair per unit, `binding = 2 * unit` and `2 * unit + 1` as
before, with `driver_location` the unit so the driver's slot model is
unchanged. Sixteen pairs for that demo.

Two things the change had to get right. A *dynamically* indexed array cannot
be split at all -- WGSL has no array of textures to index and the unit is only
known at run time -- so the pass now scans for
`nir_tex_src_texture_offset`/`sampler_offset` **before** rewriting anything and
refuses if it finds one: a half-split shader is worse than a refused one. And
the variable is retired once per variable rather than once per unit; removing
the same `exec_node` twice corrupts the list.

**`shadow_sampler`: `sampler2DRectShadow`, and the pass that broke it.** With
the projective-reference fix in place this demo still failed, now with

```
error: line:73: Result Type has 1 components, but 2 expected
```

on `%40 = OpImageQuerySizeLod %int %39 %int_0`. The cause was
`gex_wgpu_lower_old_style_shadow` itself. **A size query derived from a shadow
lookup keeps its `is_shadow` flag** -- `nir_lower_tex`'s rect lowering builds
one through `nir_get_texture_size`, which copies both shadow flags -- so a
`txs` on a 2D shadow texture matched the pass's condition, had its two
components collapsed to one, and was splatted back out. The shader then divided
its coordinate by `vec2(width, width)`. `nir_tex_instr_is_query(tex)` is now
part of the guard.

Worth keeping: `GEX_WGPU_TEX_LOG=1` prints every texture instruction's op,
dimension, array-ness, both shadow flags, its component count and the count
`nir_tex_instr_result_size` says it should have, at three points -- on entry,
after the base `nir_lower_tex`, and before ntv. That is what found this in one
run after a long stretch of reasoning about which pass could possibly have
built a one-component query. The answer was that none of them did; a later
pass shrank it.

`nir_tex_instr_result_size` disagreeing with `def.num_components` is a
malformed instruction that neither `nir_validate` nor the SPIR-V emitter
objects to -- it becomes a wrong result type and Tint's complaint arrives with
no NIR in sight. The log line prints both numbers side by side for that
reason.


## Cascaded and cube shadow maps (2026-09-02)

Widening the depth-format targets past plain 2D turned up three more defects,
all in the same place: **the depth attachment was never treated as a slice.**

`is_format_supported` answered "no depth format exists" for anything but
`PIPE_TEXTURE_2D`, and st/mesa has no fallback for that -- it asserts inside
`_mesa_choose_texture_format`, which is how `shadow_sampler` died once its
shaders started linking (a `GL_TEXTURE_RECTANGLE` depth texture). A rectangle
texture *is* a 2D texture with unnormalised coordinates, and WGSL has
`texture_depth_2d_array` and `texture_depth_cube`, so RECT, 2D_ARRAY, CUBE and
CUBE_ARRAY are all expressible; multisampling stays 2D-only, which is also all
WebGPU offers.

Claiming them was not enough, and `tests/gl44-probe.sh depth-array` is what
said so -- eight cases, a two-layer depth array and a depth cube, each layer
or face rendered with a depth on the opposite side of the same reference so a
lookup that reads the wrong one fails rather than passing by luck. Three
things had to be fixed before it passed:

- **`set_framebuffer_state` refused any depth buffer past level 0, layer 0**
  and failed the context otherwise, so attaching the second cascade of a
  shadow map latched everything. It now accepts one level and one layer,
  whichever they are, and records them the way the colour targets already did.
- **The render pass's depth view was built with a NULL descriptor**, which
  views the whole texture. WebGPU refuses an attachment with more than one
  layer -- `The layer count (2) ... used as attachment is greater than 1` --
  so every depth array failed, and a depth mip level other than the first was
  silently rendered into level zero.
- **The depth clear built its own NULL-descriptor view**, with the same
  result: one invalid command buffer, and with it every later submit. It takes
  the level and layer from the surface it was handed.

The pass key gained the depth level and layer too, or a pass rendering one
cascade would be reused for the next.

Why this matters more than a probe passing: a cascaded shadow map is a depth
2D array and an omnidirectional shadow map is a depth cube. Both were
unreachable -- the first attach latched the context -- and nothing in the
sweep said so, because the programs that use them are engines rather than
demos.


## glPolygonStipple, and why front-and-back polygon modes are refused (2026-09-02)

### The stipple

Four programs (`tri-stipple`, `tri-tex-stipple`, `gamma`, `multictx`) had every
rasterizer state refused over `poly_stipple_enable`, and `set_polygon_stipple`
failed the context outright on any pattern that was not all ones -- which is
every enabled stipple there is.

Mesa's own `nir_lower_pstipple_fs` samples a 32x32 alpha texture, which would
need a texture binding this driver decides at shader-create time rather than
per variant. The pattern is only 32 words, so it goes in as immediates and the
variant is keyed on it; applications set it once, so that costs one variant.
The row word is selected by a five-level `bcsel` tree rather than a 32-deep
chain.

**The row index is `y & 31`, and that had to be measured rather than reasoned
about.** GL anchors the pattern at the window origin, which is the bottom,
while WebGPU's fragment position counts from the top -- so the obvious formula
is `(height - 1 - y) & 31`. It is wrong: this driver already flips the geometry
in the vertex shader (`flip=1` in the draw log, from a positive viewport scale
y), so WebGPU's row zero *is* GL's row zero and a raw position load is already
a GL window coordinate. `tests/gl44-probe.sh polygon-stipple` checks a column
pattern and a row pattern on a 32-tall target and the row pattern again on a
48-tall one, because the two formulas differ only by an inversion at 32 and by
an inversion *plus* a shift at 48. One height cannot tell them apart.

One trap in writing the pass: `nir_draw_helpers`' own `load_frag_coord` sets
`INTERP_MODE_NOPERSPECTIVE` on the position input, and copying that is fatal
here -- the input becomes `@builtin(position)` in WGSL and Tint answers
"'@interpolate' can only be used with '@location'".

**The guest had no `glPolygonStipple` at all**, which the browser gate found
and the native run could not: `tools/gen_gl44_transport.py` generates the
registry-backed entry points and covers GL 4.4 *core*, refusing any command
not in it, and the pattern's registry length is `COMPSIZE()` besides. So it
joins the hand-written compatibility entry points, as one opcode carrying 32
words -- one per row, bit 31 the leftmost pixel, which is both what
glPolygonStipple defines and the form `pipe_poly_stipple` uses.

That absence was not a missing feature but a missing symbol: `libGL` exports a
fixed set, so an application calling `glPolygonStipple` failed to resolve it
and did not start. Worth remembering when a compatibility-profile feature
looks unimplemented -- check `nm` on the guest library before the driver.

Three files, and only one of them by hand: the opcode goes in
`crates/gex-gl/src/proto.rs`, from which `guest/gl/gexgl.h` and
`web/src/gl-proto.js` are both generated (editing the header directly is
undone by the next `guest/gl/build.sh`). The host case goes in
`web/mobilegl/gexgl-mobilegl.cpp`, which despite its name is the guest-facing
shim the *Mesa* wasm target links too. The WebGL host implements none of the
compatibility opcodes, so nothing was needed there.

### Front and back polygon modes differing: refused, and what it would take

Eight programs ask for it (`dlist-edgeflag*`, `tri-edgeflag*`, `vp-unfilled`,
`pgon-mode`) and the obvious plan is two draws: one with the back face culled
and the front's fill mode, one with the front culled and the back's.

**That cannot work here.** WebGPU has no polygon mode, so a non-FILL mode is
implemented by rewriting the triangles into lines or points
(`gex_wgpu_draw_unfilled`) -- and **culling does not apply to lines or
points**, in WebGPU or in GL. Once the primitive is rewritten there is no
facing left to cull by, so the second pass cannot be restricted to one side.

Doing it properly needs the facing of each triangle *before* rasterisation,
which means the transformed positions, which means running the vertex shader --
on the CPU, or in a compute pass that writes two index lists. That is a real
piece of work for eight compatibility-profile test programs and no
application, so it stays refused, now for a stated reason rather than "nothing
here could express that".

**Update (2026-09-02): the machinery now exists.** The wide-line work runs the
application's vertex shader as compute into a buffer of transformed positions
(`gex_wgpu_line_draw`), which is exactly what the facing test needs. A pass
over that buffer computing each triangle's signed area in window space could
write two index lists -- with the excluded triangles emitted as three copies of
one index, which rasterises to nothing, so no atomics and no compaction are
needed. Two `gex_wgpu_draw_unfilled` calls then follow.

What has *not* changed is the value. Six of those eight programs are about
**edge flags** as well: `glEdgeFlag` marks polygon edges as boundary or not
and only boundary edges are drawn in LINE mode, which the unfilled rewrite
does not honour either. Fixing front-versus-back alone would leave them drawing
the wrong wireframe rather than no wireframe. Anyone picking this up should
cost both halves together.


## 181 of 341 programs could not start in the browser (2026-09-02)

The app sweep runs natively, where every program links the system's own
libGL. In the browser they link `guest/gl/out/libGL.so.1`, which **exports a
fixed list of symbols** -- so a program calling one it does not export does
not lose that call: it fails to resolve the symbol and never starts. None of
that is visible in a native sweep, and nothing else was looking.

The measurement takes seconds and needs no GPU:

```
nm -D --defined-only guest/gl/out/libGL.so.1 | awk '{print $3}' \
  | grep -E '^(gl|egl|glX)' | sort -u > /tmp/have.txt
objdump -T <program> | awk '/UND/ {print $NF}' | grep -E '^(gl|egl|glX)[A-Z]'
```

**181 of the 341 installed GL programs needed at least one absent symbol.**
`glOrtho` was missing from 127 of them and `glColor3f` from 126. The
fixed-function set had been built for glxgears and its own comment said so --
"the whole set exists to run programs written before shaders: glxgears needs
exactly these twenty and nothing else".

Adding the rest of the immediate-mode and fixed-function state -- the
glColor / glTexCoord / glVertex / glNormal / glRasterPos families in every
width, glOrtho, glScalef, glTexEnv, glFog, glClipPlane, glAlphaFunc, glRect,
glLineStipple, glPushAttrib and glPopAttrib, glPixelZoom, glLightModel,
glLightf, glMaterialf, glMultMatrixf, glLoadMatrixf, glColorMaterial -- takes
it to **302 of 341**.

Narrower forms fold into the widest: glColor3f sends alpha 1, glTexCoord2f
sends r 0 and q 1. That is what GL specifies them to mean, so one opcode
serves a family and the host case is a single GL call, because the host's
context is a compatibility one.

### How to add one

Three files, and only one of them by hand:

- the opcode in `crates/gex-gl/src/proto.rs`. `guest/gl/gexgl.h` and
  `web/src/gl-proto.js` are **generated** from it by `guest/gl/build.sh` --
  editing the header directly is silently undone by the next build, which is
  worth knowing before spending a compile on it.
- the guest function in `guest/gl/gexgl.c`.
- the host case in `web/mobilegl/gexgl-mobilegl.cpp`, which despite its name
  is the guest-facing shim the *Mesa* wasm target links too (see its
  `meson.build`). The WebGL host implements none of the compatibility
  opcodes, so it needs nothing.

`tools/gen_gl44_transport.py` cannot help: it covers GL 4.4 *core* and its
`validate()` refuses any command not in that manifest.

### Gate from pixels, not from state queries

`tests/gl44-probe.sh fixed-function2` has eight cases -- glOrtho with glRectf,
glColor3f through immediate mode, glScalef, glLoadMatrixf, glAlphaFunc,
glPushAttrib/glPopAttrib, glClipPlane and linear fog -- and every one is
checked by reading pixels back. A transport that drops a command leaves
`glGet` answering whatever the host defaults to, which looks like success.
8/8 natively and 8/8 through the guest in the browser, matching llvmpipe in
both.

### What is left, and why it is not more opcodes

The remaining 39 programs need real marshalling:

| symbols | programs | why it is harder |
|---|---|---|
| `glVertexPointer`, `glColorPointer`, `glTexCoordPointer`, `glEnableClientState`, `glDisableClientState` | 10 | pointers into guest memory, followed at draw time |
| `glDrawPixels`, `glCopyPixels`, `glBitmap`, `glPixelTransferf` | 20 | pixel rectangles, with the whole pixel-store state |
| `glRenderMode`, `glSelectBuffer`, `glInitNames`, `glLoadName`, `glPushName` | 4 | selection mode, which returns a buffer to the guest |
| `glCallLists`, `glListBase` | 5 | an array of list names |
| `glXCreatePbuffer` | 4 | GLX, not GL |


## Flat shading was silently smooth (2026-09-02)

The first defect the llvmpipe pixel comparison found on its own, and it is the
kind it was built for: nothing refused, nothing crashed, nothing logged a
word. `dlist-tri-flat-tri` drew a gradient where llvmpipe draws flat red.

Two separate faults, one behind the other.

**`caps->flatshade` was left true, and the driver ignored the bit.**
`create_rasterizer_state` accepted `flatshade` into its `supported` copy and
did nothing with it, so `glShadeModel(GL_FLAT)` had no effect at all. The cap
was left on because "st never asks for it in a core profile" -- true, and
beside the point: the profile here is a compatibility one and glShadeModel is
real in it. A comment three thousand lines away already asserted the cap was
false, which is how the inconsistency survived. Turning it off has st lower
flat shading with `nir_lower_flatshade`, which sets `INTERP_MODE_FLAT` on the
varyings -- and this driver already emits WGSL's `@interpolate(flat)`.

**Then the provoking vertex was wrong.** A flat primitive takes its value from
one vertex, and GL's default convention is the *last*; WGSL's flat
interpolation always takes the first (`first` and `either` are its only
qualifiers, and there is no `last`). So the triangle came out red where
llvmpipe draws blue -- flat, but the wrong corner.

The indices are rewritten instead, so the vertex GL calls provoking comes
first. `u_index_translator` and `u_index_generator` take the API's provoking
vertex and the hardware's as separate arguments and exist for exactly this;
using them rather than rotating triples by hand is what makes strips and fans
right, where the provoking vertex sits at a different offset per primitive and
the winding alternates. zink solves the same problem with a geometry shader
(`lower_pv_mode`), which is far more machinery than an index rewrite this
driver already does three other kinds of.

**The rewrite runs only for a draw with a flat varying to get wrong**, which
is nearly none of them: `gex_wgpu_shader_has_flat_varying` checks the fragment
shader's recorded interpolation, so a shader with none pays nothing. The
recursion has to be stopped explicitly, since the rewritten draw is still
flat-shaded and the same test would send it back.

**What it costs, and the cache that got most of it back.** The rewrite is
per-draw, and glxgears -- which does use `glShadeModel(GL_FLAT)`, for its gear
faces -- lost 28% of its frame rate to it: 424 FPS down to 305. Most of that
is the index-buffer *map*, which breaks the pending batch whenever a recorded
draw already read that buffer.

Caching the rewritten lists brings it back to about 408 FPS, a residual 4%.
Two things about the cache had to be measured rather than assumed:

- **Eight slots recovered nothing at all.** The working set is one entry per
  *draw*, not per index buffer: a display list replays as many draws over one
  buffer, each with its own start and count, and glxgears alone issues about
  eighteen a frame. Sixty-four slots hit; eight thrashed completely.
- **A hash in front of the eleven-field comparison changed nothing**
  measurable, so the residual is not the lookup. It is the extra `draw_vbo`
  and the per-draw upload of the rewritten indices as user indices; keeping a
  `pipe_resource` per cache slot instead would remove that, and has not been
  done.

`tests/gl44-probe.sh flatshade` gates it with three cases -- flat, smooth, and
flat compiled into a display list, which is how the demo does it. The flat
cases check that all three samples are *identical* and that the colour is the
last vertex's; the smooth case checks that they differ and that each channel
grows towards its own corner, because absolute thresholds there would be
asserting where an 8x8 window falls inside a triangle that reaches past it.


## Geometry shaders that read a uniform, and four bugs behind them (2026-09-02)

`gsraytrace` and `geom-outlining-150` both rendered a black window, and both
for the same first reason: `set_constant_buffer` **failed the context** for
any stage other than vertex or fragment. The geometry stage takes real data --
`gs_draw` already binds `ctx->constant_buffers[MESA_SHADER_GEOMETRY]` through
`gex_wgpu_bind_constants`, and everything below is indexed by stage -- so the
only thing missing was permission to store them.

Accepting them let the geometry pass run for the first time, and it ran into
three more faults in a row. Each was invisible while the context was failing
at the first call.

**Two writable storage bindings over one buffer.** WebGPU forbids it, and one
vertex buffer feeding several attributes arrives as several Gallium slots
pointing at the same resource -- `gsraytrace` binds one 4 MiB buffer to four:

```
Writable storage buffer binding aliasing found between [BindGroup] set at bind
group index 0, binding index 0, and ... binding index 1, with overlapping
ranges (offset: 0, size: 4194304)
```

The vertex-buffer blocks the compute passes read are now declared
`NonWritable`, which Tint maps to `var<storage, read>`, and read-only storage
bindings may alias. **zink's `emit_bo` never looked at `var->data.access`**, so
the decoration had to be added there before the declaration could say so; the
layout entries had to change to `ReadOnlyStorage` in the same commit, since
WebGPU rejects a layout that disagrees with the shader.

**The index-bounds scan read from a NULL shadow.** The draw path scans the
index buffer's CPU shadow to find the highest vertex an element will read, and
dereferenced `->shadow` without checking it. A buffer written only on the GPU
has none: the geometry stage's compute pass writes `ctx->gs_indices` and
nothing ever maps it, so this was `NULL + offset`. The scan feeds a diagnostic
overrun warning and nothing else, so not knowing the bound costs only the
warning.

**A GL buffer is retargetable and a WebGPU buffer is not.** `glBufferData`
names one target and the application may bind the same object anywhere
afterwards, while a WGPU buffer's usage is fixed at creation. `gsraytrace`
writes a buffer with transform feedback and then draws from it, so st/mesa
created it with `PIPE_BIND_STREAM_OUTPUT` alone and the vertex bind was
refused -- "not bound as a vertex buffer ... bind=0x400". Every buffer
resource now carries the union of the usages a GL buffer might need (vertex,
index, uniform, storage, both copy directions), and the vertex- and
index-buffer paths no longer consult the Gallium bind flags, which record what
a buffer was created *for* rather than what it can do. MapRead and MapWrite
stay absent -- this driver keeps its own CPU shadow and never maps a WGPU
buffer -- which is also what keeps the combination legal.

`gsraytrace` now draws its scene: the same spheres in the same places and
colours as llvmpipe. What is still missing is the reflections on the ground,
and that is the known geometry-shader transform-feedback gap -- the driver
says so ("transform feedback with a geometry shader is not captured") and
fixing it needs the prefix-sum pipeline described earlier. Worth recording
that this demo is a user of it: the gap had no application behind it before.


## The GL_QUADS diagonal, which is not a defect (2026-09-02)

`quad` and `quad-degenerate` show up in the pixel comparison at RMSE 0.24, and
the shape of the difference is unmistakable: the four corner colours agree with
llvmpipe and the interior gradient runs the other way -- green across the
middle where llvmpipe is red. Sampling the centre says which diagonal each one
split on. llvmpipe's centre is the average of the top-left and bottom-right
colours; gex_wgpu's is the average of the top-right and bottom-left.

**This driver never gets asked.** `GL_QUADS` is decomposed above it, by Mesa's
own `vbo_save`, against `ctx->Const.DriverSupportedPrimMask` -- and that call
passes `PV_LAST`, which selects `do_quad`'s `inpv == LAST` branch: triangles
(v0, v1, v3) and (v1, v2, v3), the v1-v3 diagonal. llvmpipe advertises
`GL_QUADS`, so nothing converts them and its `draw` module splits (v0, v1, v2)
and (v0, v2, v3) instead. Confirmed by `GEX_WGPU_PRIM_LOG=1`, which never
prints for this program: the driver's own `u_primconvert` is not on the path.

The GL specification leaves the decomposition of a quadrilateral
implementation-dependent, so both are legal and neither is a bug to fix. The
only way to match llvmpipe would be to advertise `GL_QUADS` and convert them
in the driver with `PV_FIRST`, which would then take a flat-shaded quad's
colour from the wrong vertex. Not worth it, and worth writing down so the next
reading of the comparison list does not chase it.

**One real fix came out of looking.** Nothing was calling
`util_primconvert_save_flatshade_first`, so the driver's own primconvert had
`api_pv` at its zero default of `PV_FIRST` while GL's convention is the last
vertex. That decides the vertex order of every primitive it *does* convert, so
a flat-shaded line loop or polygon took its colour from the wrong end. Every
driver using primconvert calls this from `bind_rasterizer_state`.


## The index rewrites were corrupting the heap (2026-09-02)

The comparison sweep's second finding, and it was a regression from the first:
`fire` and `fbo_firecube` stopped making a window at all, with

```
malloc(): invalid next size (unsorted)
```

nothing to do with indices on the face of it. `GEX_WGPU_NO_PROVOKING=1` --
added to measure the provoking-vertex rewrite's cost -- put both programs back,
which named the culprit in one run. Valgrind then named the line:

```
Invalid write of size 2
   at generate_tris_uint16_last2first_tris (u_indices_gen.c:515)
   by gex_wgpu_draw_provoking_last (gex_wgpu_screen.c:14495)
```

**u_indices' generated functions write one whole primitive per iteration and
stop on `j < out_nr`.** With `out_nr` not a multiple of the primitive's stride,
the last iteration writes past the end: `fire` draws TRIANGLES with 3524
vertices, so the generator wrote 3526 uint16 into a 3524-entry buffer. The
count has to be trimmed to whole primitives first -- `u_trim_pipe_prim`, which
`u_primconvert` calls one line before its own translator call for exactly this
reason.

**The same latent bug was already in `gex_wgpu_draw_unfilled`**, which had been
passing `draw->count` raw since it was written. It had not been caught because
glPolygonMode is rare and its programs happened to draw whole primitives. Both
call sites trim now.

Worth keeping from this: the env switch added to measure a change's cost is
also the fastest bisect for a regression it caused, and it costs nothing to
leave in. And a "malloc(): invalid next size" from a GL program is worth
one valgrind run before any reasoning -- 100 seconds of it named the exact
generated function, which no amount of reading the driver would have.


## What the pixel comparison says overall (2026-09-02)

277 programs that the app sweep classified as RAN or OK, photographed on
llvmpipe and on gex_wgpu at a fixed delay after their window appeared:

| RMSE against llvmpipe | programs |
|---|---|
| <= 0.02 | 128 |
| <= 0.05 | 57 |
| <= 0.12 | 39 |
| > 0.12 | 29 |
| no window (offscreen, or no reference) | 24 |

**185 of 253 are at 0.05 or below**, which for an animated program photographed
twice is as close as this harness can measure. The tail is what to read, and
almost all of it is phase rather than pixels: `gears`, `glxgears`, `bounce`,
`glxheads`, `glthreads`, `clear-repeat`, `cubemap` and `arbfplight` all differ
only in where a rotation or a light had got to -- `arbfplight` has the same
shading with its highlight a few degrees round.

What the tail actually contained, in the order it was found:

- **flat shading** -- fixed, and the reason the harness exists.
- **`gsraytrace`, `geom-outlining-150`** -- geometry-shader constant buffers,
  and the four bugs behind them. Fixed.
- **`fire`, `fbo_firecube`** -- the heap corruption above, which the harness
  found because they stopped making a window at all.
- **`quad`, `quad-degenerate`** -- the GL_QUADS diagonal, which is
  unspecified. Not a defect; documented so it is not chased again.
- **`points`, `spriteblast`** -- `glPointSize` has no effect, because WGSL has
  no `point_size` builtin and WebGPU rasterises every point as one pixel.
  Implementable by expanding each point into a quad in a vertex-shader
  variant, which would also give `gl_PointCoord` its real value instead of the
  centre constant it returns now. Not attempted.
- **`textures`** -- three of four objects, and the missing one is a
  sphere-mapped quad. Uninvestigated; the demo animates, so a still comparison
  cannot separate the missing object from a different phase without stopping
  it.
- **`sharedtex_mt`**, **`glthreads`** -- multithreaded; a still comparison says
  little.
- **`gltestperf`** -- llvmpipe's frame is nearly empty and gex_wgpu's is a full
  moiré pattern, so gex_wgpu is more likely the one drawing what was asked.

The harness itself has one caveat worth repeating: `import -window` on X11
without a compositor reads the *screen*, so an overlapping window from another
run contaminates the capture. Two early comparisons in this session were
wrong for that reason -- `samplers_array` read 0.377 against a stale window and
0.007 once the display was its own. Run it with nothing else on the display,
or on a display of its own.


## The fixed-function vertex arrays (2026-09-02)

`glVertexPointer` and its siblings are how every pre-VBO GL program submits
geometry, and the guest exported none of them -- so ten of the installed
programs could not resolve `glVertexPointer` and nine could not resolve
`glColorPointer`, and none of them started.

They need no new machinery. `glVertexAttribPointer` with a client pointer
already works this way: the guest copies each enabled array into a scratch
buffer of its own, binds it, and re-points the host at offset zero, so the
draw command, its `first` and any index values keep their meaning. The
fixed-function arrays reuse the same upload with their own state blocks --
vertex, normal, colour, edge flag, colour index, and one texture-coordinate
array per unit, selected by `glClientActiveTexture`.

What travels is the array *shape* and an offset of zero into whatever
`GL_ARRAY_BUFFER` is current on the host, which is exactly what these calls
mean with a buffer bound. The texture-coordinate command carries its unit
rather than relying on the host's client-active selector surviving the rest of
the batch.

`308 of 341` programs resolve fully now.

`tests/gl44-probe.sh client-arrays` gates it from pixels, five cases: two
tightly-packed arrays; one interleaved array, where the stride is not the
element size and a copy that ignores it reads positions out of the colour
slots; `glDrawElements` with client indices *over* client arrays, which is two
different pointers into guest memory in one draw; `glTexCoordPointer` against
a 2x2 texture with the colour array off, so the texture is the only thing that
can colour the quad; and disabling an array again, since a stale enabled array
over a freed pointer is how this goes wrong in an application rather than in a
probe. 5/5 natively and 5/5 through the guest, matching llvmpipe in both.

### The pixel rectangles, which needed no new wire shape after all

`glDrawPixels` carries an image, and the guest already knows how to size one:
`gexgl-pixel.h`'s `gexgl_compute_pixel_layout` is shared with the texture
uploads precisely so both ends of the wire agree, "a memory-safety
requirement, not merely an optimization" as its own comment puts it. So
glDrawPixels, glCopyPixels, glPixelTransferf/i, glClearIndex, glIndex*,
glEdgeFlag, glMaterialiv, glLightiv, glListBase and glCallLists are all one
opcode each.

**glBitmap is the exception the shared calculation refuses.** GL_BITMAP is one
bit per pixel and `gexgl_compute_pixel_layout` rejects it by design, so its
rows are measured separately: `ceil(width / 8)` bytes rounded up to the unpack
alignment, `height` of them. Measuring it in two places is exactly the hazard
that header exists to avoid, so the probe checks a two-row bitmap whose rows
differ (`0xF0` then `0x0F`) -- a wrong row stride shows as the wrong half lit
rather than as nothing at all.

**324 of 341 programs resolve fully now.** `tests/gl44-probe.sh drawpixels`
gates the batch with four cases -- a 2x2 image drawn at a raster position away
from the origin, so the row order and the within-row order are both
observable; glCopyPixels moving it, which reads the target as well as writing
it; glListBase with glCallLists, called by offset so the base has to be
applied; and the bitmap. 4/4 natively and 4/4 through the guest.

### Texture-coordinate generation and glWindowPos

Two more one-symbol blocks, both cheap. `glTexGen*` is how a fixed-function
program does sphere mapping and reflections -- `texcyl` needed nothing else to
start -- and only the object and eye plane equations are vector-valued, so it
is two opcodes. `glWindowPos*` places a raster position in window coordinates,
bypassing the transform, which is how `wincopy` and `glxsnoop` position their
pixel rectangles.

**Do not hand-write the ARB spellings.** `glWindowPos2iARB` is what the
applications actually link against, and `gl-aliases-generated.inc` emits every
such alias from the registry for each core name that exists -- so defining
them by hand collides with the generated file rather than filling a gap.
Adding the core name is enough.

`tests/gl44-probe.sh fixed-function2` gained a texgen case:
`GL_OBJECT_LINEAR` with planes that make `s = x/4` and `t = y/4`, so the
ortho box maps exactly onto a 2x2 texture and each texel lands on one quadrant
-- which a wrong plane or a dropped enable cannot imitate. 9/9 natively and
through the guest.

### What is left, and why

**327 of 341 resolve fully.** The remaining fourteen are three groups, none of
them an ordinary GL application:

- **Three X servers** (Xephyr, Xvfb, Xwayland), each needing 159 symbols --
  the whole legacy GL API including accumulation buffers, colour tables,
  convolution, histograms and evaluators. They link it; they do not
  necessarily call it.
- **Eight GLX surface types** -- `glXCreatePbuffer`, `glXCreatePixmap`,
  `glXCreateGLXPixmap`, `glXUseXFont`. That is the window-system layer rather
  than GL, and a pbuffer or a pixmap is a *different kind of drawable*, which
  this stack does not have.
- **`glpuzzle`** (selection mode: `glRenderMode`, `glSelectBuffer` and the
  name stack) and **`looking-glass-client`** (`glGetTexEnviv`). Selection mode
  needs a reply that carries the hit records back into a client buffer, the
  way glReadPixels does; it is the only remaining item that is ordinary GL.

## Selection mode, the last ordinary GL the guest could not call (2026-09-02)

`glpuzzle` picks its pieces with GL selection mode, and the guest exported
none of it -- `glSelectBuffer`, `glRenderMode`, `glInitNames`, `glLoadName`,
`glPushName`, `glPopName`. It was the last item in the blocked list that was
ordinary GL rather than a window-system drawable or an X server.

Selection mode is the only part of this API surface whose result is neither
pixels nor a scalar: GL writes *hit records* into a buffer the application
owns. On this stack that buffer is on the wrong side of the wire -- the
records are produced where GL runs, on the host, and read where the
application runs, in the guest. So the split is:

- `glSelectBuffer(size, buffer)` keeps the client pointer in the guest and
  sends only the size. The host allocates its own `std::vector<GLuint>` of
  that size and gives *that* to the real `glSelectBuffer`, so GL writes into
  host memory and nothing on the wire carries a guest address.
- `glRenderMode(mode)` flushes and reads back two things: the return value
  (the hit count) and a blob holding the whole buffer. The guest copies
  `min(client size, blob words)` words into the application's buffer.

Everything else is a plain scalar command.

**Why the whole buffer rather than the hits the count implies.** A hit record
is four words plus one per name on the stack, so the count alone does not say
how many words to copy -- you would have to walk the records to find out, on
the guest, having already decided how much to send. The buffers are small
(`glpuzzle` asks for 512 words) and this is a picking path that runs on a
click, not per frame, so copying all of it is both simpler and safe.

`tests/gl44-probe.sh selection` is the gate. It pushes *two distinct names*
and checks which one each record carries, because the interesting failure is
not "no hits" but "the right number of hits with the wrong names" -- a stale
buffer, a wrong word offset or a reply read in the wrong order all produce
plausible counts. A fourth case draws outside the pick region and requires
zero hits, which is what distinguishes records that came from this frame's
geometry from whatever the buffer happened to hold. 4/4 natively on llvmpipe
and 4/4 through the browser, with identical `INFO` lines.

`glpuzzle` itself compares at **RMSE 0.0002** against llvmpipe, which is as
identical as this harness measures.

### The symbol gate is effectively closed

**328 of 341 programs now resolve fully**, up from 160 when this started. The
thirteen that remain are the three X servers (159 legacy symbols each, linked
rather than necessarily called), the eight GLX surface types (pbuffers,
pixmaps, `glXUseXFont` -- a *different kind of drawable*, which this stack
does not have), and `looking-glass-client`'s `glGetTexEnviv`. No ordinary GL
application is blocked on a missing entry point any more.

## Wide points: glPointSize, sprites and smoothing (2026-09-02)

A WebGPU point is one pixel, and WGSL has no `point_size` builtin -- Tint
refuses `SpvBuiltInPointSize` outright, so the driver dropped the
`gl_PointSize` store and reported `max_point_size = 1.0`. glPointSize did
nothing at all.

That was the largest remaining hole in the pixel comparison. `spriteblast`
(RMSE 0.257) and `points` (0.214) were the two most divergent programs in the
whole 253-program sweep, and for the obvious reason: a particle system drawn
as single pixels shares no structure with the same particle system drawn as
sprites.

### The shape of the expansion

A point becomes a quad. The interesting part is where the quad's corners come
from, because WebGPU's vertex fetch offers exactly two indices --
`vertex_index` and `instance_index` -- and a triangle is three *consecutive
vertices of one instance*. So the corners must vary along the vertex index,
which means the per-point data has to vary along the instance one:

- every vertex buffer is rebound with `stepMode: instance`;
- the draw becomes `draw(6, count, 0, first)` -- six vertices, one instance
  per point, `firstInstance` carrying the draw's start;
- a seventh vertex buffer holds six `vec4` corner records, uploaded per draw
  through the stream uploader.

`firstInstance` doing double duty is what keeps `gl_VertexID` working: WGSL's
`instance_index` counts from `firstInstance`, so it *is* `gl_VertexID`, and
the pass rewrites `load_vertex_id` to `load_instance_id` (and `gl_InstanceID`
to zero).

**Indexed point draws cannot take this road**, and that is structural rather
than unfinished: the index buffer would have to index the instance dimension
and WebGPU indexes only the vertex one. Those still draw as single pixels.
Same for an instanced point draw and for one with per-instance attributes.
`GEX_WGPU_POINT_LOG=1` prints the decision and the reason for every point
draw; a refusal also says so once on stderr.

### The offset, and why it is scaled on the CPU

The corner record carries `(dx, dy, s, t)` with the offset **already scaled**,
so the shader is two `ffma`s: `position.xy += corner.xy * position.w`. The
scale is `2 * size / viewport`, because clip space spans 2 across the
viewport.

It is the *viewport*, not the framebuffer, even when the clipped-viewport fold
is active -- the fold multiplies x by `viewport/framebuffer`, so an offset
expressed against the viewport comes out right either way. That is why the
pass runs *before* `gex_wgpu_lower_viewport_transform`, and why the probe has
a half-viewport case: a driver that divided by the framebuffer passes the
whole-viewport case and fails that one.

With `gl_PointSize` written per vertex the record carries `2/viewport` alone
and the shader multiplies by the shader's own value.

### Two ordering traps

**The point size is not known at the position store.** st/mesa emits its
output stores in slot order, and `VARYING_SLOT_PSIZ` (2) comes *after*
`VARYING_SLOT_POS` (0) -- so reading a stashed `gl_PointSize` at the position
store gets the 1.0 the temporary was initialised with. `point-param`'s
attenuated points and `spriteblast`'s sprites came out expanded and one pixel
across. The fix is to leave the position store alone, stash its value, and
store `gl_Position` a *second* time after every store the shader makes. The
last store wins, and the viewport fold that runs later rewrites both.

**gl_PointCoord cannot live at VARYING_SLOT_PNTC.** `ntv_shader_prepare`
recomputes every varying's `driver_location`, and so its WGSL `@location`, as

    location >= VARYING_SLOT_VAR0 ? location - VARYING_SLOT_VAR0 : location

PNTC is 25 under that rule, past WebGPU's sixteen inter-stage locations:

    Vertex output variable "<retval>.gex_point_coord_1" has a location (25)
    that is too large. It should be less than (16).

So the sprite coordinate rides a `VARYING_SLOT_VAR0 + n` slot, with `n` the
lowest *effective* @location neither stage uses -- effective, because
`gl_TexCoord[0]` is slot 4 and lands on `@location(4)`, so counting VAR slots
alone would collide with it.

### Three bugs behind one gradient

Getting the geometry right took one pass. Getting `gl_PointCoord` right took
three more fixes, and all three had the same signature -- a solid square of
one colour, which looks like a correctly expanded point until you check the
colours:

1. **`reads_point_coord` read the wrong thing.** `info.system_values_read`
   does not carry gl_PointCoord, so the bit stayed clear on a shader that
   plainly reads it and every expanded point got the single-pixel centre. It
   is scanned for now.

2. **The fragment variant was built but not reached.** `gex_wgpu_fragment_module`
   has an early return for a shader with no texture, no retype, no coord
   replace, no unproduced input and no stipple -- and `pointcoord` is exactly
   that shape, so it got the base module whose gl_PointCoord is the constant
   0.5. Worth remembering as a category: *the variant is built* and *the
   variant is reached* are separate claims.

3. **The unproduced-input pass zeroed the driver's own varying.** It runs
   after the substitution, cannot tell one `shader_in` from another, and the
   base vertex shader's `varying_interp_mask` naturally does not mention a
   varying the *variant* writes -- so the sprite coordinate came out (0, 0)
   everywhere. Only shaders that *had* an unproduced input were affected,
   which is why a shader reading gl_PointCoord and nothing else was already
   right by then. The coord's slot is now added to `produced`.

GL_COORD_REPLACE needed one more thing: it has to be applied whether or not
the vertex stage writes the texcoord it replaces. It used to ride on the
unproduced-input substitution, which only fires where nothing produces the
varying -- and st/mesa's fixed-function vertex shader writes `gl_TexCoord[0]`
as a matter of course. With a one-pixel point that was invisible, because the
sprite coordinate was the constant (0.5, 0.5) either way.

### GL_POINT_SMOOTH

A round point rather than a square one, done as a discard where the sprite
coordinate is more than 0.5 from the centre. GL's own antialiasing turns
coverage into alpha; this gets the *shape*, which is the whole of the
difference against llvmpipe for the opaque dots `points` and
`point-wide-smooth` draw, and gives up the one-pixel soft edge. Alpha
modulation would mean rewriting the fragment stage's colour outputs and would
only show where the program also blends.

### What it measures

`tests/gl44-probe.sh point` is the gate: extent on a full and a half
viewport, the sprite coordinate's gradient *and its orientation*, a
shader-written `gl_PointSize` with two different sizes in one draw, per-point
attributes from a vertex array (which is what says the vertex/instance
inversion is wired up), a size-1 point that must stay one pixel, and
GL_COORD_REPLACE through a 2x2 texture. 8/8 natively and in the browser, with
every `INFO` line byte-identical to llvmpipe's -- including the four quadrant
colours.

Against llvmpipe, on the programs that draw points:

| program           | before | after   |
|-------------------|--------|---------|
| point-sprite      | 0.105  | **0**   |
| points            | 0.214  | 0.00007 |
| point-wide-smooth | 0.041  | 0.009   |
| pointcoord        | 0.065  | 0.016   |
| point-param       | 0.104  | 0.017   |
| point-wide        | 0.048  | 0.024   |
| spriteblast       | 0.257  | 0.039   |

`spriteblast` animates, so its remainder is phase. `tri-point-line-clipped`
is unchanged at 0.069 and is about line clipping, not points.

`caps->max_point_size` is now 255, matching llvmpipe -- with it left at 1.0
st/mesa clamped every size before the driver ever saw it.

Two diagnostics, both env-gated and both worth reaching for first next time:
`GEX_WGPU_POINT_LOG=1` prints each point draw's count, size, size source,
viewport, whether the fragment stage reads the sprite coordinate, the
GL_COORD_REPLACE mask and the decision; it also prints each *unproduced*
fragment input's slot, which is how bug 3 above was finally named.
`GEX_WGPU_POINT_NIR=1` prints the vertex and fragment variants of the first
few expanded point draws. The fragment variant's NIR is what settled bug 3:
the texcoord read had been folded to `load_const (0, 0)`, and only the slot
in the log said which variable that came from.

### One thing the point probe measured and did not fix

A *one-pixel* point does not go through the expansion, and at exactly integral
window coordinates it lands one pixel low and left of where GL puts it. The
probe's own log says so:

    llvmpipe   INFO single box=16,16..16,16
    gex_wgpu   INFO single box=15,15..15,15

GL specifies the covered fragment as `floor(x_w), floor(y_w)`, and
`floor(16.0)` is 16. WebGPU rasterises a point as a 1x1 square centred on the
vertex, whose edges then fall exactly on the pixel centres at 15.5 and 16.5,
and the fill rule takes the lower one. Away from integers the two agree --
`floor(16.3)` and `floor(16.7)` are both 16, and the 1x1 square contains only
pixel centre 16.5 in both cases -- so this is a tie-break at exact integers
and nothing more. The expanded path does not have it: an 8-pixel point at the
same place gives `box=12,12..19,19` on both.

The *expanded* path no longer has this: the corner offsets carry a
1/512-pixel translation that breaks the tie the way `floor` does -- see the
indexed-points section below, where it turned out to matter for a shader
writing `gl_PointSize = 1.0`. What remains is the un-expanded path, where the
point is WebGPU's own and there is nothing between it and the rasteriser.

Fixing that would mean expanding one-pixel points too, and paying six vertices
per point for a tie-break; or shifting the draw's window origin, which cannot
go through `setViewport` (WebGPU requires the viewport rectangle to lie inside
the attachment and the common case fills it exactly) and would have to reuse
the clipped-viewport fold, costing a cached vertex variant per shader. Neither
has been shown to be worth it: the `restart_mode` failure it was suspected of
causing turned out to be the indexed refusal instead.

### Lines are the same trick, and are the next thing worth doing

With the point expansion in place, the remaining refusals in the app sweep
fall into five clusters, and the largest is lines:

| cluster                    | programs | note |
|----------------------------|----------|------|
| line stipple               | 6 | `line_stipple_enable` refused |
| front != back polygon mode | 7 | not implementable, documented above |
| line smooth                | 2 | `line_smooth` refused |
| logic op                   | 2 | WGSL has none |

WebGPU draws every line one pixel wide, has no stipple and no line
antialiasing -- the same three holes points had, and all three want the same
thing: a quad over the line, whose orientation depends on the line's
*direction* and therefore on **both** endpoints.

Getting both endpoints' attributes into one invocation is easy, and is the
part that generalises from the point work: for `LINES`, instance *i* wants
vertices *2i* and *2i+1*, and for `LINE_STRIP` it wants *i* and *i+1*. Both
are expressible by binding the same vertex buffer to **two** instance-step
slots, with `arrayStride` 2x or 1x the vertex stride and the second slot's
offset one vertex further in.

**The hard part is that attributes are not positions.** A quad needs both
endpoints' *clip-space* positions, and the vertex shader computes one
position from one set of attributes -- so it would have to run its whole
transform twice with the two attribute sets, which means duplicating every
input and cloning the position computation. That is a different order of NIR
surgery from the point pass, which only had to add an offset to a position the
shader had already computed.

Two routes that avoid the duplication, both bigger than the point work:

- **Reuse the geometry-shader path.** The driver already runs a vertex shader
  as a *compute* pass that writes every varying, position included, to a
  buffer at a fixed stride -- `gex_wgpu_gs_draw`, the `varying_nir` variants
  and `gex_wgpu_varying_layout`, all built for geometry shader support. A line
  draw would then be:

    1. one compute dispatch running the bound vertex shader over the draw's
       vertices, exactly as `gs_draw` pass one does;
    2. a render pass of six vertices by N line instances with a *generated*
       pass-through vertex shader -- `gex_wgpu_gs_passthrough` is the model --
       that reads endpoints A and B out of that buffer by `instance_index`,
       computes the screen-space perpendicular from their two clip positions,
       offsets the corner by half the width, and forwards the varyings of
       whichever endpoint its corner belongs to.

  No shader duplication, and the rasteriser interpolates the varyings for
  free because each corner simply carries one endpoint's values. Index
  arithmetic in the shader covers `LINE_STRIP` and `LINE_LOOP` as well as
  `LINES`, and -- unlike the point expansion -- even *indexed* line draws, by
  reading the index buffer as a storage binding the way the capture path
  already does. A stipple counter is the distance along the line in pixels,
  which the same shader can emit as a varying.

  The cost is a compute dispatch per line draw, which is why it must stay
  gated on width > 1, stipple or smooth: an ordinary one-pixel wireframe --
  and glPolygonMode's rewrite produces plenty of those -- has to keep the
  fast path.

  **The quad is axis-aligned, not perpendicular.** GL 4.6 §14.5.1 rasterises a
  non-antialiased wide line as *w copies of the line offset along its minor
  axis* -- so a 5-pixel diagonal lights five pixels per row, not the seven a
  rotated quad would. llvmpipe agrees (`INFO diagonal row=5`), which is worth
  knowing *before* writing the shader: the offset is `(0, w/2)` for an x-major
  line and `(w/2, 0)` for a y-major one, chosen by comparing |dx| with |dy| in
  window space. That is both simpler than a perpendicular and the rule GL
  actually states.
- **A brush approximation.** GL 4.6 §14.5.1 defines a non-antialiased
  width-*w* line as *w* copies of the line offset along its *minor* axis, so
  drawing the line *w* times with a sub-pixel window offset would be close --
  and a square brush (offsets in both axes) would over-draw the ends of
  diagonal lines. Cheap, approximate, and it would need to be documented as
  approximate. It does nothing for stipple.

`caps->max_line_width` is 1.0 for the same reason `max_point_size` was, and
should only be raised together with whichever of these lands.

### What landed

The first route, and smaller than it looked, because the render side needs no
new plumbing at all. `gex_wgpu_line_draw` runs the application's vertex shader
as compute into `ctx->line_vs_out` -- that is the geometry path's first pass
with the geometry stage taken out -- and then swaps `ctx->vs` and
`ctx->vertex_elements` for a generated quad shader and re-enters
`gex_wgpu_draw_vbo` with triangles. That is exactly how `gex_wgpu_gs_draw`
rasterises its own output, so pipelines, bind groups, render passes and every
shader variant come for free.

The two endpoints reach one invocation through the vertex layout rather than
through storage reads: the capture buffer is bound **twice**, at offset 0 and
one record further in, both with `instance_divisor = 1` and a stride of one
segment. For `LINES` a segment is two records, for `LINE_STRIP` it is one --
so the same layout expresses both, and neither needs an index table. That is
also why v1 is *non-indexed LINES and LINE_STRIP only*: `LINE_LOOP` wraps and
an indexed draw permutes, and both would need the index table the capture pass
already supports but this does not yet build.

A third buffer holds the six corners, at stride 32, per *vertex* rather than
per instance: which end and which side each corner is, and the draw's own
numbers (half the width in clip space, and the aspect ratio, which is what
lets the shader compare the two axes in pixels).

Declines, all of them logged once and all of them leaving the line one pixel
wide, which is what it was before: a vertex shader that samples a texture (the
capture dispatch binds vertex buffers, its output and the constant buffers,
and a compute module declaring a texture binding the dispatch does not provide
is a pipeline WebGPU rejects, which poisons the device), per-instance vertex
attributes, more than seven varyings (two attributes each against WebGPU's
sixteen), and an empty viewport.

### A uniform bug the line work uncovered

The first wide line of any program drew nothing, and every later one was
right. The capture buffer said why: `A=0,0,0,0 B=0,0,0,0` on the first
dispatch and the correct clip positions on every one after -- a
modelview-projection matrix read as zero.

Uniforms are staged into a ring and written to the GPU in one queue write,
inside `gex_wgpu_submit_pending`. That function begins

    gex_wgpu_end_pass(ctx);
    if (!ctx->pending_encoder)
       return;
    ...
    gex_wgpu_flush_constant_ring(ctx);

-- so with nothing else pending it returned *before* flushing. Every compute
pass in this driver submits its own command buffer through
`gex_wgpu_dispatch`, which calls `submit_pending` first, so a dispatch with no
other work in flight read a constant ring the GPU had never been given. The
flush moved above the early return.

This was not a line bug. It is a latent one in **every** compute pass the
driver runs -- transform feedback's capture and the geometry stage's two
passes included -- and it would show as the first such draw after a state
change reading stale or zero uniforms. Geometry shaders got away with it
because something else had usually flushed the ring by the time they ran.

### GL_LINE_SMOOTH, which is a different rule

An *antialiased* wide line is not the same shape as an aliased one. GL
rasterises the aliased one as w copies along the minor axis, and the
antialiased one as **a rectangle of width w centred on the segment** -- so a
smooth five-pixel diagonal reaches wider in x than an aliased one, which
llvmpipe's footprints show plainly (`6,6..25,25` against `8,6..23,25`). The
quad shader therefore takes a perpendicular offset when smoothing and an
axis-aligned one when not; since it is generated per varying layout *and* per
smooth location, that is a compile-time branch rather than a runtime one.

Coverage rides on one extra varying: `side` scaled by the widened half-width,
which interpolates to the fragment's distance from the line's centre in
pixels, with the original half-width in the second component (both endpoints
carry the same value, so it interpolates to a constant the fragment stage can
use -- baking it in would key a variant on the line width, and applications
change that). The fragment variant multiplies it into alpha, which is what GL
specifies and what only shows where the program blends; the *shape* changes
regardless, because the quad is a pixel wider.

Two caps traps, both of which cost a debugging round:

- **`max_line_width_aa` and `max_point_size_aa` are separate caps.** Raising
  only `max_line_width` left st/mesa clamping a smooth line back to one pixel,
  so GL_LINE_SMOOTH produced a two-pixel line where the aliased path produced
  five -- the expansion working perfectly on a width of 1.
- **`ctx->line_smooth_location` had to be initialised to `~0u`.** Zero is a
  valid varying location, and the zero CALLOC leaves had every fragment
  variant believe a coverage varying was in play: an input declared at
  location 0, that slot marked produced, and alpha multiplied by whatever was
  there. Nineteen native probes went from identical-to-llvmpipe to failing in
  a single build, which is exactly what that gate is for.

### GL_LINE_STIPPLE, and the prefix pass a strip needs

The counter is the distance from the start of the line in pixels, and GL
resets it at every `glBegin` and, for `GL_LINES`, before *every* segment --
but a strip or a loop runs on. So independent segments need nothing at all
(their start is zero) and a strip needs the summed length of every segment
before it, which depends on the transformed positions and so cannot be
computed on the CPU.

One compute invocation walks the strip and writes each segment's start. A
draw has at most a few thousand segments and this is gated on stippling being
on, so a serial loop costs microseconds where a parallel scan would be more
code for nothing. Its output is bound as an **ordinary per-instance vertex
attribute**, which is why the quad shader still needs no storage bindings; the
`GL_LINES` case points that same attribute at a zero in the corner buffer with
`arrayStride` 0, which saves a buffer and a dispatch.

The pattern and the repeat factor are baked into a fragment variant as
immediates and keyed on -- the same choice `gex_wgpu_lower_poly_stipple` makes
for its thirty-two words, and for the same reason: they change rarely, and a
uniform would cost a binding on every fragment shader in the driver.

Smoothing and stippling share one varying: coverage across the line in `.xy`
and distance along it in `.z`. The trap there was that each lowering created
that input for itself, so a stipple-only draw found no variable and did
nothing -- both now go through one find-or-create helper.

`tests/gl44-probe.sh line` is 7/7: the strip case draws two collinear halves
and compares them against the single line **pixel for pixel**, because a
counter that restarted at the join could still produce the same *count*.

## Indexed wide points, and a guest bug underneath them (2026-09-02)

The wide-point expansion refused indexed draws, because it needs the per-point
data on the instance index and WebGPU's index buffer addresses only the vertex
one. That was not a corner case. `KHR-GL33.primitive_restart.restart_mode`
draws indexed points from a vertex shader that writes `gl_PointSize = 4.0`,
and the driver's own log named it -- `points drawn one pixel wide (indexed)`
-- after five other explanations for that family's failure had been ruled out
and a sixth (the exact-integer tie-break, below) had been guessed at.

### De-indexing, by dword

An indexed draw is now **de-indexed**: the vertices it references are gathered
into a fresh buffer in draw order, and the draw proceeds as if it had never
been indexed. Nothing downstream changes -- the strides are the same, so every
attribute's `src_offset` still lands where it did, and the corner buffer, the
step modes and both shader variants are untouched.

The copy is format-agnostic on purpose. A vertex is a run of bytes whose
meaning belongs to the vertex elements, so the compute shader runs **one
invocation per dword** -- `vertex = i / stride`, `dword = i % stride` -- which
needs no per-layout variant and no loop. One dispatch per vertex buffer,
because eight sources and eight destinations would be sixteen storage bindings
and the device offers ten. The indices themselves are resolved on the CPU and
uploaded as a table, which is what the transform feedback capture path already
does for the same reason.

Three bugs, each caught by a different gate:

- **`firstInstance` was passed twice.** A gathered buffer starts at draw order
  zero, so the draw's own start has already been applied -- passing it again
  asked for instances 7..12 of a six-vertex buffer, which is a validation
  error, and a validation error takes the whole command buffer with it. Half
  of every restart-split point draw vanished. `tests/gl44-probe.sh primrestart`
  went 22/22 → 20/22 and named it.

- **A sub-pixel nudge was needed.** GL rasterises a point as the fragments
  whose centres lie in `[x - s/2, x + s/2]`, and for `s = 1` states the answer
  outright: `floor(x_w)`. A quad with its edges exactly on pixel centres is a
  tie, and WebGPU's fill rule breaks it the other way -- a point at 16.0 landed
  on pixel 15 where llvmpipe puts it on 16. That is invisible for a point four
  pixels across and decisive for one pixel, and one pixel *does* reach the
  expansion: whenever the shader writes `gl_PointSize` the size is unknown
  until the shader runs, so the draw is expanded whatever it turns out to be.
  The corner offsets now carry a 1/512-pixel translation, with its y sign
  baked on the CPU because the y-flipped module negates what it is given and
  a corner buffer belongs to one draw.

- **The bounds scan dereferenced NULL.** It was guarded on `info->index_size`,
  which a de-indexed draw still has, rather than on the index resource, which
  it no longer has.

### The guest never uploaded fixed-function client arrays for glDrawElements

The browser gate then failed where the native one passed, and the probe's own
control case is what separated the two: the *same* indexed draw at size 1,
which never goes near the expansion, drew nothing either. So the expansion was
innocent.

`glDrawElements` in the guest uploads client vertex arrays as far as the
indices reach -- and tested only `vertex_attrib_client_count`, the *generic*
attribute arrays. `glVertexPointer`/`glColorPointer` are counted separately,
so a fixed-function client array got no upload at all and the host drew from
whatever a previous `glDrawArrays` had left in the scratch buffers. For an
index reaching past that draw's vertex count, the fetch is out of bounds, and
an out-of-bounds vertex fetch reads zero -- so the draw covered no pixels and
said nothing about why.

This is old-GL bread and butter (`glVertexPointer` + `glDrawElements`), and it
was silently wrong in the browser for every program that does it. Fixed by
counting both kinds of client array.

`glDrawRangeElements` and the `BaseVertex` family are still generated as
*element-buffer offsets*, on the reasoning that "in an OpenGL 4.4 core context
these are never guest client-memory pointers" -- true, but the guest hands out
compatibility contexts now, where they can be. That gap is untested and
unmeasured; it wants the same treatment `glDrawElements` has.

### What is still wrong with restart_mode

The POINTS case no longer draws one-pixel points, and the family still fails
-- now with the same seven modes as before, which are POINTS and all six line
topologies while every triangle topology passes. Since a one-pixel line is not
something this driver can place differently, and lines get a ±1 tolerance in
that test, the remaining failure is more likely to be about lines than about
points. It has not been separated further.

### One known limit of the de-indexing

`gl_VertexID` in an expanded indexed point draw reports the vertex's *position
in the index list* rather than the index value, because the expansion reads
`instance_index` and the gather renumbered the vertices. Getting it right
means binding the index table as a per-instance integer attribute, which costs
a vertex attribute and a buffer slot; no installed program has been seen to
need it.

### The rest of the DrawElements family

`glDrawElements` was hand-written and handled client pointers; every other
indexed draw was generated from the registry as an *element-buffer offset*, on
this reasoning, quoted from the generator:

> An opaque pointer interpreted as a byte offset into the vertex array's bound
> GL_ELEMENT_ARRAY_BUFFER. In an OpenGL 4.4 core context these are never guest
> client-memory pointers.

True of a core context, and the guest hands out **compatibility** contexts now
(see the EGL profile work) -- where `indices` is a client pointer whenever no
element array buffer is bound. Sent as an offset it had the host read its own
address space, and nothing uploaded the client vertex arrays either, so the
draw read zeroes and covered no pixels. `glDrawRangeElements` is the GL 1.2
form most old code actually calls, and `glDrawElementsInstanced` sent no
indices at all.

All seven now share one opcode and one host case. Base vertex, instancing and
base instance are *fields* rather than different commands, and the host
dispatches on them; `glDrawRangeElements`' start/end are dropped, which is
legal -- they are a promise about the indices, not a change to what is drawn.
The upload extent accounts for a positive base vertex, because the host then
reads vertex `index + basevertex`.

`tests/gl44-probe.sh point` gained a `glDrawRangeElements` case with client
arrays and client indices.

Two things are still not handled, both for the same reason -- the guest cannot
see the indices:

- **Client vertex arrays with the indices in a buffer.** The guest would have
  to read the buffer back to know how far the arrays reach, so it uploads
  nothing and the host draws from whatever was there. Legal GL, untested.
- **`glMultiDrawElements`.** It carries an array of pointers, each of which
  can be a client pointer, so the same fix needs a blob per element rather
  than one blob. Its packet still treats them all as offsets.

### What the line work moved

Seven of the eight programs the rasterizer gate refused for line state now
bind their state and draw: `line-smooth`, `line-stipple-wide`,
`linestrip-stipple`, `linestrip-flat-stipple`, `linestrip-stipple-wide`,
`quad-unfilled-stipple` and `tri-unfilled-userclip-stip`. The eighth,
`tri-unfilled-smooth`, still refuses -- and for a different reason:
`fill=1/0`, front and back polygon modes differing, which is the cluster above.

SuperTuxKart, which is what all of this is ultimately for, still renders its
scene: 586 of 599 frames presented at 57 FPS, with the kart, the track, the
minimap and the HUD all where they belong.

## Telling a defect from a turning gear (2026-09-02)

The llvmpipe pixel comparison has always had one weakness, stated in its own
header: "Anything animated diverges by construction". That left its tail
unreadable -- `textures` sat at RMSE 0.33 and was written up as a known defect
("`textures`' missing sphere-mapped object"), and `clear-repeat` at 0.39 above
it, and there was no way to tell either from a real one.

`GEX_COMPARE_SELF=1` photographs **gex_wgpu twice** and reports how far the
driver's own two images are apart. That is the animation floor, and a
divergence at or below it says nothing:

| program        | vs llvmpipe | vs itself | reading |
|----------------|-------------|-----------|---------|
| textures       | 0.314       | 0.319     | animation, not a defect |
| clear-repeat   | 0.274       | 0.247     | animation, not a defect |
| quad-degenerate| 0.244       | 0         | the GL_QUADS diagonal, unspecified |
| tri-viewport   | 0.162       | 0         | **real**: one-pixel rows |
| tri-cull-both  | 0.336       | 0         | **real**: fixed, now 0 |

It costs a third run per program, so it is off by default -- but the tail is
where the next defect is, and the tail is exactly where it is needed. Two of
the five entries above were noise and two were real; without the column all
five looked the same.

**`textures` is not a defect** and the note calling it one is withdrawn.

### The one-pixel rows, and why they stay

`tri-viewport` draws a grid every 25 pixels of a 250-pixel window. Every
*horizontal* line lands one row from where llvmpipe puts it; every vertical
one is exact. GL measures window y from the bottom and WebGPU from the top, so
a rasterisation tie resolves opposite ways in the two, and geometry exactly on
a pixel boundary is exactly a tie. A one-pixel point at an integral coordinate
does the same in both axes.

The viewport is the obvious lever and it does not work. WebGPU requires the
viewport rectangle to lie inside the attachment, so it cannot be *translated*
when it already fills the framebuffer -- only shrunk, which gives a shift that
fades linearly to nothing at the far edge. Measured: 0.162 -> 0.073 at a 64th
of a pixel and 0.053 at half a pixel, moving some of the grid's lines and not
others. That is worse than a consistent tie-break, because it makes the answer
depend on where on the screen the geometry is.

A uniform translation has to happen in the shader: either a vertex variant
keyed on the framebuffer height, recompiled on every resize for every shader,
or a uniform read by every vertex shader in the driver. Both are real costs
for one pixel. Left alone, with the measurement written down.

### `dlist-degenerate`: a lead, localised but not solved

The animation floor put this one squarely in the real column: RMSE 0.335
against llvmpipe with the driver's own two frames identical. llvmpipe draws a
white square; gex_wgpu draws nothing at all and Mesa reports
`GL_INVALID_OPERATION in glEnd`.

What is known, and it is worth having:

- **It is not the Mesa version.** The same build of this tree, with
  `GALLIUM_DRIVER=softpipe`, draws the square. Only the driver differs.
- **The display list comes out empty.** Instrumenting `compile_vertex_list`:
  softpipe reaches it with `verts=3`, gex_wgpu with `verts=0`, and neither has
  `out_of_memory` set. `total_vert_count` is `prims[last].start +
  prims[last].count - prims[0].start`, so a zero means the last primitive's
  `count` was never filled in.
- **`vbo_save_NotifyBegin` runs identically** under both -- `mode=0x9`
  (GL_POLYGON), `start=0`.
- **`_save_End` runs under neither**, which is the odd part: softpipe still
  ends up with `count=3`, so something else closes the primitive there.
- **The application cannot be branching.** `objdump -T` shows it calls no
  GL query at all -- glBegin, glVertex2f, glEnd, glNewList, glEndList,
  glCallList and matrix calls, and nothing else. Identical calls, identical
  Mesa, different result.
- **It is not a timeout artefact**: 25 seconds behaves like 6.

One more thing narrows it, found after the above: the error comes from
`dlist.c:save_End`, not from vbo's `_save_End`. That function ends with

    if (ctx->ExecuteFlag)
       CALL_End(ctx->Dispatch.Exec, ());

so the list was opened `GL_COMPILE_AND_EXECUTE`, and the *exec* End is what
finds itself outside a begin/end -- `save_Begin` in the same file never calls
the exec Begin. Both drivers take that path; what differs is whether the
vertices were in the vbo save store by the time it ran. A `SAVE_FLUSH_VERTICES`
between the Begin and the vertices would reinstall the plain display-list
dispatch and send them to opcodes instead, which would leave exactly the
`verts=0` seen here.

So the divergence is inside Mesa's vbo save path and is driven by something
the driver supplies -- a cap, or a callback -- rather than by anything the
application does. `GEX_DLIST_LOG` is gone from the tree; re-add the two
`fprintf`s at `compile_vertex_list` and `vbo_save_NotifyBegin` to pick this up
where it was left.

Worth remembering that the earlier work here (the two NULL guards in
`vbo_save_api.c`) turned this program from a crash into a silent blank. That
was the right call for the crash -- a null dereference inside `glEndList`
takes the application with it -- but the blank is this bug, not that fix.

### The tail, read with the floor beside it

Sixty-one programs -- the old tail plus everything that draws points or lines
-- re-measured with `GEX_COMPARE_SELF=1`. What is left in the *real* column,
and what each one is:

| program | vs llvmpipe | self | what it is |
|---|---|---|---|
| dlist-edgeflag, -dangling | 0.46 | 0 | front != back polygon mode, and edge flags |
| dlist-degenerate | 0.34 | 0 | the vbo save lead above |
| gltestperf | 0.31 | 0 | *not* a defect -- see below |
| quad, quad-degenerate | 0.24 | 0 | the GL_QUADS diagonal, unspecified by GL |
| poly-flat-unfilled-clip | 0.15 | 0 | front != back polygon mode |
| gsraytrace | 0.12 | 0 | geometry-shader transform feedback, known |

Everything else above 0.1 has an animation floor to match it and says nothing:
`clear-repeat` 0.50 against 0.49, `cubemap` 0.40 against 0.30, `glxheads` 0.24
against 0.24, `arbfplight` 0.23 against 0.17, `bounce`, `fire`, `glxgears_*`,
`arbfslight`.

**`gltestperf` is a timing test, not a rendering one.** It draws a triangle
whose size comes out of its own `glFinish`-based measurement -- `objdump`
shows `glutGet`, `glFinish` and no geometry that does not depend on them -- so
a slower driver draws a smaller triangle and the picture differs legitimately.
Worth knowing before someone spends an afternoon on it.

So every remaining entry is either fixed, carrying a written-down lead, or a
GL feature this backend has a stated reason not to have.

## A reference can be wrong: the second-reference column

The llvmpipe pixel comparison (`tests/gl-app-compare.sh`) had one reference,
and a ranked list is only as good as the thing it ranks against. Two classes
of false defect sit in it. The animation floor (`GEX_COMPARE_SELF=1`) retires
the first: a program photographed twice by the same driver, which is how a
spinning gear is told from a wrong pixel. `GEX_COMPARE_REF2=1` retires the
second, which is a reference that is simply wrong, by photographing **softpipe**
as well and reporting how far the two references are from each other.

Read the columns as: `rmse` means nothing unless it is well above **both**
`self` and `refdiff`.

The program that forced this was `quad` -- four vertices, one `glBegin`, the
most trivial thing in the whole sweep -- sitting at 0.24378 with an animation
floor of zero, which is exactly what a real deterministic defect looks like.
It is not one. `quad` and `quad-degenerate` scored identically because they
are the same defect, and the defect is llvmpipe's:

| program | gex vs llvmpipe | gex vs softpipe | llvmpipe vs softpipe |
|---|---|---|---|
| `quad`, `quad-degenerate` | 0.244 | 0.060 | **0.251** |
| `cubemap` | 0.380 | -- | **0.551** |
| `tri-viewport` | 0.162 | 0.228 | **0.164** |
| `simplex-noise` | 0.149 | 0.207 | **0.172** |
| `tri-fbo-tex-mip` | 0.276 | 0.276 | **0** |
| `dlist-degenerate` | 0.335 | 0.335 | **0** |
| `dlist-edgeflag` | 0.463 | 0.464 | **0.029** |

Five of the nine deterministic entries at the top of the tail were the
reference. The four that survived are the ones where the references agree to
within 0.03 and the driver does not.

### Why llvmpipe disagrees about a quad

WebGPU has no quad topology, so a `GL_QUADS` draw becomes two triangles. Both
diagonals cover the same pixels and give the same four corner colours, and
only the interior differs -- so the wrong choice draws a quad that is the
right shape, in the right place, with the right corners, and a transposed
gradient. The centre pixel names the diagonal, because it lies on whichever
one was chosen and takes the average of that diagonal's two endpoints.

The GL spec leaves the subdivision implementation-dependent, so neither answer
is a conformance failure. What matters is that llvmpipe is alone:

    gl44-quad-probe   gex_wgpu 6/6   softpipe 6/6   radeonsi 6/6   llvmpipe 3/6

A driver that does not advertise `MESA_PRIM_QUADS` -- which is every hardware
driver, and this one -- has its quads converted by `u_primconvert`, whose
`do_quad()` splits on **v1-v3** under GL's default last-vertex convention.
llvmpipe advertises quads, keeps them to the `draw` module, and splits an
immediate-mode quad on **v0-v2**. Its own display-list path splits on v1-v3,
so it disagrees with itself between immediate mode and a display list.

`tests/integration/programs/gl44-quad-probe.c` is the gate. It measures
`GL_QUADS` in immediate mode, in a display list and through client arrays,
plus `GL_QUAD_STRIP` and `GL_POLYGON`, and two quads in one `glBegin` so a
converter that got the per-quad base wrong is caught. The `GL_QUAD_STRIP`
expectation is measured rather than reasoned: all four drivers split a strip
quad on v0-v3, which is not what the QUADS reasoning predicts.

## Rendering into a mip level that is not the base

`tri-fbo-tex-mip` was the top deterministic divergence that survived the
second reference -- both references byte-identical, this driver different --
and it was **two** separate bugs, both of which had been invisible because the
ordinary case hides them.

`tests/integration/programs/gl44-fbo-mip-probe.c` is the gate. It reads every
level back with `glGetTexImage` rather than sampling, so a failure names the
level that is wrong instead of the pixel.

### 1. glClear cleared level zero

`gex_wgpu_clear` reconstructed each attachment as a `pipe_surface` with
`level = 0`, `first_layer = 0` at the resource's full `width0 x height0`, on a
comment saying `set_framebuffer_state` had required exactly that. It had
stopped being true: the framebuffer path keeps `color_levels[]` and
`color_layers[]` and renders into any level and layer, and its struct comment
says why -- "rendering a mip chain is how SuperTuxKart builds its lightmaps".

So a `glClear` on a framebuffer whose attachment is mip level 4 of a 512x512
texture cleared **level 0 at 512x512** -- wiping the base level and leaving the
level being drawn into untouched. It did not fail, because clearing level zero
at its own full size is a perfectly valid clear of the wrong thing.

The colour helper already handled this; only the caller lied to it. The depth
helper did not, and fixing the caller exposed that: `gex_wgpu_clear_depth_stencil`
guarded on `dst->level != 0 || dst->first_layer != 0` while its own body built
its view from `dst->level` and `dst->first_layer`, with a comment explaining
that one layer of a depth array is exactly what a cascaded shadow map
attaches. Nothing had ever reached the mismatch. Passing the real layer turned
a silent clear of the wrong cascade into a latched context, and `depth-array`
went from 8/8 to 4/8 with `GL_OUT_OF_MEMORY` -- which is what `gex_fail`
becomes in st/mesa. The guard now matches the body.

### 2. The feedback-loop detector ate mipmap generation

The larger one. `gex_wgpu_feedback_snapshot` exists because a draw cannot
sample a texture it also renders to -- WebGPU rejects the whole command buffer
-- so the driver snapshots the attachment and samples the copy instead. The
trigger compared **resource identity only**:

    if (ctx->colors[c] && ctx->sampler_views[slot]->texture == ctx->colors[c])

`util_gen_mipmap` generates a chain as a run of blits from level n to level
n+1 of one texture. Every one of them is the same resource on both sides, so
every one was diverted to the snapshot -- and the snapshot is a copy of
**level 0**, at level 0's dimensions. A chain generated from a non-zero
`GL_TEXTURE_BASE_LEVEL` therefore came out holding level 0 everywhere.

Neither WebGPU nor GL calls this a feedback loop: both scope the rule to a
*subresource*, so sampling one mip level while rendering into another is
legal and needs no snapshot at all. The trigger now requires the attached
level and layer to fall inside the sampled view's ranges, and the snapshot
takes `sv->u.tex.first_level` at that level's size rather than level 0.

**Why it survived so long, which is the transferable part.** The ordinary case
generates from level 0, where the wrong source and the right one are the same
texels -- so `generate_mipmap` passes whether or not the bug is there. The
first version of this probe filled the base level with a flat colour and
passed on a broken driver. Three colours are needed to see it: the readback
after a failed generate is identical for "read level 0 instead of the base"
and "the base fill leaked into the chain and the generate did nothing", and
the level *below* the generate's range is the only thing that separates them.

    tri-fbo-tex-mip   0.276008 -> 0.00045764   against both references

### The third class: the driver was never loaded

After the animation floor and the second reference, the largest divergence
left in the run was `wayfire` -- a real Wayland compositor -- at 0.576 with an
animation floor of **zero** and two references agreeing to **0.0015**. That is
the perfect signature of a real deterministic defect, and it is not one.

It renders a black screen because it never loads this driver. wlroots wants a
DRM render node; the reference run gets one from the system Mesa, and the
build under test has no such device, so wayfire logs

    Failed to open DRM render device, consider specifying WLR_RENDER_DRM_DEVICE.
    Trying SW rendering instead.

and falls back to pixman. The two runs were not comparing two drivers. Nothing
in the three numeric columns could say so -- a program that renders in
software renders *consistently* in software.

The driver announces itself on every successful init, so the absence of any
`gex-wgpu` line in the captured log is conclusive, and the harness now reports
`driver never loaded` instead of an rmse. One program in 59 hit it, and it was
top of the ranking.

### And a fourth the columns cannot see

`gltestperf` is a *performance* benchmark: it walks a sequence of test cases
and what is on screen depends on how far it got in the time it was given. Its
two images are at different stages of that sequence, not two renderings of one
scene. Its animation floor is zero because the driver progresses identically
on both of its own runs -- consistently *different* from the reference is
indistinguishable from consistently *wrong* by any of these measures.

There is no cheap column for this one. The rule it leaves behind is the same
one the whole section is about: **look at the images before believing a
number.** Every false positive in this run announced itself in one glance and
in none of the statistics.

### Where the tail stands after all four filters

Of the nine deterministic entries at the top of the comparison:

| retired as | programs |
|---|---|
| llvmpipe's own quad diagonal | `quad`, `quad-degenerate` |
| references disagree | `cubemap`, `tri-viewport`, `simplex-noise`, `textures`, `arbfplight`, `sharedtex_mt` |
| driver never loaded | `wayfire` |
| benchmark progress, not a scene | `gltestperf` |
| **fixed this pass** | **`tri-fbo-tex-mip` 0.276 -> 0.00046** |
| real, known, still open | `dlist-degenerate`, `dlist-edgeflag`, `dlist-edgeflag-dangling` |

The whole deterministic tail is now either explained or one known feature
(edge flags) away from being explained. That is a much stronger statement than
the ranking supported before, and it is the reason to spend a run on the
`refdiff` and driver-loaded columns before spending a build on a fix.

Two notes on running the harness: it now refuses to start if the display
already has windows, because "the largest window on the display" is how it
finds its subject and a foreign window defeats that. A concurrent run of nine
real applications on the same display produced a `textures` row of 0.792 whose
three images were MaterialXGraphEditor, aseprite and SuperTuxKart. And
softpipe needs a longer settle than llvmpipe; `GEX_COMPARE_DELAY=25` is enough
for the slow ones.

## glPolygonMode with the faces set differently

The last rasterizer state this driver refused, and after it the only refusals
left in the whole app sweep are render-target format probing (routine) and two
`GL_XOR` logic ops (which WebGPU genuinely has no form of). Eight programs
asked for it: `dlist-edgeflag`, `dlist-edgeflag-dangling`, `tri-edgeflag`,
`tri-edgeflag-pv`, `tri-edgeflag-array`, `tri-unfilled-smooth`, `pgon-mode`
and `vp-unfilled`.

GL lets the front and back faces be filled, outlined or stippled
independently. WebGPU has no polygon mode at all, so an unfilled polygon is
already rewritten into the lines or points it would be drawn as
(`gex_wgpu_draw_unfilled`) -- but that rewrite takes *one* mode, and the state
was refused outright when the two faces disagreed, on the reasoning that
"nothing here could honour it".

**It is two draws.** Each pass has a single fill mode and culls the other
face, so everything downstream sees a state it already handles, and the
facing test is the rasterizer's own job -- nothing has to compute a winding
and `front_ccw` keeps deciding what "front" means. The application's cull
state composes rather than being replaced: a pass whose face the application
already culls is skipped, and `PIPE_FACE_FRONT_AND_BACK` skips both.

The measured effect, against llvmpipe with the softpipe floor beside it:

| program | before | after | refdiff |
|---|---|---|---|
| `dlist-edgeflag` | 0.463 | 0.0735 | 0.0295 |
| `dlist-edgeflag-dangling` | 0.464 | 0.0465 | 0.0551 |
| `vp-unfilled` | -- | 0.0201 | 0.0201 |
| `pgon-mode` | -- | 0.0407 | 0.0368 |
| `tri-edgeflag`, `tri-edgeflag-array` | -- | 0.0376 | ~0 |
| `tri-edgeflag-pv` | -- | 0.0571 | 0 |
| `tri-unfilled-smooth` | -- | 0.0690 | 0.0402 |

Three of the eight are now at or below the floor, which is as close to
"correct" as that measurement can say.

### The gap that is left, and what closing it takes

`tests/integration/programs/gl44-polygon-mode-faces-probe.c` scores 5/6 and
the failure is real: **WebGPU applies `cullMode` only to triangle topology**,
and an unfilled pass has been rewritten into lines or points before it draws,
so its cull is inert and it emits both facings' edges. `gex_wgpu_draw_unfilled`
already documents the same gap for the uniform case -- "Lines carry no
winding [...] the alternative is culling on the CPU" -- and **one fix repairs
both**: capture the post-VS positions with the compute path the wide-line
rewrite already uses (`gex_wgpu_varying_variant_get` and the dispatch
sequence in `gex_wgpu_line_draw`), compute each source primitive's
window-space signed area, and hand the unfilled translator only the
primitives of the wanted facing. It has to filter *source* primitives rather
than the translator's output, because the rewrite deliberately runs before
the topology conversion so that a quad's four edges stay four edges.

It was worth landing the split before doing that, and the numbers are why: the
residual is the wrong facing's vertex points and edges, which mostly land on
top of geometry that is already there. A single front-facing triangle with
`glPolygonMode(GL_FRONT, GL_LINE)` and `glPolygonMode(GL_BACK, GL_POINT)` --
which is six of the eight programs -- comes out as its three edges plus three
points sitting on the edge endpoints, three pixels' worth. Before the split it
came out filled.

## dlist-degenerate: located, not fixed

The other real defect the second reference left standing, and it is not in the
driver -- the driver receives a depth clear and **no draws at all**.

`GEX_DLIST_LOG=1` (added to `vbo_save_api.c`, env-gated) says what the list
holds:

    dlist: prim 0 mode=0x9 count=2 start=0 supported=0
    dlist:   converted -> mode=0x4 count=0 index_size=2
    dlist: totals idx=0 total_vert_count=0 merged_prims=1 current_bo=NULL

One `GL_POLYGON` with **two vertices**. `vbo_save_api.c` converts a display
list's primitives against `ctx->Const.DriverSupportedPrimMask`, which comes
from the driver: llvmpipe advertises every mode and never converts, this
driver advertises five and converts POLYGON, so the primitive becomes
`GL_TRIANGLES` with a count of zero, the list records no vertices and no
indices, and `save->current_bo` stays NULL. That is what the two `ib.obj`
guards in that file were added for -- they stopped a null dereference inside
`glEndList`, which is not the same as the list being harmless afterwards. The
run also reports `GL_INVALID_OPERATION in glEnd`, a `glEnd` with no matching
`glBegin`, which the reference run does not.

`tests/integration/programs/gl44-dlist-degenerate-probe.c` does **not**
reproduce it, and that is worth knowing before trying again: eleven cases,
covering every mode this driver converts, at too-few-vertex counts, in both
orders, merged with a real primitive and alone in a list -- 11/11 on llvmpipe,
radeonsi and gex_wgpu. The reason is visible in the log: a `glBegin(GL_POLYGON)`
with two vertices written straight out never reaches `compile_vertex_list` at
all, because `_mesa_End` drops an under-length primitive first. The demo's
does. Whatever gets it past that check is the thing to find, and the probe is
the place to add it once it is known.

The probe did catch one small real divergence on the way, kept as a note
rather than a case: a three-vertex `GL_QUAD_STRIP` lights about 70 pixels on
this driver and none on either reference.

### Closing the gap: culling on the CPU, and edge flags with it

The gap above is closed. `gl44-polygon-mode-faces-probe.c` is 6/6, and the
eight programs went from 0.46 to between 0.020 and 0.069.

**The facing filter.** `gex_wgpu_cull_polygons` runs the vertex shader as
compute into a capture buffer -- the same first pass the wide-line rewrite
uses, with a one-slot layout because only `VARYING_SLOT_POS` is wanted, so the
record is 16 bytes and the position sits at offset 0 -- maps it, and decides
each source polygon's facing from the sign of its signed area after the
perspective divide. Only the vertex range the draw touches is captured, which
for an indexed draw is its index bounds.

*Source* polygons are filtered, not the translator's output: the unfilled
rewrite deliberately runs before the topology conversion so a quad's four
edges stay four edges, and the mapping from output line back to input polygon
differs per topology. Strips and fans become `TRIANGLES` and a quad strip
becomes `QUADS`, which is lossless because polygon mode's edges are per-polygon
either way. A vertex at or behind the eye makes the facing undecidable, and
such a polygon is kept rather than guessed at -- erring the way the old
behaviour did.

The sign convention was measured, not derived: two triangles of known opposite
winding, and the front/back pair and both cull cases invert together if it is
wrong, which is what makes the probe able to tell.

**Edge flags came almost free.** Mesa already delivers them: `varray.c` enables
`_PerVertexEdgeFlagsEnabled` only when the polygon mode of an unculled face is
not `FILL`, and `st_atom_shader.c` turns that into `passthrough_edgeflags`, so
a vertex shader writing `VARYING_SLOT_EDGE` is exactly the case where GL says
the flags decide which edges are drawn. Capturing that slot alongside the
position costs one more 16-byte record.

GL's rule is one predicate -- the flag on a vertex says whether the edge
*beginning* at it is a boundary edge, a `LINE` polygon draws only its boundary
edges, and a `POINT` polygon draws only the vertices that begin one -- so the
line or point list is built here rather than handed to `u_unfilled_translator`,
which has no way to be told about it. A `LINES` or `POINTS` result re-enters
the draw path directly, which also picks up the wide, smooth and stippled line
rewrites on the way.

### The bug underneath: a union, and a stage change

None of it worked at first. Every capture variant of the shader `glPolygonMode`
makes st/mesa emit was rejected:

    Tint rejected SPIR-V: Capability PhysicalStorageBufferAddresses is not
    allowed by Vulkan 1.1 specification

for a shader that touches no global memory at all. The disassembly said
`OpMemoryModel PhysicalStorageBuffer64`, and ntv chooses that from exactly one
field:

    if (s->info.cs.ptr_size == 64) {
       emit_cap(PhysicalStorageBufferAddresses);
       model = PhysicalStorageBuffer64;

`shader_info`'s stage-specific fields are a **union**. All three of this
driver's capture builders -- transform feedback, the varying capture, and the
geometry stage -- turn a vertex shader into a compute one by assigning
`info.stage`, which leaves the vertex stage's bytes to be read as compute
state. Whether it bites depends on what those bytes happened to hold, which is
why it struck one shader and not the captures that had been running all along.
All three now clear `info.cs` with the stage change.

Worth remembering for its own sake: **the debug print that would have shown
this never ran**, because a mechanical "replace the first occurrence" edit put
it in the wrong one of three near-identical builders. The rejected SPIR-V hex
the driver already dumps is what actually named it -- `spirv-dis` on it took
one command and no rebuild.

## Wide and smooth lines from an indexed draw

The wide-line rewrite took **non-indexed draws only**, and its guard said so in
one clause -- `!info->index_size` -- with no decline message, so it was silent.
The consequence was larger than it looks: `glLineWidth`, `GL_LINE_SMOOTH` and
`GL_LINE_STIPPLE` did nothing at all for *any* indexed line draw, which
includes every outline `glPolygonMode`'s unfilled rewrite emits, and which is
how most real programs draw lines.

The reason for the restriction was structural. The rewrite binds the capture
buffer twice as instance-step vertex buffers, at offset 0 and one record
further in, so a segment's two endpoints have to be a fixed distance apart in
it -- true of a non-indexed draw by construction. The capture already had the
answer for indexed ones: `gex_wgpu_capture_layout.index_table`, a slot table of
one already-resolved vertex number per captured slot, which the
transform-feedback capture has used all along. With it the capture buffer comes
out in *draw* order and the premise holds again.

The two cases added to `gl44-line-probe.c` compare an indexed draw against the
same line drawn directly -- identical geometry has to give an identical pixel
count whichever way it is submitted -- with indices deliberately out of order
and starting away from zero, so a capture that ignored them and walked the
vertices in order draws a different line and is caught. 9/9, with the indexed
wide line byte-identical to llvmpipe.

### The collision underneath

None of it worked at first: every captured position came back `(0,0,0,0)`.

    #define GEX_WGPU_CAPTURE_INDEX_BINDING (GEX_WGPU_CAPTURE_PARAMS_BINDING + 1)
    #define GEX_WGPU_VARYING_OUT_BINDING   (GEX_WGPU_CAPTURE_PARAMS_BINDING + 1)

**The same number.** Nothing noticed while no shader declared both: the
transform-feedback capture has an index table and no varying output, the
varying capture had a varying output and no index table. Teaching the line
rewrite to take indexed draws made one shader declare both, and the capture
then read its vertex numbers out of the buffer it was writing varyings into.
`VARYING_OUT` moves to `+ 2`.

Worth recording how it was found, because three plausible theories were wrong
first -- a read-only/writable binding mismatch, a buffer created with the wrong
usage, and `num_ssbos` not covering the binding. What settled it was dumping
the capture variant's NIR and reading the declarations:

    decl_var ssbo ... gex_capture_index#1 gex_capture_index (0, 0, 29)
    decl_var ssbo ... gex_varying_out#2   gex_varying_out   (0, 0, 29)

The binding numbers are printed right there. **Dump the shader before
theorising about the shader.** The first dump attempt looked at the wrong
capture -- the facing filter's, which legitimately has no index block and runs
first -- so read the *second* variant when a draw takes two captures.

### A note on measuring `tri-unfilled-smooth`

Its outline is now antialiased, as GL asks, where before `GL_LINE_SMOOTH` was
ignored. Against llvmpipe it scores *worse* for it -- 0.0690 to 0.0781 --
because our smooth coverage differs from llvmpipe's by about 3% (144 lit
pixels against 148 on the probe's diagonal), and a wrong-but-present outline
costs more RMSE than an aliased one. The driver is more correct and the number
is larger; the probe is the signal here, not the ranking.

The comparison harness also reported 0.239 for the same build that direct
capture reproduces at 0.0781 eight runs out of eight. Its image is a **partial
frame** -- both outlines drawn, the fill not yet -- and its `self` column said
so at 0.2255 in an earlier run, which was read as animation noise and was not.
`self > 0` means *unmeasurable*, whatever the cause: an animated program, or a
program slow enough that a fixed delay lands mid-frame. The facing filter's
readback is a full GPU stall per unfilled draw, which makes the second case
more likely for polygon-mode programs than it used to be.

# Above GL 3.3

The driver advertises GL 3.3 / GLSL 3.30. The project targets **4.4** -- which
is why every probe here is named `gl44-*` -- and the ceiling is 4.6. The app
targets that need it are in the goal now: Minecraft 26.2 (which wants 4.4),
modern Blender, FreeCAD, Krita.

## What the version ladder actually costs

Mesa's `_mesa_compute_version` states each rung's requirements exactly, and
diffing them against what this driver exposes gives the real roadmap rather
than an impression. To 4.4 it is 26 extensions plus GLSL 440:

| rung | missing |
|---|---|
| 4.0 | `gpu_shader5`, `gpu_shader_fp64`, `tessellation_shader`, `texture_buffer_object_rgb32` |
| 4.1 | `shader_precision`, `vertex_attrib_64bit`, `viewport_array` |
| 4.2 | `base_instance`, `shader_atomic_counters`, `shader_image_load_store` |
| 4.3 | `compute_shader`, `copy_image`, `fragment_layer_viewport`, `framebuffer_no_attachments`, `robust_buffer_access_behavior`, `shader_image_size`, `shader_storage_buffer_object`, `stencil_texturing`, `texture_buffer_range`, `texture_view` |
| 4.4 | `buffer_storage`, `enhanced_layouts`, `query_buffer_object`, `texture_mirror_clamp_to_edge`, `texture_stencil8`, `vertex_type_10f_11f_11f_rev` |

**Two of those are walls, and they sit on the lowest rung.**
`ARB_tessellation_shader` -- WebGPU has no tessellation stage at all -- and
`ARB_gpu_shader_fp64`, because WGSL has no `f64`. `ARB_vertex_attrib_64bit` is
the same f64 problem one rung up. Mesa's ladder is strict, so until those are
answered the *version number* cannot rise past 3.3 however many other
extensions land. Geometry shaders were solved by lowering to compute; that is
the shape an answer to tessellation would have to take.

**The version number may not be what the applications need, though.** Mesa
advertises extensions independently of the version, and the wire already
grants a context request it cannot literally satisfy -- SuperTuxKart's own log
says so:

    gex-wgpu: application asked for 4.4, this context is 3.3 (granted;
    GL_VERSION reports the real one)

So the practical route is feature-driven: implement the extensions modern
applications actually use, and let the version follow if tessellation and fp64
are ever answered.

## The first rung: shader storage buffers

`ARB_shader_storage_buffer_object` now works. `caps->max_shader_buffers` was
zero, which is the single number st/mesa reads to decide the extension exists.

This was the right rung to take first because WebGPU **has** storage buffers.
Everything in the legacy tail -- polygon mode, logic ops, wide lines, quads --
was emulating something WebGPU lacks; from here on the features are mostly
ones it has. The driver was also already binding storage buffers for transform
feedback, for its own capture passes, and for the uniform blocks that overflow
WebGPU's twelve-uniform limit.

`tests/integration/programs/gl44-ssbo-probe.c` is the gate: 5/5, identical to
llvmpipe -- a fragment read, a fragment write read back on the CPU, a vertex
read, and two blocks at once so a slot cannot be confused for a binding.

### Four things it needed, in the order they were found

**1. A binding range of their own.** A real storage block and a *spilled*
uniform block are both `nir_var_mem_ssbo` by the time the shader is walked --
`gex_wgpu_lower_gallium_ubos` moves uniform slots past the device limit into
storage mode -- so mode cannot tell them apart. Storage blocks are assigned
`GEX_WGPU_SSBO_FIRST_BINDING + slot`, *before* the spill runs, and that range
is what identifies them afterwards. They also get the stage's descriptor set;
without it they arrived at set 0 and were rejected as "wrong descriptor set".

**2. A table of their own.** They are bound from `ctx->shader_buffers` rather
than from the constant buffers, and block 0 of one is not block 0 of the
other, so sharing the `ubo_slots` table would have collided.

**3. The SPIR-V emitter does not implement `load_ssbo`.** It asserts:

    emit_intrinsic: not implemented (load_ssbo)
    Assertion `!"unsupported intrinsic"' failed.

What it supports is a storage block declared as a variable and reached by
deref -- which is how this driver's own capture buffers are written. So the
block is flattened to a runtime-sized `uint` array and the intrinsics become
word-indexed derefs of it, the same shape `gex_wgpu_lower_ubo_load` gives
uniform blocks, one element size down because std430 packs to words rather
than vec4s. 32-bit only, and atomics and `length()` are refused by name.

**4. The layout must agree with the shader's *access*, not just its type.**
This one produced a silent zero rather than an error. `nir_opt_access` marks a
block the shader never writes, the emitter then declares it
`var<storage, read>`, and a layout saying `Storage` does not match: the bind
group fails to build and the draw reads zeros. The fragment cases split
exactly on this -- the write case passed while both read cases returned 0 --
which is what named it. There is now an `ssbo_readonly_mask` per shader.

WGSL forbids writable storage in the **vertex** stage, so a vertex block that
is written is declined with that reason rather than left to fail at pipeline
creation. Reading one there is legal and works.

### What came with it, and the risk in that

Exposing it also lit up `ARB_shader_atomic_counters` and
`ARB_program_interface_query`, which st/mesa derives from SSBO support.
Atomic counters lower to SSBO atomics, and this pass **refuses atomics** -- so
a shader using them now fails to link where before it could not have been
written at all. That is the thing to watch in the next app sweep and CTS run;
the honest fix is SSBO atomics, which WGSL supports but only on `atomic<u32>`
storage, a different declaration from the plain array used here.

## The target applications, measured

### Krita 6.0.3 works

The whole application: the welcome screen with decoded image thumbnails, and
then the canvas -- an 800x600 image at 81.3% zoom, the GL colour wheel, layer
and brush-preset thumbnails, every docker panel. Seven screen initialisations,
sixteen shaders compiled, **zero declines and zero refusals**. It is the first
ambitious user-facing application after SuperTuxKart to run on this driver end
to end.

**It needs `QT_XCB_GL_INTEGRATION=xcb_egl`, and that is not a Krita quirk --
it is how any GLVND application has to be tested on the native path.**

Qt does not link libGL; it goes through GLVND, which means the *system*
`libGLX.so.0`. That loader looks for a DRI driver in `LIBGL_DRIVERS_PATH`,
finds this build's `swrast_dri.so`, and fails:

    glx: failed to create drisw screen

Five times, and the driver is **never reached** -- no feature request, no
screen init, nothing in the log. The reason is that `libdril_dri.so` needs
only libm and libc: it is Mesa's *stub* DRI driver, not a loader for
`libgallium`, and this build ships no `libGLX.so.0` of its own for a GLVND
application to find. Forcing Qt onto EGL routes it through the `libEGL.so.1`
this build does ship, and the driver is reached normally.

Worth knowing because the failure looks exactly like a driver bug from
outside: an application that opens its window, renders in software, and logs a
GLX message. Count `native screen on Dawn` in the log before believing the
driver was involved at all.

### Modern Blender is blocked, and says so itself

Blender 5.2.0 LTS starts, creates a context, probes formats -- and then stops
without a window and without an error. Its own binary carries the reason:

    A graphics card and driver with support for OpenGL 4.3 or higher is required.

Zero shaders compiled. It checks the version and gives up, so it is blocked on
the ladder rather than on any individual feature, and the ladder is blocked at
4.0 by tessellation and fp64.

### GL compute is not blocked after all -- nobody was asking

This section previously said compute could not be advertised, on the strength
of the device reporting 256 invocations per workgroup against the 1024
`st_extensions.c` requires. **That was wrong, and the way it was wrong is worth
keeping.**

256 is WebGPU's *default*, not the adapter's maximum. `requestDevice` may ask
for more, and `gex_wgpu_native.c` already had a `GEX_WANT` macro that requests
a limit clamped to what the adapter reports -- it simply never listed the
compute limits. The adapter here offers:

    gex-wgpu: adapter compute limits invocations/workgroup 1024
              workgroup 1024x1024x1024 shared 65536B (asking for 1024)

So the granted figure is 1024 now, and the gate is met. The lesson is narrow
and general: **a limit read back from the device is what you asked for, not
what the hardware can do.** Read the adapter before calling something a wall.

`screen->compute_caps` is filled from the granted limits rather than from a
hopeful constant, so a device that grants less still reports the truth to GL.

What remains before `ARB_compute_shader` appears is
`ARB_shader_image_load_store`: st_extensions gates compute behind it and
behind `ARB_shader_atomic_counters`, and the latter came free with SSBOs.
`launch_grid`, `create_compute_state`, `bind_compute_state` and
`set_shader_buffers` are all already implemented.

Image load/store is expressible: this Dawn has `WGPUStorageTextureAccess_ReadOnly`
and `_ReadWrite` and neither is behind a feature name. The restriction to
respect is *format* -- WebGPU allows read_write storage on a narrow set
(r32uint, r32sint, r32float) and write-only on a wider one -- so the honest
shape is to advertise per-format support through `is_format_supported` with
`PIPE_BIND_SHADER_IMAGE` rather than to claim GL's whole required list.

## Image load/store, and GL compute working

`GL_ARB_shader_image_load_store`, `GL_ARB_shader_image_size` and
**`GL_ARB_compute_shader`** are all advertised and working now, on top of the
storage buffers. Gates: `gl44-image-probe` 4/4 and
`gl44-compute-ssbo-probe` 5/5, both identical to llvmpipe.

### Compute shaders do not need GLSL 4.30

The obvious reading of *"Compute shaders require GLSL 4.30 or GLSL ES 3.10"* is
that the GLSL level has to be raised, which would mean accepting every GLSL
4.30 feature. It does not. Mesa's rule is:

    has_compute_shader() = ARB_compute_shader_enable || is_version(430, 310)

So a compute shader asking for the extension by name compiles at GLSL 3.30.
The pre-existing `gl44-compute-probe` uses `#version 430` and therefore cannot
run here at all; `gl44-compute-ssbo-probe` asks for the extension instead,
which is what makes compute usable at the level this driver reports.

### Four things gated compute, none of them the dispatch

The dispatch itself -- `launch_grid`, `create_compute_state`,
`bind_compute_state`, `set_shader_buffers` -- was already written. What was
missing:

1. **`caps->compute`.** st_extensions reads it *before* it looks at
   `compute_caps` at all, so the entire compute block was skipped however well
   its own three gates were met.

2. **`compute_caps` set after `gex_wgpu_init_caps`.** `u_init_pipe_screen_caps`
   inside it resets every cap, so filling them earlier left
   `max_threads_per_block` at zero with `ARB_compute_shader` off and all three
   gates otherwise satisfied.

3. **`MESA_SHADER_COMPUTE` in the shader-caps stage list**, which held
   VERTEX, FRAGMENT and GEOMETRY only. A stage left out of that list does not
   get "no features" -- it gets **zero**, and a zero `indirect_const_addr`
   makes `st_glsl_to_nir` ask
   `nir_lower_indirect_derefs_to_if_else_trees` to lower indirect access to
   *storage blocks*. That pass asserts `start < end` on the runtime-sized
   array a storage block is, so a compute shader indexing an SSBO crashed in
   the compiler before reaching this driver.

4. **The SSBO lowering, wired into `create_compute_state`.** That path does
   not go through `gex_wgpu_lower_gallium_ubos`, so compute shaders reached the
   SPIR-V emitter with raw `store_ssbo` and asserted. It also needs
   `nir_opt_copy_prop` and `nir_opt_constant_folding` run first: a compute
   shader arrives without the optimisation the graphics path gets, and the
   block index was a `mov` of a constant -- `%21 = mov %19.x` -- which
   `nir_src_is_const` does not see through, so the lowering refused an
   ordinary shader.

### Two things image load/store needed

**The storage usage cannot be conditioned on `PIPE_BIND_SHADER_IMAGE`,
because st/mesa never asks for it.** `st_cb_texture.c` creates every texture
with `SAMPLER_VIEW | RENDER_TARGET`, and `glBindImageTexture` arrives long
afterwards with no reallocation. So any single-sampled texture whose format
WebGPU can take as a storage texture is given `StorageBinding` -- which is
what a Vulkan driver does with `VK_IMAGE_USAGE_STORAGE_BIT` for the same
reason. Without it the failure names the bind group, not the allocation:

    [TextureView of Texture (8x8 px, R32Uint)] usage
    (CopySrc|CopyDst|TextureBinding|RenderAttachment) doesn't include
    TextureUsage::StorageBinding

**A read-only image is a sampled texture binding, not a storage one.** Tint
lowers a SPIR-V storage image that is only read to a plain `texture_2d<T>` --
WGSL reads it with `textureLoad`, which needs no sampler -- and WebGPU then
rejects a `storageTexture` layout against it by name:

    Binding type in the shader (texture) doesn't match the type in the layout
    (storageTexture).

So the layout follows what the shader was compiled *into* rather than what GL
called it. And an image used only for `imageSize()` is marked both
`NonWritable` and `NonReadable` by `nir_opt_access`, which Tint refuses
outright -- it is made read-only instead.

The format restriction is real and is respected rather than papered over:
WebGPU takes a narrow list as storage textures at all, and allows read_write
on only the 32-bit single-channel three. A shader that reads *and* writes any
other format is refused by name, and `is_format_supported` answers per format
so st/mesa disables the rest.

### Regression check

The app sweep over 336 programs is **unchanged** across the storage-buffer
work: RAN 284, REFUSED 2, CRASH 12, HARNESS 3, EXIT 19, OK 16, with zero
movement in either direction. SuperTuxKart still renders correctly. The full
probe sweep shows only the intended `quad` divergence.

## Software fp64, and GLSL 4.00 as a lever

`ARB_gpu_shader_fp64`, `ARB_vertex_attrib_64bit` and `ARB_gpu_shader5` are all
advertised now, and fp64 verifiably *works*: `gl44-fp64-probe` is 4/4 against
llvmpipe, including a case a single-precision fallback cannot pass (1.0 + 2^-30
is 1.0 in binary32 and distinguishable in binary64).

**The reason this was thought impossible is worth correcting in full**, because
it was recorded here as settled. The previous note said:

> Mesa can lower fp64 in software [...] But st only *builds* that shader when
> GLSL is 4.00 or better, and Mesa clamps the GLSL version to the GL version,
> which stays at 3.3 until the whole GL 4.0 extension set is present -- fp64
> among it. So fp64 cannot be delivered on its own.

The first half is right and the second is not. **There is no clamp.**
`st_extensions.c` does exactly this:

    consts->GLSLVersion = screen->caps.glsl_feature_level;

and nothing lowers it afterwards. So the GLSL level is a cap this driver sets
freely, the circle does not exist, and the two caps simply have to move
together -- because getting it wrong is not a soft failure: without the
support shader `nir_lower_double_ops` hits `assert(softfp64 != NULL)` and
takes the process with it.

**And the GLSL level turns out to be a lever, not just a number.** Three of the
extensions the ladder still wanted are gated on it alone:

| gate | extension |
|---|---|
| `GLSLVersion >= 400` | `ARB_gpu_shader5` |
| `GLSLVersion >= 410` | `ARB_shader_precision` |
| `GLSLVersion >= 140` + formats | `ARB_enhanced_layouts` |

Note that `GL_SHADING_LANGUAGE_VERSION` still reports 3.30 in a 3.3 context,
which is correct: the string follows the context, while the cap is what st
reads for its own decisions.

### Where the ladder stands

Measured against Mesa's own `_mesa_compute_version`, the gap to 4.4 has gone
from 26 extensions to 18:

| rung | missing |
|---|---|
| **4.0** | `ARB_tessellation_shader`, `ARB_texture_buffer_object_rgb32` |
| 4.1 | `ARB_shader_precision` (GLSL 410), `ARB_viewport_array` |
| 4.2 | `ARB_base_instance` |
| 4.3 | `copy_image`, `fragment_layer_viewport`, `framebuffer_no_attachments`, `robust_buffer_access_behavior`, `stencil_texturing`, `texture_buffer_range`, `texture_view` |
| 4.4 | `buffer_storage`, `enhanced_layouts`, `query_buffer_object`, `texture_mirror_clamp_to_edge`, `texture_stencil8`, `vertex_type_10f_11f_11f_rev` |

**4.0 is two extensions away, and one of them is the tessellator.** That is now
the single thing standing between this driver and a version number that
modern Blender will accept, and geometry shaders already show the shape of an
answer: lower it to compute.

`ARB_base_instance` is worth noting as the cheapest remaining item with a known
blocker: WebGPU's draw takes a `firstInstance`, which the driver already
passes, but SPIR-V's `InstanceIndex` counts from it while GL's `gl_InstanceID`
counts from zero. Advancing each instance-step vertex buffer by
`base * arrayStride` instead of passing `firstInstance` keeps `gl_InstanceID`
correct with no shader change.

## ARB_base_instance: GL 4.2 complete

`glDrawArraysInstancedBaseInstance` works, and **GL 4.2 now has nothing
missing**. `gl44-base-instance-probe` 3/3 against llvmpipe.

The feature is two claims that fail in opposite directions, which is why the
probe reads them separately:

- per-instance attributes are fetched starting at the base instance
- `gl_InstanceID` still counts from **zero**

WebGPU's draw takes a `firstInstance`, and this driver was already passing
GL's base through it -- which gets the attributes right and `gl_InstanceID`
wrong, because SPIR-V's `InstanceIndex` *counts from* `firstInstance`. That is
exactly why the cap had been left off, with a comment saying the two "must
move together".

They do, but not in the shader. **The base is folded into the offsets of the
instance-stepped vertex buffers** -- `offset += start_instance * arrayStride`
-- and the draw is issued from instance zero. The fetch then starts where GL
says and the builtin still counts from zero, with no shader change and no
per-draw uniform. A gathered slot is left alone, since the de-indexing gather
has already resolved the draw's addressing.

The buffer-overrun check above the bind loop still computes
`start_instance + instance_count - 1`, which stays correct: it bounds the
*absolute* last element against the buffer, and folding the base into the
offset does not move where that element lives.

The probe's colour comes from a per-instance attribute and its position from
`gl_InstanceID`, so a driver that passes the base through draws the right
colours in the wrong places, and one that ignores the base draws the wrong
colours in the right places.

### The ladder now

| rung | missing |
|---|---|
| **4.0** | `tessellation_shader`, `texture_buffer_object_rgb32` |
| 4.1 | `shader_precision` (GLSL 410 alone), `viewport_array` |
| **4.2** | **complete** |
| 4.3 | 7 |
| 4.4 | 6 |

Seventeen extensions to 4.4, from twenty-six. `ARB_shader_precision` is
another pure GLSL-level item -- raising the cap to 410 grants it -- which
would leave 4.1 wanting only `ARB_viewport_array`, and WebGPU has a single
viewport.

## Texture buffers: GL 4.0 is one extension away

`samplerBuffer` and `texelFetch` work. `gl44-texture-buffer-probe` is 4/4
against llvmpipe -- rgba32f, rgb32f, r32f and r32i -- and this closes the hole
the probe was written to document: a shader using core GL 3.1 functionality
that failed to *link* on a driver claiming 3.3.

It also takes two extensions off the ladder. `ARB_texture_buffer_object_rgb32`
was one of GL 4.0's two remaining items and `ARB_texture_buffer_range` was one
of 4.3's, so:

| rung | missing |
|---|---|
| **4.0** | **`ARB_tessellation_shader`** -- and nothing else |
| 4.1 | `shader_precision` (pure GLSL-level), `viewport_array` |
| 4.2 | complete |
| 4.3 | 6 |
| 4.4 | 6 |

Fifteen to 4.4, from twenty-six.

### How it works

WebGPU has no texel buffer of any kind, so a texture buffer is bound as a
**read-only storage buffer** and `texelFetch(buf, i)` becomes a run of word
loads at `i * words` -- see `gex_wgpu_lower_buffer_texture`. Only the
32-bit-per-channel formats are served, which makes a texel a run of words and
the fetch a load plus a reinterpretation: NIR values are untyped bit patterns,
so nothing is converted and only the *padding* differs between a float and an
integer sampler (`(0,0,0,1)` as floats or as integers). The 8- and 16-bit
formats GL also allows would each need their own unpack;
`is_format_supported` is asked per format, so refusing them is visible to
st/mesa rather than silent.

### The one real design problem

**The texel's word count is not known when the shader is compiled.** A
`samplerBuffer` yields a `vec4` whatever the format holds, and the format
lives on the sampler view. So the lowering runs in the **per-draw variant
builders** and the word counts are part of both stages' variant keys --
`gex_wgpu_interp_variant_get` for the vertex stage and
`gex_wgpu_fragment_module` for the fragment one. Doing it at shader creation
is not merely wrong, it is impossible.

That also means **neither stage can have a base module** for such a shader.
Compiling eagerly does not produce a merely-unspecialised module; it produces
one Tint refuses outright, because a `samplerBuffer` reaches SPIR-V as an image
of dimension `Buffer`:

    Tint rejected SPIR-V: error: invalid dimension: 5

which arrives at the application as a link error -- exactly the symptom the
probe documents. The existing `deferred_integer_sampling` path already had this
shape for integer sampling and now covers buffer textures too.

### Two traps

**A buffer sampler view has to leave before the texture guards.**
`create_sampler_view`'s checks read `templ->u.tex`, and for a buffer view that
union member holds the offset and size instead -- so a level range read out of
it is nonsense. The buffer case returns early with no `WGPUTextureView` at
all, and the bind path recognises `target == PIPE_BUFFER`.

**Removing the tex instruction leaves its `deref_var` behind**, and dropping
the variable then leaves that deref pointing at nothing. NIR validation
catches it *inside ntv*, after ntv's own copy-prop:

    deref_var &gex_texture0 (uniform textureBuffer) error: entry

One of the four cases passed anyway because DCE happened to run first, which
made it look like a format problem rather than an ordering one. `nir_opt_dce`
now runs before the variables are dropped.

## Stencil as data: ARB_stencil_texturing and ARB_texture_stencil8

`gl44-stencil-texture-probe` is 4/4 against llvmpipe and the two extensions are
advertised, which takes GL 4.3 from six missing items to four and the total to
4.4 from fifteen to **thirteen**.

They arrive as a pair because st/mesa lights both from one
`is_format_supported` question about `X24S8_UINT` or `S8X24_UINT` as a sampler
view, so there is no way to have one without the other.

WebGPU has all the pieces: `stencil8` is a real texture format, a depth-stencil
view takes `aspect: "stencil-only"`, and such a view binds as an unfilterable
`texture_2d<u32>`. It was a wiring exercise, and the wiring was four things:

- **`gex_wgpu_is_stencil_only_format`**, because
  `util_format_is_depth_or_stencil` says yes to all four spellings and every
  place that asks "depth or stencil?" has to distinguish them. Asking it alone
  gave a stencil view the `Depth` sample type, and the draw was then refused
  for a sample type differing from the shader's `usampler2D`.
- **A stencil-aspect view names a different format from its texture by
  construction** -- `X24S8_UINT` of a `Z24_UNORM_S8_UINT` texture -- so the
  sRGB-pairing rule in `create_sampler_view` could not stay the only exception
  to the format-equality guard.
- **`stencil8` takes a buffer copy.** The spec's depth-stencil table gives it
  CopySrc and CopyDst, unlike `depth24plus` and `depth32float`, so a
  `GL_STENCIL_INDEX8` upload is an ordinary texel copy and skips the driver's
  render-pass depth-upload detour, which cannot serve it.
- **A stencil-only attachment has no depth half to load or store**, and WebGPU
  says so as a validation error rather than ignoring the ops:

      Both depthLoadOp (LoadOp::Load) and depthStoreOp (StoreOp::Store) must
      not be set if the attachment has no depth aspect

  Every attachment in this driver had a depth aspect until `GL_STENCIL_INDEX8`
  became a texture. Two sites needed the guard: the clear path and the render
  pass.

The probe's own trap is worth keeping: **`glGetString(GL_EXTENSIONS)` is removed
in a core profile** and raises `GL_INVALID_ENUM`, so it reads as "no
extensions" on a driver that has them. llvmpipe reported GL 4.6 core and zero
for both extensions until the indexed `glGetStringi` query went in.

## Correction: Mesa *does* clamp the GLSL version to the GL version

An earlier note in this document said it does not, on the strength of
`st_extensions.c` doing `consts->GLSLVersion = screen->caps.glsl_feature_level`
with nothing lowering it. That assignment is real and there is no clamp *there*
-- but `_mesa_compute_version` runs afterwards and overwrites it
(`src/mesa/main/version.c:623`):

    default:
       if (ctx->Version >= 33)
          ctx->Const.GLSLVersion = ctx->Version * 10;

So in the 3.3 context this driver is granted, `ctx->Const.GLSLVersion` is
**330**, whatever the cap says. The list of versions the compiler will accept
is built from it, which is why `gl44-sampling-matrix-probe` still says

    error: GLSL 4.40 is not supported. Supported versions are: 1.10, 1.20,
    1.30, 1.40, 1.50, 3.30, 1.00 ES, and 3.00 ES

while `caps->glsl_feature_level` has been 400 since fp64 landed.

**What the cap does and does not do.** It is read *before* the clamp, by
`st_init_extensions`, so it still gates extensions by GLSL level --
`ARB_gpu_shader5` at 400, `ARB_shader_precision` at 410 -- and it is what each
rung of `_mesa_compute_version` tests. It is a precondition declaration, not a
lever on what a shader may `#version`. Raising it cannot make a 4.x shader
compile; only granting the GL version can, and the clamp then sets the level
from the version regardless of the cap.

**Which settles the priority question.** Modern Blender and Minecraft 26.2 need
shaders above `#version 330`. That needs the GL version. The GL version needs
GL 4.0. GL 4.0 needs exactly one thing:

    tests/gl-version-ladder.sh
    GL 4.0: ARB_tessellation_shader

There is no way around it and nothing else on the ladder substitutes for it.

`tests/gl-version-ladder.sh` is new, and reports the GLSL requirement per rung
alongside the extensions for exactly this reason. Its first version reported
every rung complete: it looked for `ver_4_N:` labels, `version.c` has
`const bool ver_4_N = (...)` expressions, and an empty requirement list reads
as nothing missing. A measurement tool that cannot find its input has to say
so, so it now exits rather than printing a clean sheet.

## GL 4.0. Tessellation works.

`gl44-tessellation-probe` is **12/12** against llvmpipe, every pinned value
matched, and the driver now reports:

    OpenGL core profile version string: 4.0 (Core Profile)
    OpenGL core profile shading language version string: 4.00

`tests/gl-version-ladder.sh` says `GL 4.0: ** COMPLETE **`.

**What that buys is not the number.** Mesa clamps the usable GLSL version to
the *granted* GL version, so until this landed nothing could declare above
`#version 330` however high `caps->glsl_feature_level` was set. GLSL 400 is now
reachable, which is what modern application shaders actually need.

### Five passes

WebGPU has no tessellation stage, and unlike the geometry stage there is a
**fixed-function piece in the middle**. So:

1. the application's vertex shader, as compute, writing the control shader's
   inputs -- the geometry path's first pass, unchanged;
2. the **control shader as compute: one workgroup per patch, `vertices_out`
   invocations in it**. That shape is the whole reason for it: `barrier()` is
   then a workgroup barrier and needs no rewriting, `gl_InvocationID` is the
   local invocation index, and one patch's invocations can read each other's
   outputs -- which is what a control shader is *for*;
3. **the tessellator, on the CPU**, from the levels pass 2 wrote;
4. the evaluation shader as compute, one invocation per generated vertex;
5. an ordinary indexed draw of what pass 4 wrote, through the geometry path's
   passthrough vertex shader.

### Pass 3 is a stall, and it is the right answer anyway

Reading the levels back is a GPU-to-CPU round trip per draw. It is also the
only honest option available: the topology depends on numbers a *shader*
computed, and there is no way to know how many vertices a patch will generate
without them.

What makes it more than acceptable is *which* tessellator:
`src/gallium/auxiliary/tessellator` is the same implementation llvmpipe uses.
The generated mesh is therefore **identical** to the reference rather than
merely similar -- which is why the probe can pin exact pixel values across all
three spacings and a mixed set of levels and have them match. A WGSL port would
have had to reproduce §11.2.2 of the specification exactly to reach the same
place.

It had to be moved out of meson's `draw_with_llvm` block to be reachable. It
has no LLVM dependency -- `u_math`, `u_memory`, `p_defines` and `<cmath>` --
and was only in there because the draw module was its only consumer, so
`p_tess_init` was simply absent from a build without gallivm.

### Three traps, and one of them is a naming lie

**`p_tess_init`'s third argument is spelled `tes_vertex_order_cw` and behaves
as counter-clockwise.** The draw module passes `!shader->vertex_order_cw` into
it (`draw_tess.c:365`). Passing GL's `cw` straight in inverts every generated
triangle -- the driver drew the `cw` patch and culled the `ccw` one, exactly
backwards, with the flag reaching the tessellator correctly all along.

What made that findable in one run is that the probe has **both** cases with
the same culling. A single `cw` case can only say "wrong"; "ignored" and
"inverted" are the same failure until something distinguishes them, and they
have completely different causes.

**`caps->glsl_tess_levels_as_inputs` is not a preference.** `gl_nir_linker`
asserts on the combination of `ARB_tessellation_shader`, `compact_arrays`
(which this driver's NIR options set) and that cap being false -- "the NIR
linker doesn't support linking these as compat arrays of sysvals" -- and it
fires during *linking*, so the first tessellated program aborts the process. It
is also the shape this driver wants: a patch input is a variable with a
location, which the lowering already addresses; a system value would be an
intrinsic with no buffer behind it.

**Both tessellation stages needed permission to hold constant buffers.**
`gex_wgpu_set_constant_buffer` refused every stage but vertex, fragment and
geometry, and a refusal latches. A control shader computing its levels from a
uniform is the ordinary case, not a corner, so the first real tessellation
program produced "first refusal at gex_wgpu_set_constant_buffer" and then
`GL_OUT_OF_MEMORY` out of every `glReadPixels`, with nothing said about
tessellation at all.

### What it does not do yet

Named refusals rather than silence, all through `GEX_TESS_DECLINE`
(`GEX_WGPU_TESS_LOG=1` prints what each draw was handed): an indexed or
instanced patch draw, a geometry shader after tessellation, and a patch draw
with no control shader (GL allows the control stage to be absent, with the
levels coming from `GL_PATCH_DEFAULT_OUTER_LEVEL`). Transform feedback from a
tessellation stage is not captured, on the same terms as the geometry stage's.

### The ladder now

| rung | missing |
|---|---|
| **4.0** | **complete** |
| 4.1 | `shader_precision`, `viewport_array`, a GLSL 410 cap |
| 4.2 | a GLSL 420 cap |
| 4.3 | `copy_image`, `fragment_layer_viewport`, `framebuffer_no_attachments`, `robust_buffer_access_behavior`, `texture_view`, a GLSL 430 cap |
| 4.4 | `buffer_storage`, `enhanced_layouts`, `query_buffer_object`, `texture_mirror_clamp_to_edge`, `vertex_type_10f_11f_11f_rev`, a GLSL 440 cap |

`ARB_shader_precision` is a pure GLSL-410 lever, so 4.1 is really
`ARB_viewport_array` plus a cap bump -- and `ARB_fragment_layer_viewport` (4.3)
comes from the same cap, which makes multiple viewports the next thing worth
doing.

## Three more of GL 4.3's five

`ARB_robust_buffer_access_behavior` (5/5), `ARB_framebuffer_no_attachments`
(5/5) and `ARB_copy_image` (6/6), all against llvmpipe. **GL 4.3 is now two
extensions away** -- `ARB_texture_view` and `ARB_fragment_layer_viewport`, and
the second comes free with `ARB_viewport_array`, which is also 4.1's only real
item. Two features stand between this driver and the OpenGL 4.3 floor Blender
refuses to start below.

### Robust buffer access is a claim, not code

The extension promises that an out-of-bounds buffer access from a shader
cannot crash, cannot read another buffer and cannot write outside its own; the
value it returns is explicitly undefined. **WebGPU makes exactly that
guarantee by construction** -- its specification defines every out-of-bounds
buffer access as returning zero or being discarded, and an implementation that
let one escape would be a security bug rather than a conformance one.

So this is a cap and a verification, not an implementation. What the probe
checks is that *this driver* does not defeat the platform's promise -- a
CPU-side index clamp with signed arithmetic, or a binding sized from the
shader's declaration rather than from the buffer, would -- which is why every
case pairs its out-of-bounds access with a **sentinel in the buffer allocated
right after it**: a storage read and write past the end, the same read through
a uniform block, a `texelFetch` past the end of a texture buffer (a different
path here, since those are served as storage reads), and an index list running
past the end of a vertex buffer followed by an ordinary draw that has to still
work.

### An attachment-less framebuffer gets a stand-in

A framebuffer with nothing attached is complete in GL once
`GL_FRAMEBUFFER_DEFAULT_WIDTH`/`_HEIGHT` are set, and rendering into it runs
the fragment shader over the covered area for its **side effects** -- image
stores, storage writes, atomic counters. Voxel cone tracing, GPU-driven
culling and per-pixel linked lists are all built on it.

WebGPU's render pass needs at least one attachment, so one R8_UNORM target is
manufactured at the default size and thrown away. Standing it in as *colour
attachment zero* is what makes this small: the views, the pass, the pipeline
and the viewport bounds all work unchanged. Only one thing had to change --
**the write mask is forced to zero**, because WebGPU requires a colour target
the fragment shader does not write to be masked out, and a shader rendering
into a framebuffer with no attachments writes nothing by definition. Leaving
the application's `glColorMask` there is a validation error, and a validation
error poisons the device.

A layered default (`GL_FRAMEBUFFER_DEFAULT_LAYERS > 1`) rasterizes layer zero
and says so in the log; WebGPU attaches one layer at a time.

### Cross-format copies go through util_resource_copy_region

`glCopyImageSubData` copies between images of the same bytes *per block*
rather than the same format -- DXT1 and RG32UI are both eight bytes, so the
specification pairs them, and a 16x16 DXT1 image corresponds to a 4x4 RG32UI
one. st/mesa hands such a copy straight to `resource_copy_region`
(`st_cb_copyimage.c`'s `copy_image`, whose first branch covers "either side is
compressed" as well as equal formats). WebGPU's texture-to-texture copy
demands identical formats, so there is no command for it.

`util_resource_copy_region` is the command: it maps both sides and copies
blocks, and it already knows the whole rule -- shrink the destination box by
the source's block size when the source is compressed, expand it by the
destination's when the destination is, refuse when the block sizes disagree.
Writing that against a staging buffer would have been a WebGPU round trip per
row *and* the same arithmetic a second time. It is a CPU round trip, which for
`glCopyImageSubData` is the right trade: texture streaming calls it once per
promoted tile, not once per draw. Equal formats keep the GPU path.

### The probe bug worth keeping

`gl44-fbo-no-attachments-probe` first counted fragments with an `atomicAdd`
into a storage buffer, and reported **zero fragments for a framebuffer that
was working**: this driver refuses storage atomics, so the shader failed to
link. One probe, two features, and the failure named neither.

It now has each fragment mark *its own* slot -- one fragment per slot, so no
atomic is needed -- and the CPU counts the marks and computes the covered
rectangle. That is strictly stronger than counting: the viewport case now
checks that the covered box is `0,0..15,7` and not merely that 128 pixels were
touched, which is what would catch a stand-in target sized from the viewport
rather than from the default.

**Storage atomics remain unimplemented and are now known to matter.**
`ARB_shader_atomic_counters` is advertised (st derives it from SSBO support)
and atomic counters lower to SSBO atomics, so a shader using either fails to
link. WGSL has atomics but only on `atomic<u32>` storage, which is a different
declaration from the plain word array the lowering emits.

### Verified in a browser, and one thing only the browser caught

Every feature this session landed was re-run on the wasm path, not just native
Dawn:

| probe | native | browser |
|---|---|---|
| `tessellation` | 12/12 | **12/12**, every pinned pixel identical |
| `texture-buffer` | 4/4 | 4/4 |
| `stencil-texture` | 4/4 | 3/4 → 4/4 after a fix |
| `robust-access` | 5/5 | 5/5 |
| `fbo-no-attachments` | 5/5 | 5/5 |
| `copy-image` | 6/6 | 6/6 |

SuperTuxKart in a real browser: **58.7 FPS, 97.8% fresh frames, one rAF miss.**

The one divergence is worth reading, because it is a defect this driver has
already had and documented once. `gex_wgpu_clear_depth_stencil` builds its
attachment with `WGPU_..._INIT`, which leaves `depthClearValue` **NaN**, and
the new stencil-only path set the value only inside the `has_depth` branch. So
a stencil8 clear reached `beginRenderPass` carrying a NaN it never intended to
use.

Native Dawn accepts that. The browser reads the value off the dictionary and
validates it as a float whatever the load op says:

    Failed to read the 'depthClearValue' property from
    'GPURenderPassDepthStencilAttachment': The provided float value is
    non-finite.

...which takes the whole pass. The render pass's own depth attachment already
carries a comment about exactly this ("Unconditionally, and outside the branch
below"), and it was reintroduced somewhere new. **A finite value belongs on
every dictionary field the browser will read, whether or not the op that uses
it is set** -- and native Dawn cannot be trusted to say so.

## GL 4.3, and what Blender says next

    OpenGL core profile version string: 4.3 (Core Profile)
    OpenGL core profile shading language version string: 4.30

Blender 5.2 now gets past its version gate, compiles every shader it has, and
reports its own two remaining requirements. That is the useful outcome: the
question moved from "what version" to "which capability".

Landed to get here:

- **`ARB_viewport_array`** (6/6) and with it **`ARB_fragment_layer_viewport`**.
- **`ARB_texture_view`** (5/6).
- **Shader storage atomics** (7/7), which were a hole in a claim already made.
- **GLSL 4.30** as the cap, and **the texture limits taken from the device**.
- A **Tint crash** on non-finite constants, fixed in ntv.

### One viewport is not an approximation; routing is the only gap

GL rasterizes a primitive with no shader-written `gl_ViewportIndex` through
viewport **zero**, so a driver with WebGPU's single viewport is *correct* for
every program that does not route -- which is nearly all of them. The other
fifteen slots still have to be **stored**, because st sets every slot at
context setup and refusing them is not a missing feature but a dead context.

Routing is implemented too: `gex_wgpu_gs_route_viewports` reads the geometry
pass's index list and the viewport slot each triangle asked for back off the
GPU, **sorts the triangles by viewport**, and draws them in runs with each
viewport installed in turn. Same trade as the tessellator's level readback,
conditioned on the shader actually writing the index.

The trap: `gl_ViewportIndex` and `gl_Layer` are shader *outputs* but not
varyings, and handing them to the passthrough vertex shader fails outright --
ntv recomputes a varying's `@location` from its slot, so `VARYING_SLOT_VIEWPORT`
becomes `@location(23)`, past WebGPU's sixteen inter-stage locations. They keep
their place in the vertex buffer (the routing reads one of them from there) and
are excluded from the passthrough. That is the same trap `gl_PointCoord` hit at
`VARYING_SLOT_PNTC`.

### Texture views: everything but cross-type formats

A `GPUTextureView` carries its own `dimension`, base layer and layer count, so
level ranges, layer ranges and *target* reinterpretation map straight across --
one array layer as a 2D texture, a cube map's faces as an array, an array's
layers as a cube. What WebGPU has no form of is a view format outside the
texture's own sRGB pair, so an `RGBA8UI` view of an `RGBA8` texture is refused
by name (5/6 on the probe, and the sixth case is exactly that).

The guards had to move from the *texture's* target to the **view's**: a
one-layer view of an array is a 2D view whose single layer is whichever one it
names, and demanding layer zero of it refused the case the extension exists for.

### Storage atomics were a hole in a claim already made

`ARB_shader_atomic_counters` has been advertised since storage buffers landed
-- st derives it from them -- and any shader using an atomic failed to link.
`gl44-ssbo-atomic-probe` is 7/7 including GL's own atomic counters and a mixed
plain-store-plus-atomic-add on one buffer.

The lowering is small because ntv was already ready: `nir_intrinsic_ssbo_atomic`
becomes the **deref form** of itself on the flattened word array, and Tint's
SPIR-V reader turns a variable an `OpAtomic` addresses into `atomic<u32>`,
which is the only shape WGSL has. Nothing in the driver has to know which
operation it is -- the `ATOMIC_OP` index travels across unchanged.

### The texture limits decide the GL version

`_mesa_compute_version`'s 4.1 rung requires `MaxTextureSize >= 16384`,
`MaxRenderbufferSize >= 16384`, `MaxCubeTextureLevels >= 15` and
`MaxArrayTextureLayers >= 2048`. None is an extension. WebGPU *guarantees*
8192 and 256 layers, and the driver was reporting the guarantee -- so it was
capped at GL 4.0 however many extensions it implemented, and no amount of
feature work would have moved it.

The device is now asked for 16384 and 2048 at creation and the caps follow
what it granted, so **the reported GL version is device-dependent**: this
machine reports 4.3, a device stuck at the WebGPU floor reports 4.0. That is
what every hardware Mesa driver does, and the alternative is claiming a size
the device will reject at allocation.

`tests/gl-version-ladder.sh` reports these limits per rung now. It had said
"GL 4.3 complete" while glxinfo said 4.0, because it only read the
`extensions->` terms of a conjunction -- an incomplete reading of a rung is
worse than no reading, so it prints the numeric requirements too.

### The Tint fix worth remembering: no non-finite constants

Blender writes `gl_Position = vec4(uintBitsToFloat(0x7FC00000u))` to discard a
primitive -- a NaN position fails every clip comparison. **WGSL has no
non-finite literal**, and Tint rejects any *constant expression* evaluating to
one. At first it did worse than reject:

    tint/lang/core/constant/scalar.h:59 internal compiler error:
    TINT_ASSERT(std::isfinite(v.value))

...which killed the whole process during Blender's shader compilation.

Emitting the value as an integer constant plus `OpBitcast` is **not enough**:
Tint folds the bitcast and rejects the result. A NaN produced at *run time* is
perfectly legal, so `emit_unfoldable_zero` in ntv creates a function-local
`u32` variable, stores zero into it, loads it back, and ORs it into the bit
pattern -- a `var` in WGSL is not a constant expression. Every component of a
vector has to go through it, because one constant component would let the whole
composite fold again.

### What Blender wants next

    Warning: Unsupported platform as it supports max 8 SSBO binding locations
    Error: The OpenGL implementation doesn't support ARB_shader_draw_parameters
    Blender - Platform Unsupported

`ARB_shader_draw_parameters` is the hard one: `gl_BaseVertexARB`,
`gl_BaseInstanceARB` and `gl_DrawIDARB`. WGSL has no builtin for any of them --
`instance_index` counts from `firstInstance`, which this driver folds into the
vertex buffer offsets, so the base is not derivable in the shader. All three
are per-draw scalars the driver already knows, so the answer is a driver-owned
uniform the vertex module reads, not a shader variant keyed on the values.

The SSBO count is a warning, not the refusal. st reserves *half* of
`max_shader_buffers` for atomic counter buffers, so the device's ten became
five blocks; the cap now claims twice the device budget, which yields eight.
Blender wants more than eight, which the device's ten cannot give under that
split.

## Blender draws its interface

Blender 5.2 now passes its platform check, links and runs its UI shaders, and
**draws its own interface** -- the header, the toolbars, the sidebars, each
with the right viewport and scissor. It dies on one shader family, named below.

Five things got it there.

**`ARB_shader_draw_parameters`** (`gl44-draw-parameters-probe`, 5/5), which was
the capability Blender named. `gl_BaseVertexARB`, `gl_BaseInstanceARB` and
`gl_DrawIDARB` have no WGSL builtin -- `instance_index` counts from
`firstInstance`, which this driver folds into the vertex buffer offsets, so the
base is not even visible there. All three are per-draw scalars, so they travel
in a driver-owned read-only block in the vertex group, **suballocated from the
same constant ring the uniforms use**: the binding stays one stable buffer and
only the dynamic offset changes, which is what keeps the bind group cached. A
variant keyed on the values would recompile for every draw of a batched
renderer, which is exactly the renderer that reads them.

Four traps, in the order they appeared:

1. **The block's type is rewritten if it is declared too early.**
   `gex_wgpu_flatten_ssbo_blocks` runs *inside* `gex_wgpu_lower_gallium_ubos`
   and reshapes every storage block above `GEX_WGPU_SSBO_FIRST_BINDING`, so a
   deref built before it failed NIR validation inside ntv --
   `deref_var &gex_draw_params ... error: instr->type == instr->var->type`.
   It is declared after that lowering, and after the capture and varying
   clones, which become *compute* variants that would otherwise carry a
   storage block nothing fills.
2. **It must not be collected as an application storage block.** The bind
   group then had two entries at the same binding: "binding index (60) was
   specified by a previous entry", plus "vertex storage block 0 has no buffer
   bound".
3. **`gl_BaseVertexARB` is not the first vertex.** The specification says it is
   *zero* for a command with no basevertex parameter, while NIR's
   `load_first_vertex` -- the internal value `gl_VertexID` is derived from --
   *is* `first`. They are separate words. The probe carries a case for each,
   and a fifth that reads `gl_VertexID` and `gl_BaseVertexARB` in one shader,
   which is what catches sharing one word between them.
4. **The base vertex is gone by the time the bind path runs.** The
   provoking-vertex and unfilled rewrites build a fresh index list with the
   base folded into the indices and `index_bias` reset -- correct for the draw,
   wrong for the value the shader asked for. A `flat` varying is enough to
   trigger it. The parameters are captured at the outermost `draw_vbo`, where
   `ctx->draw_depth == 0` already means "as the application issued it".

**`gl_DrawID` counts across a multi-draw.** The driver's multi-draw loop passed
`drawid_offset` unchanged to every sub-draw, so a `glMultiDrawArrays` gave
them all the same id. Gallium says which loops increment with
`info->increment_draw_id` -- the same loop also serves the primitive-restart
splitter, whose ranges came from one draw and must share its id.

**A clear colour has to be finite.** GL puts no constraint on `glClearColor`;
for a normalised target the value is clamped when the clear happens, so a NaN
is unobservable. WebGPU validates the dictionary and rejects the *whole render
pass*: "Color clearValue ([Color r:nan, ...]) contains a NaN or Inf". Blender
clears with NaN during startup. Non-finite components now become 0, and an
infinity clamps -- which is what a normalised target would have produced.

**The texture creation limits had to follow the device too.** Taking the caps
from the device (for GL 4.1's 16384) left `resource_create` refusing at a
hardcoded 8192, and a refused allocation is `GL_OUT_OF_MEMORY` from whatever
call made it -- Blender's log had only "GL_OUT_OF_MEMORY in glTexStorage2D" to
go on. Both follow `screen->max_texture_*` now, and a refused texture
allocation is named once with its format and shape.

**Sampler-less texture queries.** `textureSize`, `textureQueryLevels` and
`textureSamples` reach NIR as `txs`, `query_levels` and `texture_samples`, none
of which carries a sampler deref -- and the metadata check refused any tex op
without one that was not `txf`. The diagnostic named none of it, because the
refusal happens before the slot masks are filled and the masks it printed were
perfectly consistent; it names the op now.

### The SSBO count, and an over-claim stated plainly

Blender wants twelve SSBO binding locations. st/mesa reserves **half** of
`max_shader_buffers` for atomic counter buffers, and Dawn's ceiling for
`maxStorageBuffersPerShaderStage` is **ten** -- so the honest post-split
number is five, and even claiming twice the device budget reaches only ten.

The cap now claims 24, which yields twelve. That is an over-claim: a shader
using more than ten storage bindings in one stage asks for more than the
device has, and WebGPU rejects the bind group -- which the draw path reports
as `bind=failed` rather than mis-rendering. Understating the driver by half
to avoid a combination nothing does was the worse trade, and the alternative
to the over-claim is that Blender does not start at all.

### What is left

    gex-wgpu: texture metadata invalid (texop=-1) textures=3 samplers=3
    texture-used=0x07 sampler-used=0x07 shadow=0x00 float=0x07 uint=0x00
    sint=0x01
    gex-wgpu: shader rejected: gex_wgpu requires dereferenced, consistently
    typed texture slots
    ERROR workbench_resolve_opaque_studio_no_cavity_no_curvature_no_shado

**Slot 0 is sampled as both a float and a signed integer in one shader**
(`float=0x07`, `sint=0x01`). The metadata check requires one type per slot,
because the bind group layout names one sample type per binding -- so two
sampler variables sharing a driver location with different base types is a
real conflict, not a bookkeeping one. Whether Blender declares two samplers
that st collapsed onto one unit, or the shader reads one texture through two
differently-typed samplers, is the next thing to find out: dump the shader
before theorising about it.

## Blender: the UI draws, the viewport is blocked on WebGPU

Six more fixes past the platform check, and Blender's shaders now all link.
Where it stops is worth stating precisely, because it is not a driver gap:

    gex-wgpu: colour target 1 refused (surface reinterprets the format)
    fb=1048x900 surface=r16_uint texture=r16_float 1048x900

    gex-wgpu: sampler view refused (shape or format)
    format=r16g16_float texture=r32_float

Blender's viewport renders integer object IDs into a float texture and reads
one format's storage as another. **WebGPU allows a view's format to differ
from its texture's only by the sRGB suffix** -- that is the whole of its
format-reinterpretation rule, for attachments and for sampled views alike. So
the read side could be emulated with a per-draw shader conversion (see
`ARB_texture_view` above, whose sixth probe case is exactly this), and the
*write* side cannot: a fragment shader writing an f16 attachment goes through
WebGPU's own conversion, and there is no way to make it deposit an arbitrary
bit pattern.

That is a platform ceiling, not a missing feature, and it is where Blender's
3D viewport stops. Its interface -- header, toolbars, sidebars, text, widgets
-- draws.

### The six fixes

**Uniform blocks in compute shaders.** `create_compute_state` hand-rolled the
storage half of the block lowering and skipped the uniform half, so a compute
shader with a uniform block reached ntv with a raw `load_ubo` and *asserted*.
Patching that in separately then grew its own, lower limits -- "compute
uniform block slot=12 above max=12", "compute storage block slot=4 above
max=4" -- on shaders the graphics path takes without complaint. It calls
`gex_wgpu_lower_gallium_ubos` now, so the limits and the spill to storage
above the device's uniform count come for free, and `launch_grid` binds one
table of blocks that records where each buffer comes from rather than three
parallel notions of "slot".

**`set_constant_buffer` had to accept the compute stage**, which it refused --
and a refusal latches.

**A dynamically indexed scalar UBO load.** std140 pads an array of vec4 so the
element index is `offset/16`, but a *scalar* array indexed at run time arrives
with `align_mul = 4` and the component has to be computed too. The lowering
computes it now and extracts dynamically; the guard that remains is that the
load must not straddle two vec4s, which the alignment guarantees.

**A query is not a sample.** `textureSize()` returns an `ivec2`, and the
texture-metadata scan read that as the texture's *sample type* -- so a shader
that both samples a float texture and asks its size declared slot 0 as float
and sint at once and was refused for an inconsistency it did not have.
Blender's whole `workbench_resolve` family does exactly that. The masks pick
the bind group's sample type, which only a sampling op constrains.

**Non-finite bits in an *integer* constant.** The earlier fix covered float
constants; `uintBitsToFloat(0x7FC00000u)` -- how Blender spells NaN -- reaches
NIR as a **uint** constant with an implicit bitcast, so ntv emitted
`OpBitcast %float %const` at the use site and Tint folded it straight back into
the value it rejects. Any 32-bit constant whose bits are a non-finite float is
made unfoldable now, whatever type it is used as. The OR that does that
produces an unsigned result, so anything else is bitcast back -- storing a
uint-typed id against a signed type made a later `OpCompositeConstruct` mix the
two ("Expected Constituents to be scalars or vectors of the same type as
Result Type components").

**Two diagnostics learnt to name things**: the refused colour target prints
format *names* rather than `pipe_format` numbers (which is how a
`surface=r16_uint texture=r16_float` pair was mistaken for something exotic),
and a refused texture allocation says its format and shape at all.

### A process note

One of these edits replaced a span whose end anchor matched a *later*
occurrence, deleting a chunk of `launch_grid` and the top of the depth-resolve
pipeline. The compiler caught it immediately -- four `redefinition of` errors
-- and the recovery was `git checkout HEAD --` on the one file plus re-applying
two small patches by hand. **Anchor a span replacement on strings that are
unique in the file, and check the size of what is being replaced**; a
mechanical edit that silently eats 1500 lines is a worse failure than one that
does not compile.

## GL 4.4. Minecraft 26.2's floor.

    OpenGL core profile version string: 4.4 (Core Profile)
    OpenGL core profile shading language version string: 4.40

All five of 4.4's extensions, each gated:

- **`ARB_vertex_type_10f_11f_11f_rev`** (3/3). Three unsigned floats in one
  dword, fetched as a raw `uint32` and unpacked in a per-layout vertex variant
  -- the machinery the 10/10/10/2 family already uses. **Not**
  `nir_format_unpack_11f11f10f**, which reaches for `unpack_half_2x16_split_x`:
  ntv emits that as a 16-bit float type and does not declare the Float16
  capability, so the module is rejected (`OpTypeFloat 16` against
  `[Int16, Matrix, Int8]`), and declaring it would need WebGPU's optional
  shader-f16 feature for what is only a bit-field extraction. The unpack is
  written out in integer arithmetic plus `ldexp`, which is exact.
- **`ARB_texture_mirror_clamp_to_edge`** (6/6). One instruction: the mode *is*
  `abs()` followed by clamp-to-edge, applied per axis -- a whole-vector `abs`
  would break a texture that wraps one axis and mirrors another.
- **`ARB_query_buffer_object`** (4/4). A readback plus a buffer write, which is
  the extension without its performance point, since this driver resolves
  queries on the CPU. `PIPE_BIND_QUERY_BUFFER` also had to be *admitted* --
  `resource_create` refused it, turning the first use into `GL_OUT_OF_MEMORY`.
- **`ARB_enhanced_layouts`** (4/4, **first try**). Already supported in full:
  the GLSL linker packs small varyings into a shared location with or without
  the extension -- this driver's transform-feedback capture reads
  `location_frac` for exactly that reason -- an explicit member offset is a
  byte offset the UBO lowering already addresses, and the shader's
  `xfb_buffer`/`xfb_offset` qualifiers produce the same `nir_xfb_info` the
  capture pass already walks. What the cap adds is permission for the
  *application* to choose the packing.
- **`ARB_buffer_storage`** (5/5, first try). WebGPU has no persistent mapping
  at all -- a mapped `GPUBuffer` is unusable by the GPU until unmapped -- so
  the mapping is the buffer's CPU shadow and **every submit uploads it**.
  "Coherent" is therefore a promise the driver keeps rather than one the
  hardware does, and it is kept at submit granularity: writes made before a
  draw are visible to it, which is all GL requires without a barrier. The
  probe's decisive case is the second draw -- different vertices written
  through the *same still-mapped* pointer, no flush, no unmap -- which is what
  separates this from uploading once at map time.

### What the 4.4 context revealed

Nine probes in the tree had been asking for a GL 4.4 context and producing
nothing all session. Six now pass outright: `msaa-blit` 8/8,
`msaa-depth-blit` 1/1, `multi-draw` 4/4, `point-parameter` 10/10,
`sample-shading` 4/4, `transform-feedback` 1/1, and `fragment-state-matrix`
23/23. The other two failed, and both were **pre-existing defects that had
been invisible**:

**`vertex-fetch-matrix` 7/16 → 16/16.** Two clusters. Five cases used
attributes at odd offsets or strides, which WebGPU forbids and
`create_vertex_elements_state` refuses -- by refusing the whole state, which
leaves nothing bound and latches the next draw. The cap that fixes it is one
line: `vertex_input_alignment` was left at `NONE`, which told u_vbuf unaligned
attributes were fine, so it passed them through to be refused. `4BYTE` makes
u_vbuf translate them, which is what it is for.

The other four used `glVertexAttribDivisor` with a divisor above one, which the
driver refused outright. WebGPU's `stepMode: "instance"` advances once per
instance and takes no divisor -- so the *layout* for a divisor of two is
already right and it is the data that is wrong: record `i` has to be the
source's record `i / d`. The buffer is de-interleaved on the CPU for the length
of the draw, at the outermost `draw_vbo` only (the restart and multi-draw
splits re-enter, and expanding an expanded buffer would divide twice). A divisor
exists precisely because the per-instance data is *shared*, so there is little
of it to copy.

**`sampling-matrix` 11/15 → 15/15.** All four failures were
`GL_CLAMP_TO_BORDER`, and the cause was one over-strict guard.
`create_sampler_state` demanded `min_lod == max_lod == 0` of a border sampler
on top of `MIPFILTER_NONE` -- but with mip filtering off only the base level is
ever sampled and the lod *range* is irrelevant, while st/mesa gives a texture
with `GL_TEXTURE_MAX_LEVEL = 1` a non-zero range whatever the filter. So an
ordinary NEAREST clamp-to-border sample of a two-level texture had its sampler
**refused**, and a refused sampler is served by the default one, which clamps
to edge. The picture was quietly wrong with nothing in the log.

That silence is now fixed too: every refusal in `create_sampler_state` names
the field that caused it. A refused sampler is never an error the application
sees -- the draw proceeds with the wrong wrap mode and filter -- which makes it
the worst failure shape available.

**The suite is now 61 probes passing with two known divergences**: `quad`,
where llvmpipe is the outlier, and `texture-view` 5/6, which is the WebGPU
format-reinterpretation ceiling.

## Minecraft 26.2 renders

GL 4.4 got Minecraft past its version check. Getting it to draw took seven
more fixes, and the last one is the reason this section is long: the game ran
at 30 fps with 148 draws a frame into a window that screenshotted pure black,
with not one refusal in the log.

### It never reached the driver

`scratchpad/mc262.sh` produced a window and zero `gex-wgpu` lines. The game's
log said:

```
Failed to create backend OpenGL
com.mojang.blaze3d.systems.BackendCreationException: Driver does not support OpenGL 3.3
```

That sentence is not what happened, and believing it costs an hour. Launching
`net.minecraft.client.main.Main` directly, without PrismLauncher's environment
scrubbing, gave the real message:

```
glx: failed to create drisw screen
```

`tests/integration/programs/gl44-glx-context-probe.c` was written to test the
obvious suspect and cleared it: 3.3 core, 3.3 core forward-compatible and 4.4
core all create through `glXCreateContextAttribsARB`, 5/5 on this driver and
5/5 on the system's. `LD_DEBUG=libs` showed why. GLFW `dlopen`s GL at run time,
and **GLFW 3.5 asks the loader for `libGLX.so.0` before `libGL.so.1`**. On any
modern distribution that name is libglvnd's dispatch, which loads its *vendor*
library from `/usr/lib` and with it the distribution's Mesa:

```
calling init: /tmp/gex-mesa-native/inst/lib/libGL.so.1        <- ours, loaded
calling init: /usr/lib/libgallium-26.1.6-arch1.1.so           <- theirs, also loaded
calling init: /usr/lib/libGLX_mesa.so.0
calling init: /usr/lib/libGLX.so.0
```

Two Mesa builds in one process. Our libGL's call to `driCreateNewScreen3` binds
to the older one, which has no `gex_wgpu` driver, so `GALLIUM_DRIVER=gex_wgpu`
selects nothing and screen creation fails. `LD_PRELOAD` of our libGL does not
help: it loads ours *as well*, and the collision is what matters.

The fix is one line per name, now in `web/build-mesa-native.sh`:

```sh
ln -sfn libGL.so.1 "$OUT/inst/lib/libGLX.so.0"
ln -sfn libGL.so.1 "$OUT/inst/lib/libOpenGL.so.0"
```

`dlopen` of a bare soname searches `LD_LIBRARY_PATH` first, so a file of that
name next to ours is found before `/usr/lib`, and there is exactly one Mesa in
the process. No sandbox, no glvnd build, and it fixes every `dlopen`-based
toolkit at once -- SDL and Qt ask the same way.

This matters beyond Minecraft: **a native run that uses `-lGL` proves nothing
about a `dlopen`-based application**, which is most of them. Add
`scratchpad/dlprobe.c`-style resolution to the harness before concluding an app
is broken.

### Five refusals of things the driver already did

Each of these was a guard stricter than the code behind it, and each latched
the context -- which reports as `GL_OUT_OF_MEMORY` from the next unrelated
call.

1. **A buffer with no bind flags at all.** `st/mesa` derives binds from the GL
   target named at allocation, and `glBufferStorage` on an object with no usage
   yet -- the normal way `ARB_buffer_storage` is used -- yields none.
   `resource_create` required at least one. Nothing downstream cared:
   `desc.usage` is already the union of every usage a GL buffer might later
   need, precisely because the creating target does not predict it. Minecraft's
   uniform ring was refused, which `glBufferStorage` reports as
   `IllegalStateException: Failed to map buffer`.

2. **`set_constant_buffer` demanding `PIPE_BIND_CONSTANT_BUFFER`** on those
   same buffers. The bind flag was never the right question; the WebGPU usage
   is. It now asks whether the buffer's usage includes `Uniform`.

3. **No buffer-to-buffer copy.** `resource_copy_region` needs a `WGPUTexture`
   on both sides, and a buffer has none, so `glCopyBufferSubData` was refused.
   WebGPU has `copyBufferToBuffer`; it only wants both offsets and the size to
   be multiples of four, and unaligned copies go through the shadow.

4. **The layer stride on a single-layer upload.** st passed 2048 for a 32-row,
   128-byte-stride 2D image -- half of the 4096 the rows occupy -- and
   `texture_subdata` refused the inconsistency. At depth 1 nothing reads past
   layer zero. The refusal surfaced as
   `GL_INVALID_OPERATION in glTexSubImage2D(PBO is mapped)`, from an internal
   map of a PBO that had nothing wrong with it: a latched context returns NULL
   from every map.

5. **`seamless_cube_map` missing from the sampler whitelist.** `st_atom_sampler`
   sets it for every sampler in a core profile, so *every* sampler was refused
   -- and a refused sampler is served by the default one, silently, so the
   picture is quietly wrong with nothing in the log. WebGPU filters across cube
   faces unconditionally, so the set bit is exactly what this driver does.
   `reduction_mode` stays a refusal (it is `EXT_texture_filter_minmax`) but is
   now a named one; both fields are printed.

And a sixth of the same kind: **`index_bias` was refused on indexed draws**
even though `wgpuRenderPassEncoderDrawIndexed` takes a signed `baseVertex` and
the call already passed it. `glDrawElementsBaseVertex` is how Minecraft draws
terrain.

### The black window

With those, the game ran. `GEX_WGPU_TIMING=1` said 29.9 frames/s, 148
draws/frame, and the window was `mean=0 sd=0`: one colour, black.

This is the trap in `docs`' oldest note -- "the guest drew" and "the canvas
shows it" are different claims -- and neither existing instrument could tell
them apart, so three were added or repaired. `GEX_WGPU_PRESENT_LOG=1` prints:

- the mean and non-zero fraction of the pixels that actually reached the front
  buffer (validated against `glxgears`: `mean=33.6 nonzero=22.3%`),
- the histogram of colour targets the frame's draws went to, via
  `gex_wgpu_report_draw_targets`, which nothing had ever called, and
- the ordered list of clears, blits and draws that touched the window texture.

`gex_wgpu_presented_texture` is now assigned in `flush_frontbuffer`; it never
was, so the "presented buffer: N" counter that has always been printed read 0
whatever happened -- indistinguishable from a frame whose draws all went
elsewhere, the exact question being asked.

They said: `854x480/b8g8r8a8_unorm: 7080` draws with `presented buffer: 7080`
and `present 7080` -- **one draw per frame, on exactly the right texture**, and
the readback all zeros. Then the sequence:

```
gex-wgpu: on the window this frame: blit(854x-480->854x480) draw
```

`src.box.height` is **-480**. A negative height is Gallium's vertical flip and
it means the box runs *upward* from `y`: the rows it covers are
`[y - |height|, y)`, not `[y, y + height)`. `u_blitter` normalises it as
`box.y += box.height; box.height *= -1`, and `gex_wgpu_copy_texture_region`
does the same by hand. `gex_wgpu_blit_by_transfer` took the absolute value and
left `y` at 480, so the copy asked for rows 480..959 of a 480-row texture.

That is a WebGPU validation error, which costs the copy and **raises nothing in
the GL API**. The blit then fell back to `u_blitter` -- the log's only hint,
one line: `blit sent to u_blitter: 854x4294966816->854x480` -- and u_blitter
got the same blit wrong for that destination, also silently.

Minecraft's frame ends with this blit because its render target is drawn
upside down relative to the window, which is true of any renderer that draws
into its own framebuffer. Its own F2 screenshot was the decisive test: it reads
the *main render target* rather than the window, and it came out perfect while
the window was black, which localised the defect to the last operation of the
frame.

### What the probe can and cannot see

`gl44-flip-blit-probe.c` covers seven flipped-blit shapes: whole surface,
sub-rectangle, and each with a format conversion to force the transfer path.
All seven pass -- **and all seven still pass with the fix reverted**, which was
checked. A declined blit falls back to u_blitter, and for an ordinary
FBO-to-FBO pair u_blitter gets the flip right, so GL never sees the difference.
Minecraft's frame only broke because u_blitter *also* failed for that one
destination.

So the probe is a regression guard over all three flip implementations, not a
test that any particular one ran. Two lessons worth keeping:

- **A silent fallback hides a broken fast path.** `blit sent to u_blitter` is
  printed once per process and is easy to miss; it should be read as "something
  the driver claims to do was declined", not as informational.
- **u_blitter is not a safety net.** In the browser it aborts the module
  outright (see the decline comments in `gex_wgpu_blit`), and natively it can
  return wrong pixels. A path that falls back to it is unverified, not covered.

### Also fixed: texelFetchOffset

`nir_lower_tex_options.lower_txf_offset` is now set. SPIR-V permits
`ConstOffset` on `OpImageFetch`; Tint does not:

```
error: ConstOffset is only permitted for sampling, gather, or depth-reference
gather operations: %41 = OpImageFetch %38 %40 %18 Lod|ConstOffset %16 %20
```

A rejected module means the program does not *link*, so
`gl44-integer-texture-probe` died at `glLinkProgram` rather than returning a
wrong texel. Adding the offset to the coordinate is what the offset means for a
fetch, which never wraps or filters.

### Probes that were failing invisibly

The native harness reported anything without a `SUMMARY` line as `NOSUMMARY`
and moved on, which hid nine probes that use an older `<name> ok` convention.
Three of those were genuinely broken:

- `integer-texture` -- the Tint rejection above.
- `texture-sampler` and `reflection` -- linked `glGetInternalformativ`,
  `glGetProgramResourceLocationIndex` and `glShaderStorageBlockBinding`
  directly. **None of the three is in Mesa's non-glvnd libGL**, whose export
  list is 1369 frozen names in `src/glx/libgl-symbols.txt`, and none is
  anywhere in the library. They resolved against the system's glvnd libGL at
  build time and died at load against the driver's, after printing part of
  their output -- which reads as a mid-run failure. Resolved through
  `eglGetProcAddress` now, as every real toolkit does. `nprobe2.sh` names any
  such symbol before running the probe, so this class cannot hide again.

`compute` and `indexed` fail on llvmpipe too and are probe or reference
problems, not driver ones; `compute` additionally stops earlier here because
`max_shader_images` is deliberately zero outside the fragment stage.

### Open, and honest about it

- **Compute shader images.** `max_shader_images` is fragment-only by design
  (`launch_grid` binds storage buffers and nothing else). Implementing them is
  the remaining piece of `ARB_shader_image_load_store` in compute.
- ~~**Sample counts are understated.**~~ Checked and withdrawn.
  `st_QuerySamplesForFormat` counts the sample counts *above one* that the
  driver accepts and only falls back to `{1}` when there are none, so our
  answer of one entry is the list `{4}` -- correct, because WebGPU core offers
  sample counts 1 and 4 and nothing else. llvmpipe's two entries are `{4, 2}`;
  2x is the difference and it is a platform limit, not an under-report.
- **The u_blitter fallback.** It silently produced black for the window blit.
  Declining, the way `gex_wgpu_blit` declines everything else it cannot do, is
  probably better than a fallback that can be wrong.
- **In-game rendering.** Verified further than the title screen: the world
  `--quickPlaySingleplayer` loads, its chunks build, and the game runs at 30
  fps with **556 draws a frame**. The same frame rendered on llvmpipe differs
  by **RMSE 0.0024** -- text, buttons, hearts, hotbar and an entity all match
  the software reference.

  One thing to be careful of, having been caught by it: that save spawns the
  player falling out of the world, so a frame captured a few minutes in is
  terrain seen from a great height. It looks flat-coloured because it is
  legitimately mip-minified, not because the atlas is being sampled wrong --
  the llvmpipe reference at the same save shows the same death screen. Driving
  the game further needs a real focus grab; `xdotool click` on bare Xvfb with
  no window manager does not reach Minecraft's button.

## The browser was three GL versions behind, and two guest gaps under it

Everything above was measured natively. Taking the same probes to a real
browser found one embarrassment and two genuine gaps.

### The bundle

`web/dist` is a built vite bundle, and it is gitignored. It had not been
rebuilt since the device limits were added to `mesa-gl.js`'s `want`, so the
browser asked for none of them and ran on WebGPU's guaranteed minimums:
`maxTextureDimension2D` 8192, `maxTextureArrayLayers` 256,
`maxComputeInvocationsPerWorkgroup` 256. It therefore reported **GL 4.0 while
the native path on the same machine reported 4.4**, and `ARB_compute_shader`
was missing there for the same reason. `npm run build` in `web/` alone moved
it to 4.4 with `tex2d 16384 layers 2048` and 1024 compute invocations.

Two instruments so this cannot recur: the driver now prints its texture limits
with the rest (they decide the reported GL *version* and were the only ones not
printed), and `mesa-gl.js` lists every wanted limit the adapter will not grant
at `console.warn` -- the harness echoes warnings and drops `console.log`. On
this machine the list is one entry, `maxUniformBuffersPerShaderStage 12<16`,
which the driver already backs with storage buffers.

**Rebuild `web/dist` before drawing any conclusion from a browser run.** The
wasm driver build does not do it.

### ARB_viewport_array was withheld from the guest

6/6 natively, 0/6 in the browser, with `viewport_array=0`: the guest never
advertised it. It exports all ten of the extension's commands. What it lacked
were the registry's eleventh and twelfth, `glDepthRangeArraydvNV` and
`glDepthRangeIndexeddNV`, which sit in a second `<require>` block the registry
labels *"ARB_viewport_array interactions with NV_depth_buffer_float"*.

An interaction block describes the pair, not the extension: those commands
belong to the other one and exist only when it is present.
`tools/gen_gl_extensions.py` now skips a `<require>` whose comment says so.
One extension unblocked; worth re-checking the other 278 withheld entries for
the same shape.

Then five of six passed, and the roundtrip read a scissor box of `2,0,0,0`.
`glGetIntegeri_v` is not always a scalar -- `GL_VIEWPORT` and
`GL_SCISSOR_BOX` are four values per index, which is what the extension is
*for*. The guest read one word, and the host passed the address of a single
`GLint` to a call that writes four: an out-of-bounds write on its own stack,
for exactly the two pnames the extension exists for. Both sides now size the
reply from the pname, the guest through the same `gexgl_state_query_components`
table `glGetIntegerv` uses, computed before the request goes out so a
disagreement cannot write past the caller's array. 6/6.

### Persistent coherent mappings never reached the host

`gl44-buffer-storage-probe`: 5/5 natively, 3/5 in the browser, and the two
failures are the whole point of `ARB_buffer_storage` -- vertices written
through a `PERSISTENT|COHERENT` mapping, then written again and drawn again.
Both drew nothing. **This was the browser path's blocker for Minecraft**,
whose uniform and vertex rings are allocated exactly this way.

A mapping the guest returns is guest memory, and the bytes used to reach the
host only at `glUnmapBuffer` or `glFlushMappedBufferRange`. Persistent means
the first never happens; coherent means the second is not required. So nothing
was ever sent.

There is no write barrier to be had -- wasm has no `mprotect` and no signals,
which is what every real implementation of this uses -- so the mapping is
compared against what was last sent and only the changed span is transported.
It has to be **per draw, not per batch**: an application writes region N,
draws, writes region N+1 and draws again, and both draws can sit in one
unflushed batch, so sending once at flush time gives the first draw the second
region's bytes. `GEXGL_NO_COHERENT=1` disables the comparison for anyone
measuring it; the cost is one pass over the mapped range per draw.

The first attempt was refused by the host with `GL_INVALID_OPERATION`:
`glBufferSubData` on an immutable buffer requires `GL_DYNAMIC_STORAGE_BIT`, and
an application asking for `PERSISTENT|COHERENT` has no reason to ask for it --
it means to write through the mapping, not through the API. The host is now
given that bit and only the host is; the flags the application passed are
recorded guest-side and `GL_BUFFER_STORAGE_FLAGS` is answered from them.

### Browser state after all of it

| probe | native | browser |
| --- | --- | --- |
| flip-blit | 7/7 | 7/7 |
| buffer-storage | 5/5 | 5/5 |
| viewport-array | 6/6 | 6/6 |
| tessellation | 12/12 | 12/12 |
| ssbo-atomic | 7/7 | 7/7 |
| stencil-texture | 4/4 | 4/4 |
| draw-parameters | 5/5 | 5/5 |
| integer-texture | ok | ok |
| clear | ok | ok |
| query-buffer | 4/4 | **0/4** |

`query-buffer` is the one gap left, and it is the same shape as the mapping
one: the guest forwards the `GL_QUERY_BUFFER` binding but then passes a client
pointer where the bound buffer makes that argument an offset, so the host
answers `GL_INVALID_OPERATION`. It needs the guest to notice the binding and
send an offset instead. `ARB_query_buffer_object` is mostly used by profilers
rather than by the applications being targeted, which is why it is last.

## Krita renders (and one X depth was the whole story)

Krita 6.0.3 is one of the named target applications and had not been checked
at GL 4.4. It renders a black window on this driver. The useful part of this
section is how much of that turned out *not* to be the driver, and the two
real bugs found on the way.

### First, a harness mistake worth naming

`scratchpad/appshot.sh` takes the driver environment from its caller. Run
without it, Krita drew perfectly -- on the *system* Mesa. Any app-sweep result
is only about the driver whose environment was actually set, and the sweep's
logs are worth checking for `gex-wgpu: native screen on Dawn` before being
believed. (Of the 336 in the last sweep, 308 have it; the 28 without are
Wayland compositors, KMS programs and X servers, which cannot run here at all.)

### Two real driver bugs, neither of them the cause

**One WebGPU device per process, not per screen.** Mesa builds a screen per X
display connection, and a toolkit rendering on several threads opens several:
Krita reaches five. `create_device` ran per screen, so each got its own device.
That costs seconds of adapter and device creation each, and makes resource
sharing across them impossible -- Qt shares GL contexts, `QOpenGLWidget`
requires it, and a texture on one WebGPU device does not exist on another.
Nothing reports that; the sharing just yields nothing. Now created once.

**One drawable per screen, not one global.** The per-screen present state was a
single global that the last screen created overwrote, so every other screen
presented through that one's winsys and displaytarget, at that one's size. Now
looked up by `pipe_screen`, which `flush_frontbuffer` is already handed.

### Then three defects in the instruments

Each of these sent the investigation somewhere wrong, which is the reason they
are worth recording:

- The present log reported **every thirtieth** present. Krita presents six, so
  one line appeared, and "one present for six swaps" read as five dropped
  frames. There were none. The first eight are now reported too.
- The PPM dump wrote a texel's first three bytes as RGB. A window here is
  BGRA, so every dumped frame had red and blue swapped -- which reads as a
  driver colour bug in an otherwise correct picture.
- Every early return in `flush_frontbuffer` was silent. Each now names itself.

### What is actually true

With the instruments fixed, the driver comes out clean:

- `qtdiag6` reports `4.4 (Compatibility Profile)` through Qt on this driver,
  with the right device string.
- A Qt Quick scene (`qml6`) renders identically to softpipe -- 142 colours
  against 141.
- Krita makes **the same 39 GLX calls in the same order with the same return
  values** on this driver and on softpipe, six `glXSwapBuffers` among them,
  each preceded by a successful `glXMakeCurrent`. (`scratchpad/glxspy.c`
  interposes them.)
- Our `flush_frontbuffer` is called for all six, drops none, and the readback
  of each is **Krita's complete interface** -- menus, panels, thumbnails,
  text -- at 960x998, `mean=126`, pixel-comparable to the same run on
  softpipe. The dumped frames are in the handoff's own record.
- Every thread is idle in Qt's event loop afterwards. Krita is not stuck.
- `glXQueryDrawable` reports the resized size correctly (checked against
  softpipe with `scratchpad/qdrawable.c`), drawable resize works (glxgears
  resized to 700x500 presents 700x500), and no resource or format was refused.

So the driver draws the right pixels and hands them to the winsys, and they do
not reach the X window. The hand-off code was read end to end --
`drisw_swap_buffers` → `drisw_copy_to_front` → `drisw_present_texture` →
`flush_frontbuffer` → `dri_sw_displaytarget_display` → `swrastXPutImage`, with
its shm and non-shm paths -- and nothing in it is conditional on anything this
driver does differently. glxgears goes through the identical path and works.

### The answer: an image depth the window could not accept

The interposer (`scratchpad/xspy.c`, on `XPutImage`/`XShmPutImage`/
`XShmAttach`) took one run to settle it:

```
glxgears:  XShmPutImage(drawable=0x200003 304x300 depth=24 bpp=32) = 1   -> visible
Krita:     XShmPutImage(drawable=0x200013 960x998 depth=32 bpp=32) = 1   -> nothing
```

Krita's window is **depth 24**. Putting a depth-32 image into it is a
`BadMatch`, and X reports that asynchronously -- so `XShmPutImage` returns
success, the driver sees nothing wrong, and the frame is gone. Six correct
frames, one black window, no error anywhere.

The depth comes from the *fbconfig's visual*: `drisw_create_drawable` sets
`pdp->xDepth` from `config->visualID`'s depth, and the XImage is built with
it. So the question became why Qt chose a config with a depth-32 visual for a
depth-24 window, and the answer was in this driver's own refusal list, which
had been scrolling past all along as routine format probing:

```
render target refused b8g8r8x8_unorm ... (unmapped format not renderable ...)
render target refused r8g8b8x8_unorm ... (unmapped format not renderable ...)
```

**B8G8R8X8 and R8G8B8X8 were unmapped.** Mesa builds its GLX fbconfig list
from the formats the driver accepts, so there was no 8,8,8,0 config: all 144
had an alpha channel (measured -- zero configs with `GLX_ALPHA_SIZE` 0), and
24 of them carried a depth-32 visual. A toolkit asking for an opaque window
had nothing opaque to choose, and whether it drew depended on which visual it
happened to land on.

The fix is four lines of format mapping. WebGPU has no ignored-alpha format,
so they map to the same `BGRA8Unorm`/`RGBA8Unorm`, and GL's promise that an
X format's alpha reads as one is kept by the view swizzle -- next to the
`DXT1_RGB` case that is there for exactly the same reason. 96 opaque configs
appeared, and Krita renders: its welcome screen, and its full editor with the
GL canvas, colour wheel, layers and brush presets, pixel-comparable to
softpipe.

This was never Krita-specific. Any toolkit that asks for an opaque config was
affected, which is most of them.

`gl44-window-present-probe.c` now covers the step that had no probe at all:
every other probe reads back with `glReadPixels`, which never leaves the
driver. It creates a window from a config's own visual, clears it, swaps, and
reads the window back *through X* -- opaque, alpha with a depth-24 visual, and
alpha with a depth-32 ARGB visual. 3/3 here and on softpipe, and it would have
caught this: before the fix the opaque case could not find a config.

## Two probes that were lying, and a regression the browser gate caught

### gl44-indexed-probe stopped at its third case on every native driver

It asserted that a shader name and a buffer name differ. Only the *guest*
allocates object names from one global pool; a native driver may hand out
shader 1 and buffer 1, and llvmpipe and this one both do. The condition was
`!getenv("GEX_NATIVE_REFERENCE")` -- a variable the reference run sets and the
*native driver* run does not, though both are native -- so a guest-only
invariant was asserted against a native driver and the probe exited three
cases in. It now opts in from the guest, which `tests/gl44-probe.sh` always
sets.

And `check_green` called `exit(3)`. Combined with the above, six of eight
cases had never run on this driver and three had never run on either. With
both fixed the driver passes **8 of 8**, including the instanced
base-vertex/base-instance draws that had never been exercised.

### llvmpipe drops a whole multi-draw when one sub-draw has count zero

The probe's two multi-draw cases had `counts = {0, 3}`. A zero count is legal
GL and this driver handles it -- the draw path returns early for an empty
sub-draw rather than refusing, which `glMultiDrawArraysIndirect` needed --
but **llvmpipe draws nothing at all for the entire call**. The equivalent
single draw at the same offset passes there, so it is the zero entry that
does it.

That made the probe un-gateable in the browser: `tests/gl44-probe.sh` takes
its expectation from a native Mesa run and requires it to pass. Both entries
now draw, which tests the multi-draw transport rather than the geometry, and
this is the third place the reference is the outlier -- after `quad`'s
GL_QUADS triangulation and `texture-sampler`'s extra 2x sample count.

### An indexed query's arity is per index

Making `glGetIntegeri_v` pname-sized last pass took the arity from
`gexgl_state_query_components`, which answers the *plain* query:
`glGetIntegerv(GL_MAX_COMPUTE_WORK_GROUP_COUNT)` is three values, while
`glGetIntegeri_v` of the same pname is one value per index. The guest expected
three words where the host sent one, discarded the reply as the wrong length,
and read back zero -- below the minimum GL requires. Only `GL_VIEWPORT` and
`GL_SCISSOR_BOX` are vectors per index, which is the rule the host side
already applied.

**The browser gate is what caught it.** The native runs were unaffected and
reported 8 of 8. That is the case the gate exists for, and it is worth running
it on a probe whose transport was touched even when the native numbers look
clean.

## The app sweep, re-run: 60 refusals down to 2

`scratchpad/gl-app-sweep.sh` runs every installed GL program for eight
seconds and records the driver's own complaints. It had not been re-run since
a long list of fixes landed, and its notes were describing a driver that no
longer existed -- its biggest cluster, 45 programs on one rasterizer-state
field, had been fixed in between.

Re-run over the same 336 programs:

| verdict | before | after |
| --- | --- | --- |
| RAN | 192 | **281** |
| REFUSED | 60 | **6** → 2 |
| CRASH | 32 | **11** |
| WGPU-ERROR | 14 | **0** |
| EXIT | 22 | 22 |
| OK | 16 | 16 |

Grouping the notes by cause is what makes this a work list rather than a
score. The whole remaining refusal set was two causes:

- **Four programs on GL 1.x `GL_CLAMP`** (`convolutions`, `projtex`,
  `shadowtex`, `texenv`). It clamps to the *border* for NEAREST and blends
  toward the border colour for LINEAR; WebGPU has no such mode and neither
  does any modern GPU -- it was removed in the core profile and every current
  driver serves it as clamp-to-edge, which is what these programs already see
  everywhere else. Refusing it was the worse trade, because a refused sampler
  is silently served by the default one and the filters, LOD range and
  **comparison mode** went with it. Now accepted: all four run with no
  refusal, and `shadowtex` draws its shadow.
- **Two programs on logic ops** (`line-xor`, `tri-logicop-xor`). WebGPU has
  no logic ops at all, and these demos exist to draw with `GL_XOR`.
  Accepting and ignoring would draw solid instead of XOR -- wrong either way,
  so this one stays a named refusal. It is a platform ceiling.

The 11 crashes are worth a note: seven are `signal 127` (the program is not
executable here at all) or `signal 6` from Hyprland, and the two real ones
are `texture_from_pixmap` (`signal 11`) and `glxpixmap` -- GLX pixmap
rendering, which this driver has never claimed.

Two cautions for the next re-run, both learned the hard way:

- Check each log for `gex-wgpu: native screen on Dawn` before believing a
  verdict. 308 of 336 have it; the 28 without are Wayland compositors, KMS
  programs and X servers that cannot run here at all.
- It uses `DISPLAY=:77`, so an app screenshot taken while it runs picks up
  *its* windows. One Blender check this pass photographed a sweep program's
  window and had to be redone.

### Blender, checked for a regression from the new configs

Adding opaque configs changes which fbconfig a toolkit picks, so Blender was
re-checked: it shows no window within 55 s and dies on the documented ceiling,
`colour target 1 refused (surface reinterprets the format) surface=r16_uint
texture=r16_float`, followed by a `GL_OUT_OF_MEMORY` cascade from the latched
context and a core dump.

That is **not** a regression from this pass -- verified by removing the four
format mappings, rebuilding, and running it again: identical, no window. The
format-reinterpretation ceiling is where Blender stands either way. Worth
noting that it now reaches that ceiling and *crashes* rather than sitting at a
drawn interface, so "Blender's UI draws" from an earlier pass should be read as
"it drew before it got to the viewport", not as a state that survives startup.

## Two fatal compute bugs, found by taking a latch off

Blender's viewport is a platform ceiling, but its *crash* was not, so the
latch on the framebuffer refusal was removed to see what came next. Three
things did, and two of them are fatal bugs that had nothing to do with
Blender.

**A read-only compute SSBO gave the pipeline a layout WebGPU rejects.**
`launch_grid` declared every storage block as read-write `Storage`. A block
the shader never writes is emitted as `var<storage, read>`, and the layout has
to agree:

```
The buffer type in the shader (BufferBindingType::ReadOnlyStorage) is not
compatible with the type in the layout (BufferBindingType::Storage)
```

An invalid compute pipeline makes the whole command buffer invalid, so the
submit fails and everything recorded with it is lost. The graphics path has
carried this bit per stage as `ssbo_readonly_mask` for a long time; compute
had no equivalent.

**A storage block the shader declares with nothing bound failed the context**,
in all three bind paths. GL leaves the *contents* undefined rather than the
call fatal, and this driver advertises `robust_buffer_access_behavior` under
which the read is zero -- so zeros are what has already been promised. A
grown, zero-filled placeholder is bound instead. A *uniform* block with
nothing bound still fails: its size is part of the layout and there is no
defensible answer.

**And a refused sampler view was dereferenced**, segfaulting the process
rather than declining the shader variant. Unreachable while the framebuffer
refusal latched first -- which is exactly what removing a latch exposes.

Neither compute bug had any coverage: `gl44-compute-probe` stops at the
deliberate fragment-only shader-image cap, so every SSBO case after it had
never run here. `gl44-compute-readonly-probe.c` now covers all three,
including both spellings of read-only (the `readonly` qualifier, and inferred
because the shader never writes the block -- only one is visible in the
GLSL). 3/3 native, 3/3 in the browser, and it catches the defect: with the
binding type reverted it fails 2 of 3, verified by reverting and rebuilding.

### The framebuffer refusal stays a latch, and this was tried the other way

Dropping the unexpressible attachment -- the treatment the bytes-per-sample
budget already gets -- lets the draw proceed against a framebuffer the
application believes it has. Blender reinterprets **three** colour targets and
a sampler view (`r16_uint` over `r16_float`, `r16g16_float` over `r32_float`,
`r16g16b16a16_float` over `r32g32_float`), so dropping them left nothing to
draw into and the failure moved from a named refusal to a segfault: first
inside this driver, then inside Blender itself once that was guarded.

A latch is a bad outcome. A crash with no diagnostic is worse, and Blender
cannot render that pass either way. The comment in the code records the trade
so the next pass does not retry it blind. Worth revisiting only for an
application that reinterprets *one* of several attachments and would keep
working without it.

### A third stale artefact

The browser run of the new probe failed 2 of 3 -- because the staged wasm host
module predated the fix. Its `GL_VERSION` carries the git sha, which is how it
was noticed, after the run. `tests/gl44-probe.sh` now warns when the driver
source is newer than the staged module. That makes three investigations
started by a stale build artefact: the vite bundle, the generated extension
table, and this.

## Compute shader images, indirect dispatch, and a probe that had never run

`max_shader_images` was fragment-only, so `ARB_shader_image_load_store`
stopped at the fragment stage and `gl44-compute-probe` died at link --
"Too many compute shader image uniforms (1 > 0)" -- at its third case. Behind
that were three driver bugs and three probe bugs, and the probe had never run
past case three on *any* driver.

**Driver.** `launch_grid` now collects images from the NIR the way the
graphics stages do, with the same two refusals, and builds a view and a layout
entry per image per dispatch (`glBindImageTexture` names a level and a layer
the next dispatch may change). A read-only image gets a *sampled* texture
entry rather than a storage one, because Tint lowers a read-only storage image
to a plain `texture_2d<T>`.

Then:

- The images kept **descriptor set 2** while `launch_grid` binds one group at
  index 0, so Dawn refused the pipeline -- "the entry-point uses bindings in
  group 2 but [PipelineLayout] doesn't have a BindGroupLayout for this index"
  -- and an invalid compute pipeline invalidates the command buffer, so the
  dispatch wrote nothing and the image read back as zeros. The set-to-zero
  loop covered UBOs and SSBOs and not images.
- **`glDispatchComputeIndirect` was refused outright.** The counts are now
  read on the CPU and re-issued as an ordinary dispatch, which is the choice
  the draw path already makes for `ARB_draw_indirect`; the native
  `dispatchWorkgroupsIndirect` would need `WGPUBufferUsage_Indirect`, which
  `resource_create` does not ask for.
- The **image-unit heuristic preferred `driver_location`** over an explicit
  `layout(binding = N)`. st gives a fragment image `UINT_MAX` there and a
  compute image `0`, whatever the GLSL said, so every compute image went to
  unit 0 and the driver found nothing bound.

**Probe.** It read 8 uints from the wrong buffer -- `glBindBufferBase` sets the
*generic* binding point as well as the indexed one, so
`GL_SHADER_STORAGE_BUFFER` still named the 7-word indirect buffer. And four
textures were mipmap-**incomplete**: one level (or levels 0 and 1 of an 8x2)
with the default `GL_NEAREST_MIPMAP_LINEAR` filter. `glCopyImageSubData`
refuses an incomplete source by name; `st_convert_image_from_unit` silently
*drops* one and hands the driver an empty image view with nothing to say why.
That second failure mode is worth remembering -- an image that is simply not
there, with no GL error and no driver message.

`gl44-compute-probe` now passes end to end natively and in the browser.

### Probe suite: 73, with three divergences and all of them explained

- `quad` -- llvmpipe is the outlier on GL_QUADS triangulation (this driver 6/6).
- `texture-sampler` -- llvmpipe offers a 2x sample count; WebGPU core has 1
  and 4 only, so one entry (`{4}`) is the correct answer here.
- `texture-view` 5/6 -- the format-class ceiling.

`compute` and `indexed` were in that list at the start of this pass and are
not now.

### The version ladder was measuring GLSL against a level the driver passed

`tests/gl-version-ladder.sh` defaulted `GEX_GLSL_LEVEL` to 430, hardcoded,
long after the driver reached 440 -- so the 4.4 rung was reported as needing
a GLSL level it already had. It now reads `caps->glsl_feature_level` from the
driver source and exits rather than assume. The honest state:

- **to 4.4**: seven numeric limits, no extensions.
- **4.5**: `ARB_conditional_render_inverted`, `ARB_cull_distance`,
  `ARB_derivative_control`, `ARB_shader_texture_image_samples`,
  `NV_texture_barrier`, GLSL450.
- **4.6**: seven more plus GLSL460.

Of the 4.5 set, `NV_texture_barrier` and `ARB_derivative_control` look close
to free (the driver already hooks `texture_barrier`, and WGSL has
`dpdxFine`/`dpdxCoarse`), `ARB_shader_texture_image_samples` needs the sample
count in the shader, and `ARB_cull_distance` and
`ARB_conditional_render_inverted` need real implementations -- so the rung
does not move by granting the cheap two, and granting a cap the driver does
not honour would draw when it should not.

## GL 4.5 is one extension away

The ladder said 4.5 wanted five extensions plus GLSL450. Four of them landed
this pass, each with a probe, and the fifth is the one that needs real work.

### ARB_conditional_render_inverted -- and a claimed feature that latched

`caps->conditional_render` was set, so Mesa advertised
**ARB_conditional_render, which is GL 3.0** -- and `render_condition` failed
the context for any query, on a comment that "queries do not exist here".
True when written; not since occlusion queries landed. So an application that
used a feature the driver claimed got `GL_OUT_OF_MEMORY` from every later
call.

It now stores the predicate and tests it before each draw (at the outermost
level, so a draw the rewrite paths split stays one decision) and before a blit
that asks for it. The convention is worth writing down because it is not
obvious and getting it backwards inverts every conditional draw: **st passes
`condition` true for GL's `*_INVERTED` modes, and the test is against the
negation of "result is non-zero"**. `GEX_WGPU_CONDRENDER_LOG=1` prints each
decision.

Two bugs surfaced underneath, both wrong independently of conditional render:

- **An occlusion query with no draws reported the previous query's count.**
  The result is resolved from slot 0 of one `WGPUQuerySet`, which holds
  whatever the last pass wrote -- so a query that counted nothing read back a
  stale total (64 where 0 was right). Queries now record whether a pass ever
  carried them. *Any* application using an occlusion query to decide whether
  something is visible was affected.
- **A draw with the scissor test enabled and no rectangle pushed was
  refused.** st's `update_scissor` pushes only when the rectangle differs from
  its own zero-initialised cache, so an *empty* scissor is never sent at all.
  "Enabled and never pushed" therefore means an **empty box**, not GL's
  default whole-window one -- an application that never calls `glScissor` gets
  the window dimensions, which do differ from the cache and are pushed
  normally. Getting this backwards (treating it as no scissoring) drew the
  whole framebuffer and made the occlusion query above count 64.

### ARB_texture_barrier / NV_texture_barrier

`glTextureBarrier` promises that writes to a texture are visible to later
reads, and `gex_wgpu_texture_barrier` already submitted everything recorded --
which on a driver that orders every command on the WebGPU queue *is* that
guarantee rather than an approximation of it. The cap was simply missing.

The guest could not claim it, though: `gen_gl_extensions` claims an extension
only when the guest exports its commands, and `glTextureBarrier` was not
generated because the transport generator's command universe stopped at GL 4.4
core. The universe now runs through 4.5 -- which widens the universe only,
since emission is still driven by the policy sets.

### ARB_derivative_control and ARB_shader_texture_image_samples

Both were a missing cap over a chain that already worked: zink's emitter has
`SpvOpDPdxFine`/`Coarse` and declares the DerivativeControl capability, and
`textureSamples()` becomes `OpImageQuerySamples`, which Tint takes as
`textureNumSamples`. That is exactly the situation where it is tempting to
turn a cap on and move on, so both got probes:
`gl44-derivative-control-probe` (fine and coarse must agree on a linear ramp,
compared by magnitude because GL does not fix a derivative's sign) and
`gl44-texture-samples-probe` (checked against what the implementation says it
allocated, not against 4).

### What is left: ARB_cull_distance

A primitive is discarded when, for some i, **all** of its vertices have
`gl_CullDistance[i] < 0`. WGSL has clip distances and no cull distances.

Mapping cull onto clip is tempting and wrong: clipping cuts a primitive where
the interpolated value crosses zero, so an all-negative primitive does
disappear (correct) but a mixed-sign one is partly cut when culling would keep
it whole. That is a silent mis-render, which this driver refuses elsewhere on
principle.

The honest implementation is primitive-level culling, and the machinery for it
exists: the driver already runs a compute pass to emulate geometry shaders,
captures vertex outputs, culls by facing on the CPU for per-face polygon mode,
and rewrites index lists in several places. A cull pass would evaluate the
distances per vertex and build a filtered index list. It is a real piece of
work rather than a cap, which is why 4.5 does not land in this pass.

Note the rung does not move by granting the four: `_mesa_compute_version`
wants all of them plus GLSL450. So the version stays 4.4 while four of its
five extensions are now advertised and tested -- applications that detect
them individually can use them today.

## A sampler view across format classes -- the "ceiling" that was not one

This was written up here as a WebGPU ceiling: `viewFormats` spans only the
sRGB pair, so a view whose format is a different *class* from its texture's
cannot exist. That much is true, and the conclusion drawn from it was wrong.

**A copy is format-agnostic.** A texture-to-buffer-to-texture round trip moves
bytes without caring what either side calls them, because a buffer is just
bytes -- the only requirement is that the bytes per texel match. So the
source's texels are copied into a scratch texture declared with the *view's*
format and the shader samples that.

Scope and cost, deliberately bounded:

- One level of one layer of a 2D texture, which is what the cases asking for
  it use. A wider view still refuses rather than serving part of itself.
- Refreshed when the source's `write_serial` moves, not per draw -- resources
  now carry that serial, bumped at the four places a texture is written. A
  view of a texture nothing has written since the last copy costs nothing.
- The copy is still real work when the source *is* written every frame, which
  is the reason for the level/layer bound.

**Where the refresh runs is the whole subtlety.** It has to submit pending
work, because the source's writes must reach the queue before they can be
copied -- and doing that while a pass is being built releases the encoder the
draw is recording on. Dawn then faults inside `ValidateCanEncodeOn` at
`BeginRenderPass`. It was written the obvious way first (at the point the bind
group is built) and segfaulted exactly there. It now runs at the outermost
draw entry, before anything is recorded.

`gl44-texture-view-probe` is **6/6**, the case it used to fail returning the
exact expected texel, natively and in the browser. The suite is 77 with two
divergences, and both are llvmpipe being the outlier: `quad`'s GL_QUADS
triangulation and `texture-sampler`'s extra 2x sample count. SuperTuxKart is
unchanged at 72 frames/s.

Blender's **sampler-view** refusal is gone with this -- 0 refusals where there
was one. What remains for its viewport is the same reinterpretation on a
*render target* (`r16_uint` over `r16_float`), and that is a bigger change
than this one: the scratch has to be threaded through three call sites rather
than one -- the pass attachments, `glClear`'s own per-target path, and
`end_pass` -- and it needs the copy in *both* directions, since a pass whose
`loadOp` is Load must see the real texture's bytes before it renders. Those
copies can ride the pending encoder around the pass, so no submit is needed;
the risk is that the framebuffer path is what makes SuperTuxKart, Minecraft
and Krita work.

### Newly surfaced: an sRGB view reads zero in the browser

Gating this probe in the browser for the first time turned up a separate,
**pre-existing** bug: the `srgb` case (an `GL_SRGB8_ALPHA8` view of an
`GL_RGBA8` texture, which is the one reinterpretation WebGPU *does* allow)
reads 0 in the browser and 55 natively.

What is known, so the next pass does not re-derive it:

- Reading the same texel through the **base** texture in the browser gives
  128, the correct linear value -- so the upload landed and the aliasing that
  `glTextureView` sets up works. Only the differing-format view is wrong.
- There is no `sampler view refused` and no WebGPU error anywhere in the run.
  The view is created and samples zero.
- The `levels` and `layers` cases use `glTextureView` too and pass, so the
  entry point and the alias are fine; the format is the only variable.
- It is **not** a regression from the work above, verified rather than
  reasoned: rebuilding the wasm host from the pre-change driver and gating
  again fails `srgb` identically.

Narrowed further, and this is where it stands:

- The driver now reports what it built for any differing-format view, and the
  two builds print an **identical** line: the same view format over the same
  texture format, `reinterpreting=0` (the sRGB pair needs no copy), mapping to
  the same pair of WGPU formats, over a resource that is written in both. So
  the right view is built and it samples wrong.
- The `srgb=` field in the draw dump is `util_format_is_srgb(sv->format)` and
  drives nothing -- there is no sRGB shader variant to suspect.
- **`format-class` passes in the browser** through the scratch-copy path added
  this pass, while the *direct* WebGPU sRGB view fails there. So the browser
  samples a copied texture correctly; what it does not do is sample an
  `rgba8unorm-srgb` view of an `rgba8unorm` texture.
- The software-adapter comparison is not available: that adapter grants fewer
  limits, so the GL version drops and `ARB_texture_view` is not advertised at
  all (the probe reports its never-sampled sentinel, 65535).

### The repro was written, and it clears the browser

`scratchpad/srgbrepro/repro.html` does the whole thing in raw WebGPU with no
driver involved: an `rgba8unorm` texture created with
`viewFormats: ['rgba8unorm-srgb']`, a known texel written to it, then read
back through both an sRGB view and a linear one, by `textureLoad` *and* by
`textureSampleLevel` with a filtering sampler. Served over `http://localhost`
(a secure context) and reported out through the server's request log, because
`--dump-dom` fires before the async GPU work finishes and
`--virtual-time-budget` does not advance real GPU work.

In the same headless Chromium the harness uses, with the same
`--use-angle=vulkan` flags:

```
load-srgb=55  load-linear=128  sample-srgb=55  sample-linear=128
```

All four exact. **The browser's WebGPU is correct for this**, sampled path
included. So the fallback sketched here previously -- routing sRGB views
through the scratch copy -- would paper over a driver bug, not a platform
limitation, and is off the table.

### Every layer between GL and WebGPU now measures correct

Working inwards, all measured in the failing browser run:

- The driver builds an **identical** sampler view in both builds: same view
  format over the same texture format, `reinterpreting=0`, same WGPU format
  pair, over a resource that reports itself written.
- The **bind group layout** is identical for all six of the probe's draws,
  the sRGB one included: same `sampleType`, same view dimension, same
  `depth_as_float` bit.
- The **JS `createTexture`** call declares `viewFormats:
  ["rgba8unorm-srgb"]`, so the view is legal rather than an invalid object
  that would silently drop the draw.
- The **JS `createView`** call is `format=rgba8unorm-srgb dim=2d baseMip=0
  mipCount=1 baseLayer=0 layerCount=1 aspect=all` on an `rgba8unorm`
  texture -- byte for byte what the passing repro asks for.
- The draw machinery is fine in that build: the `format-class` case, which
  samples a scratch texture through the same fragment path, **passes** in the
  browser.
- The probe sets `GL_NEAREST` filters, so texture completeness is not it, and
  the `srgb=` field in the draw dump drives nothing.

Two more instruments ran before the answer turned up. The *draw* is fine: a
constant fragment body in that case reads 128 in the browser, so it is the
**sample** that returns black. And a *nearest* sampler on an sRGB view is
also correct in the raw repro (`near-samp-srgb=55 near-samp-lin=128`), which
removed the last difference between the repro and the driver -- the probe
sets `GL_NEAREST` and the repro had used a filtering sampler.

### The answer: StorageBinding usage forbids an sRGB view

Every instrument above measured the *request*, and the request was legal. The
rejection was inside Dawn, and it had been in the browser console the whole
time:

```
The texture usage (CopySrc|CopyDst|TextureBinding|StorageBinding|
RenderAttachment) includes TextureUsage::StorageBinding, which is
incompatible with the format (TextureFormat::RGBA8UnormSrgb).
```

**No sRGB format is storage-compatible**, and this driver hands
StorageBinding to every storage-capable single-sampled texture -- deliberately
and with its own comment, because st/mesa never asks for
`PIPE_BIND_SHADER_IMAGE` up front and `glBindImageTexture` arrives long after
the allocation. So on an 8-bit texture the sRGB counterpart that
`resource_create` declares in `viewFormats` could never actually be *used*.
The declaration is accepted; the view built from it is not.

That explains every contradictory measurement at once. The texture is valid,
the view descriptor is right, the bind group layout is right, the shader is
right -- and the `WGPUTextureView` object is invalid, so its bind group is
invalid, so the draw is dropped and the probe reads back the clear colour. No
GL error, no `sampler view refused`, nothing in the driver's own logs.

Native Dawn does not enforce the rule, which is why this was browser-only.
That is the third time the "native Dawn is more permissive" note has cost a
pass; a native-only pass is not a pass.

The fix routes those views through the scratch copy a cross-class view
already uses (`storage_forbids_view` in `create_sampler_view`). The bytes are
identical either way -- an sRGB view differs from a linear one only in how the
sampler decodes what is there -- and the scratch carries
`CopyDst|TextureBinding` alone, so it may legally be sRGB. `texture-view` is
now **6/6 in the browser** as well as natively, the native suite is unchanged
at 77, and `image`, `compute`, `compute-readonly`, `swizzle`, `copy-image`,
`integer-texture` and `clear` all still pass in the browser.

### The lesson, which is worth more than the fix

**Grep the browser log for WebGPU validation errors before building any
instrument.** Five instruments were built across two passes -- a driver view
log, a bind-group-layout log, JS monkey-patches of `createTexture` and
`createView`, a raw-WebGPU repro, a constant-shader probe variant -- and the
root cause was one `grep -i validation` away in a log already on disk. The
searches that were run looked for the driver's own strings, which is exactly
the blind spot: the driver cannot report an error it never learns about.

When reading those errors, note that an invalid object **cascades**:

```
validation: [Invalid TextureView (unlabeled)] is invalid due to a previous error.
validation: [Invalid BindGroup (unlabeled)] is invalid due to a previous error.
validation: [Invalid CommandBuffer] is invalid due to a previous error.
```

Only the *first* message names a cause; the rest are downstream noise. Sort
unique and look for the one line that mentions a format, a usage or a limit.

One related hole is now known but unmeasured: the same rule applies to a
**render-target** view of a differing format, which is how `GL_FRAMEBUFFER_SRGB`
is expressed. Nothing in the probe suite trips it today (a full browser sweep
of the affected probes shows no remaining `incompatible with the format`
line), but a real app that renders through an sRGB view of a storage-capable
texture would hit it, and the driver currently refuses colour-target
reinterpretation outright rather than copying. That is the next thing to
measure in a real app run.

## Blender 5.2 renders (2026-09-03)

The last of the named target applications that had never drawn anything now
matches the system Mesa on the same window: mean 60 with 482 colours to its
484. The menu bar, every tab label, the splash screen's two file lists, the
outliner tree, all eleven properties panels with their values, and a viewport
with grid, axes, camera and navigation gizmo. No refusals and no WebGPU
errors in the run.

Six separate defects stood between "dumps core before opening a window" and
that, and **not one of them reported anything**. They are worth listing in
order, because the order is the lesson:

1. **A refusal killed the process.** The refusal latch is never cleared, and
   `buffer_map` was gated on it. Mesa turns a NULL map into
   `GL_OUT_OF_MEMORY`, applications dereference the result, and the crash
   lands in application code with nothing of this driver's in the backtrace.
   A colour-target refusal Blender would have survived with one wrong panel
   killed it instead. Maps and every upload path now proceed regardless;
   draws, clears and dispatches stay latched, which is what the latch is for.

2. **Only the first refusal was ever printed.** That one hid the fatal one:
   the log named the colour target and said nothing about the map. Every
   *distinct* site is now reported.

3. **A colour target whose surface reinterprets the format** latched the
   context, which stopped every draw after it -- so the window froze on a
   half-painted frame. Blender reinterprets three targets (`r16_uint` over
   `r16_float`, `r16g16_float` over `r32_float`, `r16g16b16a16_float` over
   `r32g32_float`), and the earlier attempt to drop the attachment instead
   segfaulted because that left nothing to draw into. It now renders into a
   scratch texture of the surface's format, copied in at bind and back at
   every submit -- the same byte-preserving trick the cross-class sampler
   views use. Only a pair that changes the *texel size* is still refused.

4. **`GL_POLYGON_SMOOTH` refused the whole rasterizer state**, which binds as
   NULL and so loses culling and the scissor as well. WebGPU has no coverage
   antialiasing, so it is ignored now; Blender enables it while drawing its
   interface.

5. **An unbound storage block was bound with size 0**, and WebGPU reads an
   explicit zero as a zero-byte binding. That invalidated the bind group,
   then the command buffer, then the whole submit -- every draw batched with
   it lost. Found by grepping the log for validation errors, per the lesson
   below.

6. **The storage-block slot fell back to the GLSL `binding =` qualifier**
   when Mesa left `driver_location` unset, which the fragment halves of
   Blender's shaders do. Blender numbers those 0, 1 and 7, so its third block
   was looked for at slot 7 while st had bound it at slot 2, and the shader
   silently read the zero placeholder. Enumeration order is the block order.

7. **Storage buffers, buffer textures and shader images were missing from the
   batch-touched list.** An untouched resource reports no conflict, so
   `buffer_subdata` and the unmap upload were free to replace its bytes while
   draws that read them were still unsubmitted -- and `wgpuQueueWriteBuffer`
   is ordered against the following submit, so the *last* write is what every
   batched draw reads. Blender rewrites a 128-glyph storage buffer whole
   before each label's draw, so each frame kept only the final label:
   "Delta Transform" rendered as "orm", "Scene Collection" as "tion". This is
   the SuperTuxKart vertex-buffer failure the note on
   `gex_wgpu_batch_conflicts_usage` already describes, reached through three
   resource classes that note did not cover.

**The two instruments that mattered**, out of everything tried: grepping the
browser (or Dawn) log for validation errors, which found 5 in one line after
the earlier sRGB investigation had spent two passes learning to do it; and
comparing against the *same application* on the system Mesa, which is what
turned "the text looks odd" into "these exact labels are missing their
leading glyphs, and the last one in each group is intact" -- the observation
that names a per-batch overwrite rather than a font problem.

**What is left on Blender**: nothing known. The remaining two-colour
difference against the reference is most likely the ignored polygon
smoothing. Its viewport was previously documented here as blocked on a
WebGPU ceiling for format reinterpretation on a render target; that is now
implemented, so the note above it is superseded.

### A probe for the render-target path, and what else attaches a view

`tests/integration/programs/gl44-target-view-probe.c` covers the new path
directly, using Blender's own format pair: an R16UI view of an R16F texture,
attached to a framebuffer and written through, then read back **through the
base texture** with a sampler -- which is the property that matters, since
that is what a later pass reads. 0x3C00 is half-float 1.0, so a correct round
trip reads 255 and a scratch that was never copied back reads 0. Three cases:
a draw, `glClearBufferuiv`, and clear-draw-clear, the last because each has to
reach the base texture *in order* rather than once at the end. 3/3 natively,
agreeing with llvmpipe.

The other places a view of a colour target gets built were audited at the same
time:

- `gex_wgpu_clear_render_target` (which `gex_wgpu_clear` delegates to) renders
  into the scratch as well, and copies back straight away -- the surface may
  not belong to the bound framebuffer, in which case no submit would ever do
  it.
- `gex_wgpu_blit` needs nothing: its `srgb_compatible` test already requires
  each format to be in its own resource's linear class, so a cross-class blit
  falls through to `u_blitter`, which draws through
  `set_framebuffer_state` and picks up the scratch there.
- Shader *images* still take the texture's own format through
  `gex_wgpu_storage_texture_format`, and `glBindImageTexture` accepts any
  format of the same size class. That combination is not audited and no
  application here reaches it; it would show up as an `incompatible with the
  format` validation line rather than silently, now that the log is grepped
  for those.

### Blender's *rendered* viewport, and the ceiling that stops it

Pushing Blender past its default viewport -- `--python-expr` setting a VIEW_3D
area's `shading.type` to RENDERED, so EEVEE runs -- found one bug and one real
platform ceiling.

The bug: EEVEE attaches a **layered** colour target (two layers of a 1x1
r32_float) early in its setup, which was refused, which latched the context
and stopped every draw after it. Fixed by rendering the attachment's first
layer, which is what GL specifies when nothing writes `gl_Layer` -- the same
policy the geometry path already states for a shader that does write it.

The ceiling: EEVEE's shaders declare **storage images in formats WebGPU has
no storage support for**, and three are enough to stop it:

```
image 'out_scattering_img'      r11g11b10_float
image 'indirect_radiance_1_img' r11g11b10_float
image 'rp_value_img'            r16_float
```

Each makes the shader fail to link, and Blender dumps core on the null
program rather than degrading -- so this is not a partial render, it is no
window at all. With the layered refusal gone, these three messages are the
*only* driver complaints in the whole run.

WebGPU's base storage formats do not include either, and the optional feature
that would add them (`texture-formats-tier1`) does not exist in the pinned
emdawnwebgpu package (April 2025) -- its whole feature list is 22 names, and
neither tier is among them. So this needs either a Dawn/emdawn pin bump or a
driver-side workaround.

**The workaround that fits the driver already exists in miniature.**
`gex_wgpu_format_backing()` maps a format to a wider one and
`texture_subdata` converts through `util_format_translate`, which is how the
16-bit normalized formats are served by float32 today. Adding
`R16_FLOAT -> R32_FLOAT` and `R11G11B10_FLOAT -> R16G16B16A16_FLOAT` would
make both storage-capable, since r32float and rgba16float *are* base storage
formats, and would raise precision rather than lower it.

What stops that being an obvious win is the trigger. Promoting the formats
unconditionally doubles or quadruples the memory of two formats renderers use
heavily for HDR and depth, for the sake of the few textures actually bound as
images -- and the driver cannot know which those are at creation, because
st/mesa never asks for `PIPE_BIND_SHADER_IMAGE` (the same reason
StorageBinding is given to everything storage-capable). A **lazy promotion** -- recreate on
the first `set_shader_images` that binds the texture, convert its contents
once, bump `cache_epoch` -- sounds like the answer, and it is where this
stopped, but the shape of the code says why it is not cheap.

The backing is resolved **globally, per format**: `gex_wgpu_texture_format()`
consults `gex_wgpu_format_backing()` itself, so every site that names a
format -- sampler views, colour attachments, clear views, image views, the
copy helpers -- agrees on the wider one without knowing a resource is
involved. Making the promotion per-resource means threading a resource
through all of those, and a site that is missed does not fail loudly: it
builds a view whose format is not the texture's, which WebGPU returns as a
non-null *invalid* object, and the draw disappears. That is exactly the
failure mode that cost two passes earlier in this document.

So the two honest options are: **eager** promotion, which is a four-line
change to `gex_wgpu_format_backing()` and costs 2x memory on two formats
renderers use heavily for HDR (a real cost in a browser with a 4 GB budget --
see the note on exhausting it); or the per-resource refactor, done carefully
with the invalid-view trap in mind. Neither is guesswork-sized, which is why
this pass measured and recorded it rather than picking one. If a future pass
wants the cheap version first, put it behind an environment variable and
measure a Blender run's peak against the default before making it the
default.

### The 341-application sweep, re-run against all of this

Against the previous run: **RAN 281 -> 285, REFUSED 6 -> 2**, CRASH 11 -> 11
with an *identical* set (no application crashes that did not crash before,
none that stopped), EXIT and OK unchanged. The four that recovered --
`convolutions`, `projtex`, `shadowtex`, `texenv` -- were all refused for
`wrap=1`, GL_CLAMP, which this pass accepted. The two remaining refusals are
the GL_XOR logic-op demos, which are a genuine WebGPU ceiling.

That is the broad statement that the pass's larger changes -- the scratch
render target, the batch-touched resource classes, the un-gated uploads and
maps -- did not cost anything elsewhere.

## A compute shader may sample a texture (2026-09-03)

Found by pushing Blender's rendered viewport, and much bigger than that
application: **this driver had no support for sampling a texture from a
compute shader at all**, and the failure was not a refusal. The pass that
splits a combined `sampler2D` into the texture and sampler WebGPU wants ran
only for the graphics stages, so a compute shader reached the SPIR-V emitter
with its texture derefs lowered away and asserted there --

```
nir_to_spirv.c:4702: emit_tex: Assertion `var' failed.
```

-- which aborts the *application*. Binding a view for the compute stage was
refused separately, and that latched the context. GL 4.3's compute shaders
sample as freely as a fragment shader does, and a renderer that does its
post-processing in compute does little else, so this was a large hole.

What it took: the split pass runs for compute too, numbering its texture and
sampler above the ranges the blocks and images already use (a dispatch has
one bind group for everything, where the graphics stages have a set of their
own); the compute state carries texture and sampler tables like a graphics
shader; `set_sampler_views` and `bind_sampler_states` accept the compute
stage; and `launch_grid` binds both.

The split pass also had to stop **skipping a tex instruction that already has
a texture deref**. The comment there assumed st/mesa always lowers them away
-- true for the graphics stages, false for compute -- and the deref it leaves
names the combined variable the pass then retires, so it dangles:

```
deref_var &combined_tx (uniform sampler2D) error: entry
NIR validation failed after nir_remove_dead_variables
```

Those are repointed at the texture half, with the unused derefs cleared
before the variables they name are removed. Blender has five such samplers in
one compute shader.

`gl44-compute-texture-probe` covers the three shapes that have to work: a
filtered sample, a `texelFetch` (no sampler, a different WGSL binding shape),
and a 2D *array* sample, where the view dimension the layout names must match
the shader's. 3/3 natively and in the browser.

### Storage-format promotion, and the conflict that decides it

`GEX_WGPU_PROMOTE_STORAGE=1` widens `R16_FLOAT` to `R32_FLOAT` and
`R11G11B10_FLOAT` to `R16G16B16A16_FLOAT` through the same
`gex_wgpu_format_backing` machinery the 16-bit normalized formats use, so
that both can be storage textures. The NIR image format is promoted with the
texture, or the WGSL declaration, the bind group layout and the view would
disagree -- and `r16float` is not a WGSL storage format at all, so naming the
narrow one would not compile.

**It is off by default, and the memory is not why.** Measured: a SuperTuxKart
race peaks at 1571 MiB of video memory with it and 1563 MiB without -- half a
percent -- because these formats live in shaders rather than in the large
textures that dominate a scene. And it earns something real: without it,
selecting Blender's rendered viewport fails a shader link and Blender dumps
core on the null program.

What keeps it off is a **direct conflict with format reinterpretation**, and
this settles the question the earlier section left open. Widening is *per
format*: promoting `R16_FLOAT` stores every one of them as four bytes, so a
colour target viewing an `R16_FLOAT` texture as `R16_UINT` is no longer
looking at two bytes per texel and the copy that serves it has nothing to
preserve. `gl44-target-view` -- Blender's own pair -- goes from **3/3 to 0/3**
with the switch on. The probe written earlier the same day caught it
immediately, which is the argument for writing it.

So the cheap option is not viable, not merely expensive: **only per-resource
promotion can serve both**, recreating a texture with the wider backing when
it is first bound as an image. The reinterpretation checks now compare the
*stored* format rather than the Gallium one, so with the switch on that case
is refused by name -- "surface reinterprets the format at another texel size"
-- rather than quietly copying the wrong number of bytes.

### What actually stops EEVEE, in the end

With promotion on, Blender's rendered viewport reaches "Compiling EEVEE engine
shaders" with its whole interface and viewport drawn -- from "dumps core
before opening a window" to that. Four genuine platform limits stop it there,
and none of them is a driver defect:

1. **Image atomics.** ntv emits `OpImageTexelPointer`, whose result lives in
   SPIR-V storage class Image, and Tint reports `unknown SPIR-V storage
   class: 11`. WGSL has no atomic operation on a texture in any form.
2. **A read-write image at 16 bits.** WebGPU allows `read_write` storage only
   on r32uint/r32sint/r32float, and EEVEE's `color_accum_img` is read *and*
   written.
3. **`workgroupBarrier` in non-uniform control flow**, which WGSL forbids and
   GLSL permits. Hoisting it would change what the shader means.
4. **A dual-source blend output shape** Tint rejects: `@blend_src` on one
   member of a structure while another member with `@location` has none.

The barrier *semantics* problem next to these was ours and is fixed: zink
turns `nir_var_mem_global` into `CrossWorkgroupMemory`, which Tint refuses
(`unsupported control barrier semantics: 512`), so global now folds into ssbo
-- which is what the memory is here. That is a general translation fix; the
four above are the platform.

## Anisotropic filtering, which was refused and cost more than itself

`EXT_texture_filter_anisotropic` and, at a maximum of 16,
`ARB_texture_filter_anisotropic` -- one of GL 4.6's ten. WebGPU has it as a
sampler field, so the refusal that stood here was never necessary, and it was
expensive out of all proportion: **any** sampler with anisotropy above one was
turned away, and a refused sampler binds as NULL, so an application that
enables anisotropic filtering -- which most games do by default -- lost its
wrap modes and filters along with it and drew the wrong thing. SuperTuxKart is
launched in this tree with `--anisotropic=0` for exactly that reason; it now
runs with `--anisotropic=16` and no sampler refusal at all.

GL treats anisotropy as a quality hint rather than a promise, and WebGPU
requires all three filters to be linear above one, so a sampler that is not
fully linear takes one instead of being refused.

`gl44-anisotropy-probe` deliberately does not measure filtering quality, which
is implementation-defined. It measures what was broken: the extension is
claimed with a maximum of at least 16 (which is what st gates the ARB form
on), setting `GL_TEXTURE_MAX_ANISOTROPY` raises no error, and with anisotropy
on a `GL_REPEAT` wrap still repeats and a `LINEAR` filter still blends two
texels. Those last two are the cases a NULL sampler fails, because the default
it falls back to clamps and does not filter the way the application asked.
4/4 natively and in the browser.

### ARB_polygon_offset_clamp: implemented, measured, and not claimed

The neighbouring 4.6 item looked like a free cap: `offset_clamp` is already
carried to `depthBiasClamp`, which means the same thing, the value arrives
intact, and the pipeline key covers the whole depth-stencil state. So the cap
went in and a probe case went with it -- two coplanar quads under `GL_LESS`
with a pull of -8 units, which the second wins, and a clamp of -1e-9, a
sixtieth of a 24-bit depth unit, which should round that pull away and make
it lose.

**llvmpipe loses it. This driver still wins it.** The clamp reaches the
descriptor and the depth bias beside it works, so the clamp alone is being
ignored; Vulkan gates a non-zero `depthBiasClamp` on a device feature, which
is the likeliest place for it to vanish. The cap was withdrawn rather than
left claiming an effect that is silently absent -- an application asking for
a clamp would otherwise get an unclamped offset with no way to tell.

The browser side cannot be asked at all yet: the guest exports no
`glPolygonOffsetClampEXT`, and adding a command shifts the opcode numbering
the guest and host share (`opcodes = 1024 + index in the alphabetic 4.4 set`),
so it is a coordinated change rather than a one-line one.

The probe cases were removed rather than left as a standing failure, with the
reasoning next to `caps->polygon_offset_clamp` and in the probe's header.
Restoring them is a two-line change the day the platform honours it. Worth
noting as a pattern: **the cheap-looking caps are the ones to measure**, since
the implementation being present says nothing about the platform honouring it.

## FreeCAD renders (2026-09-03)

The last of the named target applications that had never been tested, because
it was not installed. It still is not: the distribution package could not be
fetched (the local database is stale against the mirror, and fixing that on
Arch means a full upgrade, which is not a thing to do to someone's machine
unasked). The **AppImage** needs no installation at all, and that is how this
was tested:

```
curl -L -o freecad.AppImage https://github.com/FreeCAD/FreeCAD/releases/\
download/1.1.3/FreeCAD_1.1.3-Linux-x86_64-py311.AppImage
chmod +x freecad.AppImage && ./freecad.AppImage --appimage-extract
```

**The AppImage bundles glvnd**, which is the trap `libGLX.so.0` set for GLFW
earlier in this document: `squashfs-root/usr/lib/libGL.so.1` exports 3470
entry points and would load the *system* Mesa vendor, so the driver under test
would never be reached. Since the image is extracted, the fix is to point the
bundled names at this driver's own libraries:

```
L=squashfs-root/usr/lib; INST=/tmp/gex-mesa-native/inst
for n in libGL.so.1 libGL.so.1.7.0 libGLX.so.0 libOpenGL.so.0; do
  ln -sfn $INST/lib/libGL.so.1 $L/$n; done
for n in libEGL.so.1 libEGL.so.1.1.0; do
  ln -sfn $INST/lib/libEGL.so.1 $L/$n; done
```

Then run `squashfs-root/AppRun` with the usual driver environment.

**It works.** FreeCAD 1.1.3 draws its whole interface -- menus, three
toolbars, the Model tree, the first-run page with its widgets -- and, driven
to a document with `scratchpad/fc-macro.py` (a box with a cylinder cut out of
it), its **3D viewport renders the shaded solid with its edges, the navigation
cube and the axis cross**. No refusals and no WebGPU errors in the run.

Two things worth knowing for the next pass:

- The Start page is **not** an MDI subwindow: `QMdiArea::subWindowList()`
  returns exactly one entry (the document), yet the Start tab sits in front of
  it. Activating the subwindow from a macro does not help. Clicking the
  document's tab does -- `scratchpad/appshot-click.sh <out> <settle> <x> <y>
  <cmd...>` exists for that, and the tab is at about (313, 953) in a 1280x1000
  window.
- A key sent with `xdotool key` after that click goes to the Tasks panel
  rather than the view, so the view stays where the macro left it. The
  screenshot on record is a TOP view, which is enough to show the geometry,
  the shading and the edges.

### The bug FreeCAD found immediately

Anisotropic filtering had stopped being refused earlier the same day, and
FreeCAD asks for 4x on a texture it samples *unfiltered*. Every sampler here
is built with a non-filtering twin for the vertex stage -- all three filters
forced to Nearest -- and the twin inherited the descriptor's anisotropy, which
WebGPU forbids:

```
One of minFilter (FilterMode::Nearest), magFilter (FilterMode::Nearest) or
mipmapFilter (MipmapFilterMode::Nearest) is not FilterMode::Linear while
using anisotropic filter (maxAnisotropy is 4.000000)
```

A one-line fix, and a good argument for testing a new application: none of
the 80 probes nor the four other target applications asks for anisotropy on
an unfiltered texture.

### A false alarm worth remembering

Running the probe suite from a shell that had **exported the driver's
`LD_LIBRARY_PATH`** made all 80 probes report `BUILDFAIL`:

```
/usr/bin/ld: .../libGL.so: undefined reference to `__glXGLLoadGLXFunction'
```

The *linker* had resolved `-lGL` against this driver's non-glvnd libGL. It
looks like a catastrophic regression and is nothing at all -- run the suite
with `env -u LD_LIBRARY_PATH -u LIBGL_DRIVERS_PATH -u GALLIUM_DRIVER`, or from
a shell that never exported them. `nprobe2.sh` sets what it needs per run.

## GL 4.5 is one cap away, and the cap is a silent bug (2026-09-03)

`ARB_cull_distance` is the only extension between this driver and GL 4.5 --
everything else 4.5 requires is granted and probe-verified. Setting
`caps->cull_distance` grants 4.5 immediately; the version was read back as
`4.5 (Compatibility Profile)` with the cap on. So the only question was ever
whether the result is *right*.

**It is not, and it fails silently.** `gl44-cull-distance-probe` was written
to answer exactly this, and llvmpipe passes all three of its cases:

| case | with the cap on |
|---|---|
| `all_negative_culled` -- every vertex negative, nothing drawn | **pass** |
| `all_positive_kept` -- the control | **pass** |
| `mixed_kept` -- one vertex negative, triangle drawn *whole* | **fail** |

The failing case is the one that tells the two features apart. A clip distance
cuts a primitive along the plane where it reaches zero; a cull distance
discards the primitive entirely, and **only when every vertex is negative**,
so a mixed-sign primitive is drawn whole -- including the corner where the
distance is negative. With the cap on, that corner is missing: the primitive
was *clipped*.

The mechanism, which is worth knowing before anyone tries this again: Mesa's
GLSL linker runs `nir_merge_clip_cull_distance_vars` **unconditionally**
(`gl_nir_linker.c`, "Combine clip and cull outputs into one array"), packing
the cull values into the clip array behind the clip ones. zink's emitter then
hands that whole array to WebGPU's `clip_distances`. So the cull values clip,
and "map cull onto clip" is not a shortcut that loses a little quality -- it
is a silent geometry bug.

**What implementing it would take**, and why it was not done here. Two parts:

1. Split the merged array before ntv, so only the first
   `clip_distance_array_size` elements reach `clip_distances` and the rest
   travel as an ordinary varying.
2. Cull per primitive from that varying. This part is nearly free:
   `gex_wgpu_cull_polygons` is already exactly this shape -- it captures
   post-vertex-shader outputs with `gex_wgpu_capture_positions`, resolves the
   draw's indices to vertex numbers, tests each primitive on the CPU, and
   returns a rewritten index list that the caller re-issues as a user-index
   draw. Cull distance needs the same walk widened to points and lines (the
   current one handles triangles, quads and polygons) and a different test.
   The capture already takes an arbitrary set of varying slots, one vec4 each.

What makes it more than a morning's work is *where* part 1 lands: the same
path user clip planes take. `caps->clip_planes = 0` has st run
`nir_lower_clip_vs`, so **every application clip plane is a gl_ClipDistance
write** -- and applications running here depend on it. Breaking that to add a
rarely-used extension, for a version number no target application asks for,
is the wrong trade; 4.4 is the stated target and it is met.

The probe stays in the suite as a **feature-gap marker**: llvmpipe reports
3/3 and this driver reports 0/0 (extension absent), so it shows as a DIFF
alongside `quad` and `texture-sampler`. It is also the guard -- anyone who
enables the cap will see it move from 0/0 to 2/3, which is the suite saying
what this section says.

## The native CTS as a bug source (2026-09-03)

There is a **native** CTS build at `/tmp/gex-glcts-native`, and it reaches
driver bugs far faster than an application does:

```
cd /tmp/gex-glcts-native/external/openglcts/modules
env LD_LIBRARY_PATH=/tmp/gex-mesa-native/inst/lib:~/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=/tmp/gex-mesa-native/inst/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=gex_wgpu DISPLAY=:78 \
  ./glcts --deqp-case='KHR-GL44.compute_shader.*' --deqp-surface-type=fbo \
          --deqp-surface-width=64 --deqp-surface-height=64
```

The mustpass list is at
`gl_cts/data/mustpass/gl/khronos_mustpass/main/gl44-main.txt` (16843 cases);
`awk -F. '{print $1"."$2"."$3}' | sort | uniq -c` gives the family sizes.
First measurements: `compute_shader` 2/19, `texture_view` 1/7,
`buffer_storage` 6/9.

**Collect the refusal reasons first**, not the pass rate:
`grep -aE "rejected:" | sed 's/.*rejected: /REJECT: /' | sort | uniq -c`.
For `compute_shader` that turned 17 failures into four distinct causes, which
is what makes the work tractable.

### An array of uniform blocks is one variable over many slots

`uniform InputBuffer { ... } g_in_buffer[12]` gives Gallium twelve constant
buffers -- each element is bound separately -- and NIR **one** variable at the
first of them, whose type is the *element* struct with no array left in it. So
there is no length to read anywhere, and matching a load's slot against
`driver_location` found nothing for the other eleven:

```
compute shader rejected: load_ubo slot=2 block=(none) interface=no fields=-1
```

The block whose `driver_location` is the greatest one at or below the slot
owns it -- no length needed, and still right when a shader has several arrays.
Two things about this were easy to get wrong, and both were, in the order
written:

1. The search must be bounded by `nir->info.num_ubos`. Unbounded, it answers
   *every* slot with the nearest block, so a load the driver genuinely cannot
   account for reads the wrong buffer **silently** instead of being refused.
   That is a worse outcome than the bug being fixed.
2. Each element needs a variable of its own, or they all deref the first
   one's binding and read the first one's data -- silently again. The fixup
   after this pass assigns the descriptor set and the binding from
   `driver_location`, including the spill to a storage binding above the
   device's uniform-buffer count, which this case needs: twelve blocks plus
   the default one exceeds the twelve WebGPU guarantees.

`resource-ubo` passes now. Arrays of uniform blocks are ordinary in engine
code, so this was never only a conformance question.

### Barriers, twice more

Tint's SPIR-V reader **does not implement `OpMemoryBarrier`** ("unhandled
instruction with opcode 225"), which is what zink emits for GLSL's
`memoryBarrier()` family -- a barrier with no execution scope. WGSL has no
standalone memory barrier; `storageBarrier()` and `workgroupBarrier()` are
control barriers. Giving such a barrier the workgroup *execution* scope makes
zink emit `OpControlBarrier`, which Tint takes, and a control barrier is
**stronger** than the memory barrier it replaces.

Tint then wants the *memory* scope to be the workgroup as well
("storageBarrier requires workgroup memory scope"). A workgroup is the widest
scope WGSL has, so that is what it gets -- weaker than the device scope GLSL
asked for, and the same ceiling as the cross-workgroup semantics already
folded away here. Compute only: WGSL has no barrier in the other stages, so
one there stays a named refusal instead of becoming an illegal module.

One shader then reports `'workgroupBarrier' must only be called from uniform
control flow`, which is a WGSL restriction GLSL does not have. Promoting a
memory barrier into a control barrier is what exposes it; there is nothing to
do about that one.

### Atomic counters: a convention clash, left alone on purpose

`resource-atomic-counter` declares one SSBO and two atomic counters. Mesa's
`nir_lower_atomics_to_ssbo` creates storage blocks for the counters and puts
their block index in **`data.binding`**
(`ssbo->data.binding = ssbo_offset + var->data.binding`), leaving
`driver_location` at zero -- the opposite of an ordinary GLSL storage block,
where `driver_location` is the block index and `binding` is the GL binding
point. This driver's fixup then overwrites `binding` with
`SSBO_FIRST_BINDING + driver_location`, so the index is gone before the
lowering looks for it. `GEX_WGPU_SSBO_LOG` now prints the variables, and shows
it plainly: four of them, all at `driver_location` 0 and `binding` 32, for
block indices 0, 1 and 2.

**Not fixed, deliberately.** The only marker distinguishing the two
conventions is that Mesa names the type `counters`, and a heuristic that
guesses wrong mis-slots *ordinary* storage blocks -- which every modern
application uses. Two atomic-counter cases are not worth that risk, and the
refusal is loud. If a future pass wants it, the sound fix is to capture the
block index **before** the driver's fixup overwrites `binding`, not to guess
afterwards.

## Three crashes the CTS found, and what they were (2026-09-03)

Continuing the CTS work above. Every one of these killed the *application*
rather than reporting anything, which is the failure mode this driver tries
hardest to avoid.

### A geometry shader may take points and emit triangles

`KHR-GL44.texture_view.view_sampling` needs it, and so does every billboard,
particle and impostor ever drawn. The path refused a shader whose input and
output primitives were not the same class -- and the refusal was hiding a
defect: `verts_per_input_prim` was derived from the **output** primitive,
while its two other readers treat it as the input count (one derives the
draw's topology from it, the other divides a vertex count by it). A
points-in shader would have read three vertices per point. Separating the two
is the fix, and `gl44-gs-point-expand-probe` is the proof -- two points, two
quads, a pixel checked inside each *and* in the gap between them, with the
tint carried through the geometry stage differently per point.

### A tex instruction with no deref aborts the emitter

Lifting that refusal immediately exposed the next layer: `emit_tex: Assertion
'var' failed`, this time from a *tessellation control* shader compiled as
compute. The combined-sampler split is what rebuilds those derefs, and it does
not run on every path. `gex_wgpu_compile_nir` now checks and refuses by name,
naming the stage, so the next path to need the split is a message rather than
a core dump.

### A vertex shader that samples cannot be run as compute

Three `texture_view` cases were producing this:

```
The entry-point uses bindings in group 3 but [PipelineLayout] doesn't have a
BindGroupLayout for this index
```

A vertex stage's texture and sampler live in descriptor set 3; a compute
dispatch has one bind group, number zero. **An invalid compute pipeline makes
the command buffer invalid**, so the submit fails and everything recorded with
it is lost -- not just that pass. Two of the four callers of the
varying-variant builder already checked for this and the geometry and
tessellation paths did not; the transform-feedback capture has a builder of
its own that did not either. Both builders check now, so every caller is
covered.

### Software fp64 crashed because int64 was never claimed

This one is worth the whole section. The driver asks Mesa for
`nir_lower_fp64_full_software` -- WGSL has no doubles, and Mesa can build them
out of 64-bit integers -- but it never set `caps->int64`, and
`src/compiler/glsl/float64.glsl` opens with
`#extension GL_ARB_gpu_shader_int64 : require`. So Mesa's built-in source
failed to compile, and `glsl_float64_funcs_to_nir` cloned the null shader
*before* checking `CompileStatus`:

```
#0  nir_shader_clone (mem_ctx=0x0, s=0x0)
#1  glsl_float64_funcs_to_nir
#2  st_link_glsl_to_nir
```

A segmentation fault with nothing naming fp64 anywhere in it. Two fixes: the
null dereference in Mesa (one reordering, so the failure becomes the
diagnostic it was always meant to be), and `caps->int64 = true`, which is
honest -- `lower_int64_options` already asks for every 64-bit integer
operation to be expanded into 32-bit pairs, and that is the very machinery the
fp64 lowering runs on.

`KHR-GL44.vertex_attrib_binding` goes from a segfault with no results at all
to 12/30, and **`KHR-GL44.gpu_shader_fp64.builtin` runs 478/650** -- software
doubles work, where before any shader using one killed the process.

### Tint aborts on an image shape WGSL has no type for

GLSL has `imageCube`, `imageBuffer`, `image2DRect`, `image1DArray` and
`image2DMS`. WGSL has exactly four storage textures: `texture_storage_1d`,
`_2d`, `_2d_array`, `_3d`. This driver mapped every other shape to 2D **in its
own tables** while the shader went on declaring the real one, so the layout
and the module disagreed -- and Tint does not report that. It raises an
internal compiler error building the `storage_texture` type, which aborts the
process with SIGILL from inside its own destructor, with nothing in the
backtrace naming the image.

`gex_wgpu_image_view_dimension` now answers which shapes exist and both
collection paths refuse the rest by name. `KHR-GL44.shader_image_load_store`
runs to completion at 6/51 instead of aborting. `GEX_WGPU_IMAGE_TRACE` lists a
shader's images, which is what identified the three shapes involved.

**The pattern across all five**: a driver limit expressed as "map it to
something close and carry on" becomes an abort in a component that has no idea
what GL is. Refusing by name costs a feature; guessing costs the process.

## A crash sweep over all 89 GL 4.4 families (2026-09-03)

Crashes are the worst failure this driver has, and they also abort the family
they are in, hiding everything after. So rather than chasing scores, the whole
mustpass was swept for how each family *ended*:

```
for fam in $(awk -F. '{print $2}' gl44-main.txt | sort -u); do
  timeout 90 ./glcts --deqp-case="KHR-GL44.$fam.*" --deqp-surface-type=fbo \
      --deqp-surface-width=64 --deqp-surface-height=64 >/tmp/f.log 2>&1
  printf '%s\t%s\n' "$fam" "$?"
done
```

132 is SIGILL, 134 an abort, 139 a segfault; 124 is only the timeout and means
the family was still running happily. Result after the fixes above: **89
families, one crash** -- and it is fixed below. The script is
`/tmp/cts-crashsweep.sh`; a family that crashes has its log kept as
`/tmp/cts-crash-<family>.log`.

### ARB_clear_texture: 10/78 to 78/78

`glClearTexImage` is core in GL 4.4 and the driver had no `clear_texture`
hook, so st fell back to `u_default_clear_texture`, which clears by
**rendering** into the texture. That needs the format and target to be a
render target, and a 1D texture cannot be one in WebGPU -- so the fallback
called `clear_render_target` for a shape this driver refuses, **the refusal
latched the context**, and the rest of the run collapsed into
`GL_OUT_OF_MEMORY` from every later map. Only ten of the seventy-eight cases
were really about clearing.

Filling a texture with one colour needs no rendering here at all: the value
arrives in the resource's own format, so repeating it across the box and
handing that to `texture_subdata` is the entire operation -- and that path
already knows the wider backing formats, the block sizes and the row
alignment. A few lines, and it works for every target and format the render
path could not.

Depth, stencil and compressed formats go to `util_clear_texture_sw` rather
than to the default, because the default would render for those too and this
driver's depth clear only does a *whole level* -- so `glClearTexSubImage` of a
region refused and latched all over again.

**78/78**, with no refusals and no `GL_OUT_OF_MEMORY` anywhere in the run.

### A patch topology has no primitive count

The last crash. A draw with a query active counts the primitives its vertex
stage produced, via `u_prims_for_vertices` -- which has no entry for a patch,
because a patch's vertex count comes from `glPatchParameteri` and reaches the
driver through `set_patch_vertices` rather than in the draw info. So
`u_prim.h`'s table carries an increment of zero and the helper *asserts*,
aborting the application. Every draw with a patch topology and a query active
reaches it, which is ordinary for a tessellating program.

Patches now count as patches, which is well defined and which the driver has
the vertices-per-patch to compute. That is not what `PRIMITIVES_GENERATED`
means on a tessellating draw -- GL asks for what the evaluation stage emitted
-- so the query is approximate there and the comment says so. Approximate
beats aborting. `KHR-GL44.tessellation_shader` runs to completion at 6/20.

## Two more from the CTS, and a correction to how it is read (2026-09-03)

### Read the failure count, not the gap

Ranking families by `total - passed` is wrong: it counts **not supported** as
failure. `shader_image_size` looked like 73 missing and is really 1 passed, 13
failed, 60 skipped; `constant_expressions` looked like 464 missing and has
**zero** failures. Collect all three numbers:

```
p=$(... | grep -aoE "Passed: +[0-9]+" | tail -1 | grep -oE '[0-9]+')
f=$(... | grep -aoE "Failed: +[0-9]+" | tail -1 | grep -oE '[0-9]+')
n=$(... | grep -aoE "Not supported: +[0-9]+" | tail -1 | grep -oE '[0-9]+')
```

By real failures the list is: `shader_multisample_interpolation` 99,
`shader_storage_buffer_object` 91, `geometry_shader` 64, `texture_gather` 46,
`shaders` 37 of 2397, `transform_feedback` and `compute_shader` 16 each.

### Count storage blocks, not block variables (a regression of my own)

Earlier the storage-block slot fallback was changed from "the GLSL binding
qualifier" to "the enumeration order of the variable list", to fix Blender's
fragment shaders. That is right when a block is one variable -- and a block
declared **without an instance name** is one variable *per member*.
`KHR-GL44.shader_bitfield_operation`'s single `buffer Data { ... }` of fourteen
members arrived as fourteen variables sharing a binding and an interface type,
the counter gave each its own slot, and the layout asked for thirteen storage
buffers that do not exist:

```
The number of storage buffers (12) in the Compute stage exceeds the maximum
per-stage limit (10)
```

The pipeline was invalid, the dispatch never ran, and every case whose inputs
were not already zero read zeros. **Only the `_zero` variants passed, across
every function in the family** -- which is what named the cause: not one
bitfield builtin was wrong, the shader had never run. The interface type says
which block a member belongs to, so distinct interface types are what get
counted. **324/324, from 51/324**, with Blender unchanged.

The lesson generalises: when a fix keys on "the order of the variable list",
ask what happens when one declaration produces several variables.

### Centroid and sample must match across the stages

WebGPU rejects a pipeline whose fragment input and vertex output disagree
about interpolation *sampling*, not only about the mode. GL puts the qualifier
on the fragment input alone, so every `centroid in` mismatched -- and an
invalid render pipeline invalidates the command buffer, losing every draw
batched with it.

The driver already builds a vertex variant matching the fragment stage's
interpolation modes; centroid and sample are separate bits on the variable, so
they were never carried. They are now, keyed into the same variant. **That
alone changed nothing**, because zink's emitter decorates Centroid and Sample
only on a fragment *input* -- Vulkan tolerates the disagreement, so the output
branch never needed it. Emitting it there too is what gives the copy something
to travel on.

The nine mismatch rejections are gone. The family's score does not move,
because what remains is `gl_SamplePosition` (SPIR-V builtin 19) and
`interpolateAtSample` (GLSL.std.450 77), neither of which WGSL has in any
form. The fix is not about the score.

### Where the rest of shader_storage_buffer_object's 91 failures are

Measured, not fixed: 34 are `var with 'storage' address space and 'read_write'
access mode cannot be used by vertex pipeline stage` (WGSL forbids a writable
storage buffer in the vertex stage); 20 are `.length()` on an unsized array,
which WGSL has as `arrayLength` but zink's emitter reaches through its own
SSBO layout rather than this driver's, so it needs the size supplied by the
driver instead -- the shape to copy is the draw-parameters block; 6 are a
storage block indexed at run time; 4 are `OpMemoryBarrier` outside compute,
where WGSL has no barrier at all.

## The geometry stage, generalised: 12 of 76 to 73 of 76

`KHR-GL44.geometry_shader` was the densest remaining cluster after the
storage-block and interpolation work, and almost all of it came down to the
geometry path implementing one shape of geometry shader and refusing the rest
at *link* time -- which fails the application, not the draw.

The path itself is sound and did not change: a geometry shader runs as a
compute pass that writes vertices and indices for an ordinary indexed draw.
What changed is everything around it that used to be hard-coded.

### Input and output primitives are separate tables now

Two refusals accounted for 29 of the 64 failures:

    gex_wgpu geometry shaders must output points or a triangle strip   (15)
    gex_wgpu geometry shaders must take points or triangles            (14)

GL has five input primitives and three output ones. The input decides only how
many vertices an invocation reads and which draw topology feeds it; the output
decides how many indices a primitive takes and how many of them a strip of
`vertices_out` closes. Both are now tables in `gex_wgpu_create_gs_state`, and
the emitter grew a line-strip branch beside the triangle-strip one -- a line
strip closes a line from the *previous* vertex, one back rather than two, with
no winding to alternate.

Removing those refusals moved the score not at all: 12/76 before, 12/76 after,
with every shader now compiling. That is worth stating plainly, because it is
the shape of a whole class of driver work -- **a refusal often hides a second
defect, and lifting it only reveals the one behind.**

### The one behind: indexed and instanced draws

Each `rendering` case runs its geometry through *five* draw calls -- arrays,
arrays-instanced, elements, elements-instanced, range-elements -- and the
geometry path took only the first. So every one of those cases failed however
many primitives it could compile.

Both were much less work than they look, because the vertex capture already
had the pieces:

  * **Indexed.** The capture can read a slot table -- one entry per captured
    vertex, naming which vertex of the draw it is -- because the wide-line
    rewrite needed exactly that. Building one for the geometry pass makes the
    capture write the vertex shader's outputs in *primitive* order, so the
    geometry pass goes on reading `invocation * verts_per_input_prim + vertex`
    and never learns that indices exist.
  * **Instanced.** Unrolled into the capture rather than handed to the
    rasterizer. The capture runs once per (instance, vertex) and the geometry
    pass once per (instance, primitive), both instance-major -- and because
    `prims_per_instance * verts_per_input_prim == count`, that same linear
    formula already lands on the right vertex. The capture gained a
    slot-to-(instance, vertex) split and, with it, real `gl_InstanceID` and
    real vertex-attribute divisors, which it had never had: instanced capture
    was refused outright, so the layout carried no divisor and every attribute
    was read per vertex.

That took the family from 12/76 to 39/76.

### A strip is a topology, not a primitive

A `layout(triangles)` shader may be fed triangles, a strip or a fan; GL calls
those the same primitives assembled differently. The path compared the draw's
mode against the shader's declared one and declined anything else. Now
`gex_wgpu_gs_input_class` maps a draw topology to the class of shader that
takes it, and anything that is not already a plain list is resolved through
the same slot table the indexed case uses.

Fans and loops never reached this code -- `GEX_WGPU_PRIM_MODES` does not claim
them, so u_primconvert had already rewritten them into lists. Strips it does
claim, which is why only strips were left.

**GL's vertex order for a strip is not transform feedback's.** Both keep the
winding of an odd triangle by exchanging two of its three vertices, but they
exchange a different two: feedback swaps the last pair, `(i, i+2, i+1)`, and
GL's table for what a geometry shader reads swaps the first, `(i+1, i, i+2)`.
A shader that reads `gl_in[0]` and `gl_in[1]` separately can tell, and the
CTS's rendering cases do. The two orders now live in two functions.

### Transform feedback out of a geometry shader

28 of the 37 failures that remained at that point were this one feature --
every `primitive_counter` case, both `input` cases, and all of `adjacency`.
The driver captured nothing and said so only on stderr, leaving the feedback
buffers holding whatever they held.

It is not the same problem as capturing a vertex stage. A vertex shader emits
exactly one vertex per invocation, so where each writes is known before the
draw. A geometry shader emits a number it decides at run time and may emit
none -- and an unemitted index slot holds a degenerate primitive that an
emitted degenerate one is indistinguishable from. So:

  1. the geometry pass records how many primitives each invocation closed,
     written at the end of the shader and *before* the bounds check is
     lowered, so `nir_lower_returns` predicates it away for an invocation with
     no primitive exactly as it does everything else;
  2. a scan turns those counts into the offset each invocation starts at;
  3. a compaction pass -- one invocation per (invocation, primitive slot) --
     writes the captured components packed, reading the vertices through the
     indices the geometry pass already wrote, so a triangle strip is recorded
     as separate triangles with its winding applied and this pass never learns
     that strips exist.

The scan is one invocation running a serial loop. That is the honest shape for
what this is: the upgrade, if a profile ever asks, is the usual two-level scan
-- block sums in parallel, a serial pass over those, then each invocation
adding its within-block prefix -- and it needs no change anywhere else,
because only that module's contents would differ.

How much was recorded is a number only the GPU has, and both the append offset
on each target and the primitive queries need it on the CPU, so it is read
back. That stalls. The alternative is to leave it on the target and resolve it
at the three places that can observe it -- a resumed capture, an appended one,
`DrawTransformFeedback` -- which is the shape to reach for if this ever
matters. It recorded *nothing* before, so four bytes are cheaper than the
bookkeeping.

### Two smaller things the tests then found

**gl_PrimitiveIDIn must survive a primitive restart.** The driver splits a
restart draw into restart-free sub-draws through Mesa's own helper, and each
sub-draw's invocations start at zero -- so the geometry shader numbered its
primitives 0, 1, 2, 0, 1, 2 where GL numbers them across the whole draw call.
The sub-draws cannot carry a base, being ordinary draws, so the context holds
one for the length of the split. That was the nine `*_rp` cases.

**PRIMITIVES_GENERATED counts the last stage before rasterization.** With a
geometry shader that is the geometry stage, not the draw's vertex count -- and
the generic counting in `draw_vbo` was adding the vertex-stage count, the
geometry path was adding its own, and the re-entry that rasterizes what the
geometry stage produced was adding a third. A shader emitting six primitives
from three points reported **18**. The generic count now stands aside for both
a bound geometry shader and that re-entry, and a draw with a query but no
feedback bound runs the scan alone to get the number.

### And one bug that was not this driver's

`adjacency_*_triangle_strip` survived all of the above. The draw was arriving
as `TRIANGLES_ADJACENCY` -- `GEX_WGPU_PRIM_MODES` claims no adjacency
topology, so u_indices had already converted it -- and the six vertices it
produced were wrong.

`u_indices_gen.py`'s `tristripadj` writes the odd primitive from GL's table
and the even one as the identity, `(i+0 … i+5)`. That is right for the
triangle's own three vertices, which are at slots 0, 2 and 4 either way, and
wrong for the three *adjacent* ones -- which nothing noticed, because the
consumer it was written for discards adjacency. A geometry shader reads all
six. GL's table (4.6 spec, table 10.2) gives the even primitive
`(i, i-2, i+2, i+6, i+4, i+3)`, and gives the first and last primitives rows
of their own because the ends have no neighbour to borrow from.

The identity also walked off the end of the buffer: the last primitive of a
12-vertex strip read index 12. That is visible in the driver's own dump --

    GEX_WGPU_GS_ASSEMBLY=1
    gex-gs:   prim 3: 8 4 6 9 10 12      <- before
    gex-gs:   prim 3: 8 4 6 9 10 11      <- after

-- which is what that flag is for, and how this was found at all.

### Where it stands

`KHR-GL44.geometry_shader` 12/76 -> 73/76. The three left are not the geometry
stage: `layered_rendering` cannot build its texture, and
`layered_rendering_boundary_condition_various_textures` and `stencil_support`
are the layered-FBO and stencil paths.

New probes, both gated in the browser: `gl44-gs-topology-probe` (line-strip
output, lines in, triangles-adjacency in, indexed, instanced, and a strip draw
whose only discriminator is which vertex `gl_in[0]` is) and
`gl44-gs-feedback-probe` (packing, ordering, strip decomposition, an
invocation that emits nothing, and `GL_PRIMITIVES_GENERATED`). The native
suite is 83 OK with the same three known differences; STK is unchanged at
mean 67 / stdev 17.

## The refusal latch was costing whole families

`.length()` on an unsized storage array turned out to be two findings, and the
second is much larger than the first.

### `.length()` itself

WGSL has `arrayLength`, and this driver's flattened storage block -- a struct
whose one member is a runtime-sized array of words -- is exactly the shape it
wants. What did not work was the route: ntv's `emit_get_ssbo_size` takes the
length against *zink's* descriptor array, `ctx->ssbos[2]`, indexed by buffer
number, which names a binding layout no other consumer of that emitter has.

So ntv gained `deref_buffer_array_length` -- the NIR intrinsic already existed
and nothing emitted it -- which takes the length against the block the deref
names, assuming no layout at all. The driver lowers `get_ssbo_size` onto it
and multiplies by four, because `get_ssbo_size` is defined to give bytes and
its consumers go on to compute `(size - offset) / stride`.

One thing had to be put back. **A block whose only use is `length()` has no
deref anywhere**, so nothing keeps its variable alive and NIR removes it long
before the driver sees the shader -- leaving a hole in the slot numbering that
the `length()` still names. KHR-GL44's unsizedArrayLength shaders declare
eight blocks and arrive with seven variables, numbered 0-4, 6, 7. The driver
recreates it, marked read-only, because a block that is only measured must not
become a writable storage variable in a vertex shader -- WGSL forbids that
outright, so putting it back to let a vertex shader measure a buffer would
have been what stopped it.

That took `advanced-unsizedArrayLength` from 0/28 to 19/28. The nine left are
the separate WGSL rule above: those shaders genuinely write storage from the
vertex and fragment stages.

### The second finding

The subfamily passed 19/28 run on its own and 3/28 run inside its family.

`ctx->failed` is never cleared, and **a refusal latched it**. So the first
shader the driver could not compile killed everything after it: every later
draw and dispatch returned early, unrelated calls came back as
`GL_OUT_OF_MEMORY`, and dEQP stopped running the family's remaining cases
altogether -- which is why the case *counts* below change as well as the
scores.

Four sites, all of the same kind -- a state combination this backend does not
serve, none of which touches the device:

  * the draw path's refusal list (no vertex shader, no fragment shader, no
    colour target, multi-draw, topology, index size, primitive restart, …);
  * `launch_grid` with no compute state, which means the compute shader was
    refused when it was created;
  * the tessellation caller, which turned `gex_wgpu_tess_draw`'s decline --
    already refused *by name*, which was the point -- back into a latch;
  * `texture_subdata` refusing an upload, which left one texture stale and
    took every later operation on every other object with it.

The geometry path had never done this: its caller returns without latching,
and its own declines say "declined, not latched" for the reason. That is the
model the other three now follow. `gex_fail` still means what it should --
the device has actually broken.

| family | before | after |
| --- | --- | --- |
| `shader_storage_buffer_object` | 28/124 | **67/124** |
| `transform_feedback` | 5/21 | **18/21** |
| `compute_shader` | 3/19 *(19 of 42 run)* | **24/42** |
| `tessellation_shader` | 6/20 *(20 of 140 run)* | **31/140** |
| `geometry_shader` | 73/76 | 73/76 |

Two of those families were only *reporting* a fifth and a seventh of their
cases. Any earlier ranking of "the biggest remaining clusters" taken from a
whole-family run was measuring the latch as much as the driver.

**What this means outside the CTS is larger.** An application that hits one
unsupported shader used to lose every frame afterwards -- not the feature, the
whole context, surfacing as `GL_OUT_OF_MEMORY` from whatever it called next.
Now it loses the draw. The app sweep's shape did not change (285 RAN, 2
REFUSED, both logic-op), because those applications never hit a refusal in the
first place; the ones this helps are the ones that did.

New probe `gl44-ssbo-length-probe`, gated in the browser: the whole buffer,
a `glBindBufferRange` over half of it (so a driver reporting the allocation
rather than the binding fails there and only there), a fixed member ahead of
the array, and `length()` bounding a loop that writes every element.

### And the run itself was ending 31 cases in

With the four latch sites fixed, a full GL 4.4 mustpass run still stopped
after 31 of 16843 cases:

    Test case 'KHR-GL44.transform_feedback.geometry'..
    Mesa: error: GL_OUT_OF_MEMORY in glBeginQuery
      InternalError (Error)
    Test run was ABORTED!

Not a latch this time. `gex_wgpu_begin_query` held **one** active query, and
GL allows one per *target*: an occlusion query, a primitives-generated query
and a primitives-written query may all be running at once. That test begins
two, the second `glBeginQuery` returned false, st/mesa turned that into
`GL_OUT_OF_MEMORY`, and dEQP treats it as a fatal ResourceError.

The context now holds one slot per target group -- occlusion, generated,
emitted -- and each counting site adds to the group it belongs to rather than
switching on whatever single query happened to be active. A non-zero *stream*
index is accepted too, and answered with zero, which is the truth here: a
geometry shader that writes any stream but zero is refused when it is created,
so nothing is ever produced on the others. Refusing the query outright was the
last remaining way for one unsupported thing to end an entire run.

`transform_feedback` 5/21 -> **19/21**.

The general shape of all five of these is one thing worth remembering: **the
cost of a refusal is not the feature it refuses.** Four of them cost every
operation after them, and the fifth cost every test after it. Refusing by name
is right; refusing by killing the context is what made these expensive, and
the geometry path had the answer written in a comment the whole time.

### Two more, found by running the suite end to end

Fixing the four latch sites let the mustpass reach 864 cases instead of 31,
and it then stopped at `framebuffer_blit`: a multisample resolve between two
formats `gex_wgpu_resolve` declines by name, whose decline the *caller* turned
into a latch, whose latch turned the next `glReadPixels` into
`GL_OUT_OF_MEMORY`, which dEQP treats as fatal. Exactly the tessellation
pattern, in a different caller.

Rather than fix that one site and find the next, the rule was applied to the
class it belongs to. Gallium's clears, blits, copies and state setters return
**void**: the caller cannot be told the operation failed, so latching does not
report anything -- it only converts a local no-op into a global failure.
Eighteen more sites across `clear`, `clear_render_target`,
`clear_depth_stencil`, `resource_copy_region`, `set_framebuffer_state`,
`set_viewport_states`, `set_scissor_states` and `set_blend_color` now call
`gex_decline` instead, which reports the site once, exactly as `gex_fail`
does, and leaves the context alive.

`gex_fail` keeps its meaning: the device has broken and nothing after it can
be trusted. That is a real state and it is worth latching. What was wrong was
using it for "this driver does not implement that".

### The colour-attachment budget was being spent in the wrong currency

With the latches gone, a full mustpass run reached `draw_buffers`, and there
the **GPU was lost**:

    Total color attachment bytes per sample (56) exceeds maximum (32)
      with formats ([ 7 x TextureFormat::RGBA8Unorm ])
    ...
    radv/amdgpu: The CS has been cancelled because the context is lost.
      This context is guilty of a hard recovery.

There was already a budget check here. It spent the budget in the formats'
*own* byte sizes -- seven rgba8 targets came to 28 against a limit of 32, so
it let them all through -- and WebGPU does not count that way. Its table
charges everything from 32 to 64 bits wide a flat **eight**, so those seven
are 56, and the guaranteed budget of 32 buys **four** rgba8 attachments and
not the eight the old comment claimed. The accounting is now the spec's own,
alignment included; `gex_wgpu_color_sample_cost` has the table.

The native path already asks for 64 at device creation. This adapter grants 32
regardless, so four is what there is, and `draw_buffers_1` fails honestly
instead of building a pipeline WebGPU rejects.

### Still open: a GPU hang in draw_buffers_1, late in a long run

The fix above removed the *validation* error and the run got no further: the
device is still lost at the same case, now with no validation error before it.
What is established:

  * `draw_buffers_1` **alone** is clean -- it fails, drops the targets past the
    budget, and the device survives;
  * the last 121 cases before it reproduce the loss;
  * the last 16 reproduce it;
  * the last 8 do **not**, and the 8 before those plus the test do **not**.

So it is cumulative rather than a specific pair, which points at something the
driver accumulates rather than at one bad command buffer. VRAM at rest is
unchanged (~520 MB) so it is not a simple allocation leak.

**I stopped bisecting here deliberately.** Each attempt costs an AMD hard
recovery on a machine that cannot be physically reset, and this blocks a
*measurement* rather than any application -- the app sweep is unaffected and
every target app renders. The next pass should start from the four facts
above rather than from scratch, and is probably better served by instrumenting
what the driver holds live across contexts than by more bisection.

Until then the mustpass is measurable to 878 cases, which is where it stops:
**853 passed, 9 failed, 16 not supported.** Before this pass it stopped at 31.

## gl_SamplePosition, and the two gates that hid it

`KHR-GL44.frag_coord_conventions.multisample` failed with

    Tint rejected SPIR-V: error: unknown SPIR-V builtin: 19

which is `SamplePosition`. I had recorded that as a WGSL ceiling. It is not
one: WebGPU does not let a pipeline *place* its samples, so they are at the
standard locations Vulkan and D3D both specify, and the sample *index* does
reach WGSL as `@builtin(sample_index)`. The position is a lookup on
gl_SampleID, which already worked.

The other half was worse and quieter. `get_sample_position` was
unimplemented, so `glGetMultisamplefv(GL_SAMPLE_POSITION, i)` answered
(0.5, 0.5) for **every** sample of a 4x target -- an answer true of no sample
but the single-sampled one, returned without complaint. GL requires that query
and gl_SamplePosition to agree, and they cannot disagree now because there is
one table.

llvmpipe reports exactly the same four points -- (0.375, 0.125),
(0.875, 0.375), (0.125, 0.625), (0.625, 0.875) -- so no y flip is needed
despite this driver rendering flipped: the flip cancels the difference between
GL's lower-left pixel origin and the table's upper-left one.

### Three things had to line up, and each failed silently

  1. **gl_SamplePosition is a variable, not an intrinsic.** GLSL gives it as a
     `nir_var_system_value` variable, which is what the SPIR-V emitter
     decorates and so what Tint refuses; only a shader already through
     `nir_lower_system_values` has `nir_intrinsic_load_sample_pos`. Matching
     the intrinsic alone found neither form.
  2. **It cannot have a base module.** The position depends on the
     framebuffer's sample count, which is not known when the shader is
     created -- the same reason a `samplerBuffer` defers, and with the same
     consequence: compiling eagerly does not give a wrong module, it gives one
     Tint refuses, which reaches the application as a link error.
  3. **`gex_wgpu_fragment_module` has *two* "does this need a variant?"
     gates**, and both had to learn about it. Missing either returns
     `fs->module` -- NULL, because the shader is deferred -- and the caller's
     `if (!fragment.module) return;` is a silent early exit. The draw did not
     happen, no message was printed anywhere, and the probe's marker channel
     is what said so: a constant written alongside the position, so a zero
     result could be told from a target nothing wrote.

That marker is worth copying. "The value is wrong" and "the draw never
happened" look identical in a readback, and they have nothing to do with each
other.

New probe `gl44-sample-position-probe`, gated in the browser: the query
reports four distinct in-range points; the shader compiles at all; the two
agree per sample; and an edge through the middle of a pixel covers two of its
four samples and not the other two -- `covers 1 0 1 0`, the same on llvmpipe,
which is what makes the positions describe geometry rather than agreeing with
themselves.

What is left in `shader_multisample_interpolation` is `interpolateAtSample`
and `interpolateAtOffset` (GLSL.std.450 77 and 78). Those could be emulated
from screen-space derivatives -- `v + dpdx(v)*dx + dpdy(v)*dy`, with the
offset coming from this same table -- but that is exact only for a
noperspective varying, and these tests check values rather than plausibility,
so it is not obviously worth having.

## A correction: the centroid fix was not a fix

Earlier in this work I recorded that WebGPU's interpolation-*sampling*
mismatch had been fixed by emitting `SpvDecorationCentroid` and
`SpvDecorationSample` on the vertex output as well as the fragment input. It
had not been. Fifteen of those validation errors were still happening, and I
had not looked -- what I measured then were Tint *SPIR-V rejections*, which
are a different failure from a WebGPU *validation* error, and the two do not
appear in the same place in the log.

**Those decorations are defined for fragment inputs.** A pre-fragment output
carrying them is not valid SPIR-V, Tint drops them, and the variant the driver
built to differ from the base module came out byte-identical to it. There is
no SPIR-V spelling for "this vertex output is interpolated per sample", so
through a SPIR-V intermediate the two stages *cannot* be made to agree that
way.

`GEX_WGPU_DUMP_VS` now dumps both stages, and it shows the whole thing at
once:

    @fragment fn main(@location(0) @interpolate(perspective, sample) ...)
    struct main_out { @builtin(position) ..., @location(0) v_base_1 : f32, }

The fragment side has the attribute; the vertex side never does.

### What it does instead

The fragment side gives the qualifier up: `centroid` and `sample` are stripped
from every fragment input, and the varying is interpolated at the pixel
centre. Centroid and centre differ only where a pixel is partially covered,
which GL already leaves to the implementation. `sample` is a real difference
and is reported once by name.

That is not a compromise between two working options -- the alternative was
losing the draw, and everything batched with it. `shader_multisample_-
interpolation` went **10/109 to 40/109**: thirty cases were being thrown away
by a pipeline that never got created.

GLSL helps here in a way worth knowing: it permits `centroid` on the fragment
input alone -- that asymmetry is exactly what WebGPU objects to -- but
*requires* `sample` on both sides. So stripping keeps a sample-qualified pair
consistent rather than making it disagree.

The remaining route to honouring `sample` is patching the WGSL text after
Tint, which is worth doing only if something actually needs it.

`interpolateAtSample(v, gl_SampleID)` was implemented against this and then
removed. It reduces exactly to a read of a `sample`-qualified varying -- which
is the thing that does not exist here, so the read would silently have been
the centre value. It stays refused by name.

New probe `gl44-centroid-varying-probe`, gated in the browser: a `centroid in`
varying draws at all and carries its value, and a `sample` pair does not choke.
Each case writes a constant marker beside the value, because a draw that never
happened and a value that came back zero are indistinguishable otherwise --
and while this was being written, both were true at different moments.

## The device loss, narrowed from 121 cases to 22

The app sweep re-ran against the current driver and came back **identical** to
the one before the decline work -- 285 RAN, 22 EXIT, 16 OK, 11 CRASH, 2
REFUSED, the two refusals still logic-op -- and SuperTuxKart, Minecraft 26.2
and Blender all render at their recorded baselines. So the latch changes cost
no application anything, which is what that sweep is for.

The hard recovery is the one thing still open, and it is now much better
understood. Two knobs went in for it, both inert unless set:
`GEX_WGPU_NO_PIPELINE_CACHE` builds every pipeline fresh, and
`GEX_WGPU_NO_BLITTER` declines a blit that would have gone through
u_blitter's quad.

**Eliminated by experiment:**

  * not the pipeline cache -- the loss survives building every pipeline fresh;
  * not u_blitter's quad draw -- the loss survives declining the blit;
  * `KHR-GL44.framebuffer_blit.scissor_blit` is **necessary**: the same 21
    following cases keep the device without it and lose it with it;
  * and not sufficient: that case alone, forty times over, or followed only by
    the case that reports the loss, does not reproduce.

**Eliminated by reading:** resource destruction releases its textures and
buffers; texture views, samplers, query sets and compute pipelines are each
created and released in balance; the bind-group-layout cache's deliberate
leak-when-full never fires, because its warning appears in none of the logs;
and the pipeline key does contain the module and layout pointers, but Dawn
refcounts those, so the address-reuse hazard that cache guards against cannot
bite there.

The reproducer is `sed -n '866,887p' /tmp/mp-filtered.txt` -- 22 cases, ending
at `texture_size_promotion.functional`, which is where the loss is reported
rather than where it is caused.

One thing worth saying about *when* this became reachable: before the refusal
latch was fixed, the context was dead long before this point, so the suite
never got here. This is newly exposed rather than newly broken.

### A trap in how I was waiting for it

Several background waits of the form `while pgrep -f gl-app-sweep; do ...`
never exited, because `pgrep -f` matches the *waiting shell's own command
line*. It cost an hour of apparently-stalled sweeps. Match on something the
waiter does not contain, or use the task machinery.

## Logic ops: the last application-level refusal, now served

The 341-program sweep has exactly two refusals left and both are the same
cause: `glLogicOp`. `line-xor` and `tri-logicop-xor` are the programs; the
idiom behind them is the XOR rubber-band rectangle that 2D and CAD software
has drawn selections with since the 1990s. Nothing else in the sweep is
refused by this driver at all.

WebGPU has no logic-op blend state, so it needs the destination in the
shader, and the pieces are all already here:

  * `gex_wgpu_feedback_snapshot` already copies a colour attachment and hands
    back a view, per draw, precisely so a shader can read what it is also
    writing -- WebGPU forbids the direct feedback loop. Logic ops want the
    same copy for a different reason.
  * The fragment variant machinery already specialises on framebuffer state.
  * `gex_wgpu_declined` already exists for the shapes this will not serve.

The shape of the work:

  1. Accept `logicop_enable` in `gex_wgpu_create_blend_state` instead of
     refusing it -- it already copies the whole `pipe_blend_state`.
  2. `PIPE_LOGICOP_COPY` is the default and means "write normally"; handle it
     as no work at all. `PIPE_LOGICOP_NOOP` means "leave the target alone",
     which is a zero colour write mask and needs no shader.
  3. Everything else needs the destination. Snapshot colour target 0, bind it
     at a texture binding of its own, and have the fragment variant read it
     with `texelFetch(snapshot, ivec2(gl_FragCoord.xy), 0)` -- a fetch, not a
     sample, so it needs no sampler.
  4. GL's logic ops act on the framebuffer's *integer* representation, so for
     an 8-bit unorm target the shader converts both sides with
     `u32(round(x * 255.0))`, applies the operation, and divides back. Other
     formats have other conversions: **refuse them by name** rather than
     approximate, which is what this driver does everywhere else.
  5. Both of `gex_wgpu_fragment_module`'s "does this need a variant?" gates
     have to learn about it, or the draw disappears silently -- see the
     gl_SamplePosition section for what that looks like.

Two things to know before starting. GL disables blending when a logic op is
enabled, so the two never have to combine. And overlapping primitives *within
one draw* will read a stale snapshot, because the copy is taken once per draw
-- which is correct enough for a rubber-band rectangle and wrong for anything
that overdraws itself, so that limit belongs in a comment and probably in a
refusal.

A probe should check the ops that are not self-inverse (`XOR` twice restores
the original, which a driver doing nothing at all also passes), so: `XOR` once
against a known destination, `INVERT`, `AND` and `OR` against a destination
with bits the source does not have, and `NOOP` leaving the target untouched.

### It was built that way, and one thing the plan did not predict

All of the above holds, and `gl44-logicop-probe` checks exactly those cases
against a destination that is neither black nor white, so "did nothing" and
"wrote the source" are both visible. It matches llvmpipe 5/5, and `line-xor`
and `tri-logicop-xor` now run with no refusal -- the sweep has no driver
refusals left.

What the plan missed: **the snapshot has to be taken before the render pass
begins.** Taking it where the bind group is filled reads naturally, because
that is where the view is needed, and it crashes inside Dawn's
`BeginRenderPass` -- the copy wants a command encoder of its own, and by then
one is already open. The feedback path takes its own snapshot much earlier,
during binding validation, which is where this one goes too. The backtrace
says `DeviceBase::GetScopedLock` and names neither the copy nor the reason;
it took a core dump to find.

## GL 4.5: ARB_cull_distance, and why the hard part was not hard

`gl_CullDistance` was withheld for several passes with a comment explaining
why: turning the cap on grants 4.5 immediately and silently eats geometry,
because Mesa's linker merges the cull values into the `gl_ClipDistance` array
and the emitter hands that whole array to WebGPU's `clip_distances` -- so the
cull values *clip*. A clip distance cuts a primitive where it reaches zero; a
cull distance discards the primitive only when **every** vertex is negative
and otherwise leaves it whole, including the parts where the distance is
negative. Mapping one onto the other looks right for the two easy cases and
eats a corner of the third.

That note said the split had to happen structurally, and that what made it
more than a morning's work was where: the same array is what
`nir_lower_clip_vs` writes for every user clip plane, a path real applications
here depend on.

**It does not have to be split.** A clip distance of +1 never clips anything,
so storing +1 into the cull components at the end of the rasterising shader
neutralises them however the shader wrote them -- no understanding of the
writes, and the user-clip-plane path untouched. `gex_wgpu_neutralise_cull_-
distances` is fifty lines.

The culling itself reuses the facing cull's machinery: the vertex shader
already runs as compute there, so it runs here, and the primitives every
vertex of which is negative in one distance are dropped from an index list the
draw is reissued with. It declines rather than guesses, which draws too much
rather than too little -- the safer direction for something that only removes
geometry.

One thing did have to give. `gex_wgpu_emit_varying_stores` refused any output
that was not a vector or scalar, and `gl_ClipDistance` is an **array** -- so
the capture failed with "no compute variant for this layout" and the cull
never ran. Four elements now go into a capture slot. That is the whole reason
the first attempt culled nothing while the neutralisation worked: the probe
showed `mixed_kept` passing and `all_negative_culled` failing, which is the
exact inverse of the old failure and was the clue.

With the extension in place, GL 4.5's rung wanted only GLSL 4.50, so the
feature level goes to 450.

    GL 4.4: ** COMPLETE **
    GL 4.5: ** COMPLETE **
    GL 4.6: ARB_gl_spirv ARB_indirect_parameters ARB_polygon_offset_clamp
            ARB_shader_group_vote ARB_spirv_extensions
            ARB_transform_feedback_overflow_query GLSL460

`gl44-cull-distance-probe` is 3/3 against llvmpipe and gated in the browser,
and it is no longer one of the native suite's differences: **88 OK with two
left, and both are cases where llvmpipe is the outlier.** SuperTuxKart,
Blender and Minecraft 26.2 are unchanged at 4.5.

## Four of GL 4.6's six, and why the last two are not this driver's

With 4.5 granted the ladder listed six for 4.6. Four are now in, and the two
left are limits of the platform rather than of the driver.

**ARB_transform_feedback_overflow_query.** Both capture passes already worked
the answer out: GL stops recording on a *whole* primitive, so the
primitives-written counter has to report what fit rather than what was
produced, and the predicate is that comparison. `gl44-xfb-overflow-probe`
checks a capture that fits, one that does not, and that the counter agrees
with the predicate in the overflowing case -- the third being what catches a
predicate bolted on beside a counter that disagrees with it.

**ARB_indirect_parameters.** `util_draw_indirect` already maps the count
buffer, and this driver resolves every indirect draw through it. The existing
`gl44-indirect-probe` gained the two `Count` variants, with three commands in
the buffer and a count of two -- so a driver ignoring the count draws the
third, whose vertices miss the sampled pixel.

**ARB_gl_spirv and ARB_spirv_extensions.** Mesa does the whole thing;
`glSpecializeShader` runs `spirv_to_nir` and what arrives is the NIR this
driver already takes. An extension whose implementation is "nothing" is one
nobody has checked, so `gl44-spirv-probe` embeds a real module built with
`glslangValidator -G` and checks the pixel, over a red clear so a program that
links but draws nothing fails.

### The two that are left

**ARB_shader_group_vote** has every link of its chain but one. Mesa gives the
three ballots as NIR vote intrinsics, zink's emitter has
`SpvOpGroupNonUniformAny/All/AllEqual` for exactly those, WGSL has
`subgroupAny` and `subgroupAll`, and this adapter grants `Subgroups` -- which
is now requested with the other optional features, so the day it works nothing
else has to change. Tint's SPIR-V **reader** does not know the instruction:

    Tint rejected SPIR-V: error: unhandled instruction with opcode 335:
    %27 = OpGroupNonUniformAny %24 %26 %25

The shader is refused and the program fails, which is worse than not having
the extension, so the cap stays off. `gl44-group-vote-probe` measures it and
skips cleanly meanwhile.

**ARB_polygon_offset_clamp** was measured earlier: the clamp reaches Dawn's
descriptor intact and has no effect on it.

### A note on probes and entry points

Three of these probes had to resolve their entry points through
`eglGetProcAddress` -- `glMultiDrawArraysIndirectCount`, `glSpecializeShader`,
`glShaderBinary`. Mesa's non-glvnd libGL exports none of them, and a direct
call fails at *load* with "undefined symbol", killing the probe before it
prints anything. The harness warns about this ("UNLINKABLE needs via
getProcAddress") and it is worth reading.

The SPIR-V probe is deliberately **not** gated in the browser. The guest GL
library exports neither `glShaderBinary` nor `glSpecializeShader`, and -- this
is the good part -- it filters its own extension list against its exports, so
the browser reports `gl_spirv=0` and never claims what it cannot do. The
native path advertises and implements it; the browser advertises neither. Both
are honest, and they differ, so a gate comparing the two would only ever fail.

## The device loss, down to three cases — and it is our path

    KHR-GL44.framebuffer_blit.scissor_blit
    KHR-GL44.texture_storage.compressed_data
    KHR-GL44.texture_size_promotion.functional

In that order, in one process. All three are needed: no pair reproduces it.
It is deterministic -- twenty-one cases never lose the device and the same
twenty-one behind `scissor_blit` always do, twice each way.

**radeonsi runs the same three cases clean.** That is the fact worth having:
this is not the tests, the hardware, or Mesa's front end. It is our stream
through Dawn.

What the hang is, from `RADV_DEBUG=hang`: **zero active waves**, an **empty
vm_fault log**, and the command processor stopped on a `WAIT_REG_MEM` after a
`RELEASE_MEM` -- a GPU-side fence wait that never completes. Not a runaway
shader, not a bad address: something was waited for that nothing signalled.
The failure surfaces later, at `gex_wgpu_texture_map` reporting `[Device] is
lost`, which is where it is *noticed* rather than where it is caused.

### Eliminated, each by experiment

| hypothesis | how |
| --- | --- |
| the pipeline cache | survives `GEX_WGPU_NO_PIPELINE_CACHE` |
| u_blitter's quad draw | survives `GEX_WGPU_NO_BLITTER` |
| NGG | survives `RADV_DEBUG=nongg` |
| command-buffer batching | survives `GEX_WGPU_MAX_BATCH=1` |

And by reading rather than running: resource destruction releases its textures
and buffers; views, samplers, query sets and compute pipelines are each
created and released in balance; the bind-group-layout cache's deliberate
leak-when-full never fires, because its warning is in none of the logs; and
the pipeline key holds module and layout pointers but Dawn refcounts them, so
the address-reuse hazard that cache guards against cannot bite there.

### Three more facts

**Block compression is required.** With `GEX_WGPU_NO_BC` withholding
`TextureCompressionBC` the reproducer keeps the device, twice; with it, the
device is lost, twice. Weaker evidence than it looks, and worth saying so:
without BC the compressed test errors out early and therefore also does
*less*, so this may be about the work rather than about the format.

**Order does not matter.** All three orderings lose the device. It is a set,
not a sequence -- not "one leaves state the next trips on", which is what the
first bisection made it look like.

**A standalone approximation does not reproduce it.** A program doing a
scissored multisample blit, a BC1 texture with data, and a dozen
single-format render targets cleared and read back survives. So the trigger is
more particular than the shape of the three tests. That is a negative result
worth having: the obvious next move is to hand-write a small reproducer, and
it does not work.

### Where to go next

Not more bisection -- three cases is where that ends. The reproducer is now
small enough to run under Dawn's own instrumentation, and the question to put
to it is which fence the command processor is waiting on and what was supposed
to signal it. `GEX_WGPU_FALLBACK_ADAPTER` is in place for the other half of
the question -- whether a CPU implementation executes the same stream happily
-- and needs a Dawn built with SwiftShader, which this tree's is not.

One thing to keep in mind while reading the reports: after a recovery the
*next* run's report says "This context is innocent", because it inherited a
GPU that was already unwell. Only a run on a settled GPU says "guilty", and
only that one is evidence.

## Tessellation had no transform feedback, and counted its primitives twice

`KHR-GL44.tessellation_shader` sat at 31/140 and was the largest failing
family left. 76 of the 109 failures were one sub-family,
`tessellation_control_to_tessellation_evaluation`, and 72 of those failed on
one of two messages -- "Invalid gl_Position data exposed in TE stage" or
"Invalid gl_PointSize data exposed in TE stage". That reads like a builtin
problem, and it is not: it is 91/140 now, from two defects that had nothing to
do with builtins.

**The evaluation stage recorded nothing.** With tessellation bound the
evaluation shader is the last stage before rasterization, so its output is
what GL captures -- and the tessellated draw had no capture path at all. It
also had no refusal for one, so a draw with feedback active rasterized
normally and wrote *nothing* to the bound buffers. Every case in that
sub-family reads its result back through feedback, so all of them read zeroes
and blamed whichever value they happened to check first. `gl_Position` is
simply the first thing that function checks.

The fix is a third capture pass beside the vertex stage's and the geometry
stage's, and it is the simplest of the three: a geometry shader decides at run
time how many primitives each invocation emits, so the geometry capture needs
a prefix sum before it knows where anything goes, whereas the tessellator has
already produced a fixed mesh -- every primitive in the index buffer is
emitted, in order -- so a primitive's destination is its own position in that
buffer and there is nothing to scan. It reads the evaluation pass's own output
buffer through the tessellator's index buffer, which already names the right
vertices in the right order, so winding is applied before this pass sees it
and it never learns that domains exist.

**`GL_PRIMITIVES_GENERATED` counted the same draw twice**, and this is the
half worth remembering. The tessellated draw ends by re-entering `draw_vbo`
with the tessellation stages unbound to rasterize the finished mesh. The
geometry stage has a guard against exactly this -- its comment says the count
was reported three times over -- and tessellation needed the same one: the
patch draw itself must not count, because the tessellator has not run yet, and
neither must the rasterizing pass.

Almost no application reads that query. It mattered because **the CTS sizes
its feedback buffer from it**: it measures the expected vertex count by
running a counter program and asking this query, so a count of twice the truth
produced a buffer of twice the size, the capture filled half of it, and the
tail stayed zero. The tests collect the *unique* values they read and require
there to be exactly one; they saw two. Both halves of the value were correct
and the log said so -- "set gl_Position to (1984, 2016, 2048, 2080) instead of
expected value (1984, 2016, 2048, 2080)" -- which is what a length check
failing behind an equality check looks like, and it is worth recognising: an
identical got/expected pair in a failure message means the count is wrong, not
the value.

`tests/integration/programs/gl44-tess-builtin-probe.c` covers both. Its
`every_vertex` case exists because the sum it captures is patch-constant, so
a capture that records the first primitive and stops satisfies any check that
reads vertex 0 alone -- which is how a partial capture survives a probe that
looks only at the front. Its `primitives_generated` case asserts that
`GL_PRIMITIVES_GENERATED` and `GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN` agree
when the buffer is oversized, which is false by a factor of two under the
double count.

The 13 that remain are a different shape: three are `barrier()` in a control
shader, four more are link failures, and the rest are single cases about
`gl_PatchVerticesIn`, `gl_InvocationID` and level values.

### Two more, and one of them explains a whole class of false leads

**`barrier()` sat inside an unreachable guard.** Both tessellation stages run
as compute, and both got a tail guard -- "if this invocation is past the work,
return" -- because a dispatch rounded up to whole workgroups has invocations
with nothing to do. The evaluation shader needs it. The control shader does
not: its dispatch is one workgroup per patch with `vertices_out` invocations
in each, so the guard's condition is unreachable by construction.

It was not merely redundant. An early return becomes a conditional around
everything after it, and a `barrier()` inside a conditional is not in uniform
control flow, so Tint rejected the whole module --

    'storageBarrier' must only be called from uniform control flow

-- and a control shader that shares data between invocations, which is the
only reason to write one that barriers at all, failed to compile. Skipping the
guard for the control shader fixed two of the three
`tessellation_shader_tc_barriers` cases.

**An evaluation shader can stand alone.** GL lets you bind a vertex and an
evaluation shader with no control shader: the levels come from
`glPatchParameterfv`, `gl_in[]` is the vertex shader's output directly, and
`gl_PatchVerticesIn` is the draw's `GL_PATCH_VERTICES`. This driver declined
that combination by name and drew nothing, and `set_tess_state` was not
implemented at all, so the default levels never reached it.

The shader side needed no change, which is worth knowing before touching it:
the per-vertex stride the evaluation module reads is already a dispatch
parameter, so setting that parameter to the patch size instead of the control
shader's output size is the whole of it. The draw path skips pass two, builds
the level records on the CPU from the default levels -- which also skips the
readback stall, since nothing on the GPU computed them -- and points the
evaluation pass at the vertex capture buffer instead of the control shader's
output. The variant cache already keyed on the producer's layouts, so it only
had to stop insisting the producer was a control shader.

**The false leads are the lesson.** Four of the remaining failures reported a
captured value of 0 where they wanted 2.7, or 4, or 3 -- an unmistakable
"wrong builtin" signature. A builtin probe reproduced none of it: the levels,
`gl_PatchVerticesIn` and `gl_TessLevelOuter` all read correctly. They were
`TESSELLATION_TEST_TYPE_TES` iterations, and the draw never ran. **A zero in a
feedback buffer means "nobody wrote here" exactly as readily as it means "the
wrong value was written."** Before chasing a zero as a value bug, check that
the draw happened -- a primitive-count query, or a marker component the shader
writes unconditionally, distinguishes them in one run.

`KHR-GL44.tessellation_shader` is 132/140 now, from 31. The eight that remain:
three need a geometry shader *after* tessellation (a real missing feature --
Mesa strips an evaluation output the geometry stage does not read, so those
shaders arrive empty and are refused for writing nothing), one needs an
array-typed per-patch output, and four are individual value cases.

### Where 128 components are honest and where they are not

`MAX_TESS_CONTROL_OUTPUT_COMPONENTS` was 64 where GL 4.0 requires 128, and
`MAX_TESS_PATCH_COMPONENTS` is st/mesa's own 120 while the layouts held
sixteen vec4 slots -- so a shader built to the advertised limit overflowed and
was refused. The layouts hold 32 slots now.

The interesting part is which caps were *not* raised with them, because the
reasoning generalises to every stage limit this driver reports. A stage whose
outputs are rasterized reaches the fragment shader through the passthrough
vertex shader, so WebGPU bounds it **twice** -- once by the vertex-attribute
count and once by the inter-stage variable count, both sixteen. Advertising
more would move the failure from link time to pipeline creation, which is
strictly worse: a link error is a diagnosable thing an application can read,
and a rejected pipeline is a silently dropped draw. So only the control
shader's outputs and the evaluation shader's inputs are raised -- the one
interface in this driver that is a buffer at both ends.

And these limits have to agree with **each other**, not only with the
hardware. Raising the control shader's *inputs* to match made the CTS skip the
case outright -- "GL_MAX_VERTEX_OUTPUT_COMPONENTS <
GL_MAX_TESS_CONTROL_INPUT_COMPONENTS" -- because a vertex shader's outputs may
be rasterized and so stay at sixteen. A skip reads as green on a pass-rate
summary and is not a pass; watch for one after any limit change.

### The three that need a geometry shader after tessellation

This is the one real feature left in the family, and it is worth writing down
what it would take, because the shape is already there. The geometry pass
reads its input as "slot k holds the k-th vertex of the k/N-th primitive",
built by a CPU slot table plus the vertex capture. After tessellation the same
buffer can be produced by a small gather: the evaluation pass has already
written one record per generated vertex, and the tessellator's index buffer
already names them in primitive order, so record k is evaluation vertex
`indices[k]`. Then the geometry pass runs unchanged with `prims =
index_count / per_prim`.

It was left undone deliberately rather than missed. The payoff is three CTS
cases and no known application -- tessellation and a geometry shader together
is rare -- and the cost is a bypass threaded through `gex_wgpu_gs_draw`, which
is long, deeply parameterised on the draw, and currently serving real
applications at 117/136. That is the wrong trade to make without a reason to,
so the reason should be an application that needs it.

Note also that Mesa strips an evaluation-shader output the geometry stage does
not read, so those shaders arrive with no outputs at all and are refused for
writing nothing. That refusal is accurate about what it sees; the cause is
downstream of it.

## textureGather, and four bugs that were not about gather

`KHR-GL44.texture_gather` went 29/75 to 68/75. Only the first of the four
defects was a gather problem; the rest were general and the suite happened to
be the thing that reached them.

**A dynamic texture offset cannot reach WGSL.** Tint's SPIR-V reader takes the
`ConstOffset` image operand and refuses the dynamic `Offset` one, and WGSL's
offset parameter must be a const-expression, so there is nothing to translate
it to. GL 4.0 allows a non-constant offset for `textureGatherOffset`, so this
is a real feature rather than a bad shader, and it was every `offset-gather`
case including the float ones. `nir_lower_tex`'s `lower_offset_filter` folds
such an offset into the coordinate instead. A *constant* offset is
deliberately left alone: `ConstOffset` is exact, while the lowering divides by
the texture size and lands on a texel boundary, which is precisely where a
gather's footprint is ambiguous.

**An integer gather has to be decomposed.** WGSL has `textureGather` for
`texture_2d<i32>`, but Tint's *reader* rejects `OpImageGather` on an integer
image outright -- "sampled image must have float component type". A gather is
four unfiltered texel reads, so it becomes four nearest samples at the four
texel centres, and this driver already lowers a nearest integer sample into a
fetch with GL's wrap rule, edge clamp, border colour and view swizzle applied.
Two details cost a rebuild each: the footprint is `floor(P * size - 0.5)`, and
GL's rotation is `(i0,j1), (i1,j1), (i1,j0), (i0,j0)` -- component 3 is the
lower-left texel, which is not the order anyone guesses. And the four samples
must carry an **explicit LOD**: an implicit-LOD sample is illegal outside a
fragment shader, and SPIR-V validation reports that against the *entry point*
rather than the instruction --

    OpEntryPoint Entry Point's callgraph contains function %main, which
    cannot be used with the current execution model

-- so an implicit decomposition passes a fragment-shader probe and fails every
case in a suite that samples in the vertex stage. A gather reads the base
level anyway.

**A partial depth upload wrote zeroes over its own region.** This is the one
worth remembering, because it has nothing to do with gather and affects any
application that builds a depth texture incrementally. WebGPU takes no buffer
copy into the depth aspect of `depth32float`, so depth is uploaded by
rendering: the floats go into an r32float staging texture and a fullscreen
draw moves them in through `@builtin(frag_depth)`, with the viewport limiting
the draw to the box. The fragment shader indexed the staging texture by
`@builtin(position)` -- the *framebuffer* coordinate -- while the staging
texture holds only the box, so every upload away from the origin read out of
range. WGSL's `textureLoad` answers that with a zero rather than an error, so
the draw wrote a depth of zero over exactly the region it was filling.
`glTexImage2D` with data was correct and `glTexSubImage2D` was not, and the
damage was confined to the piece a test then reads.
`gl44-depth-upload-probe` covers it, and its `neighbours` case is there so
that a "fix" which uploads the whole level, or clears before drawing, fails
too.

**Three bind-group declarations were wrong in ways that write nothing.** A
compute shader declared every texture as a `Float` sample type; a compute
shader comparing a depth texture needs `Depth` and a `Comparison` sampler; and
the vertex stage hardcoded every sampler to non-filtering, which is true of
filtering and false of comparison -- a comparison is a binding type of its
own. Each of the three is rejected by Dawn, and **a rejected bind group means
the work runs and writes nothing**, so all three read back as zeroes: an
integer texelFetch that returned whatever the buffer held, and two depth
comparisons that looked like they had answered false everywhere.

### Two process notes from this

**A fixup that derives one table from another must run after everything that
writes the second.** The compute sampler-type fixup reads the texture sample
types; it was placed before the scans that decide them and so read zeroes.
That is a shape to watch for, not a typo.

**Both of the "reads as zero" traps here were only diagnosable because
something distinguished "never written" from "written wrong."** The compute
probe fills its buffer with `0xbeef` rather than zero for exactly that reason,
and it is what turned the sample-type bug from a mystery into a one-line read.
See also the tessellation section above, where four failures looked like wrong
builtins and were a draw that never ran.

The 7 that remain are 5 integer *cube* gathers and the two
`gather-geometry-shader` / `gather-tesselation-shader` cases.

The cube ones are **not** the gather decomposition's fault, which is worth
writing down because it looks like it should be. WebGPU has no integer cube
sample type, so integer cube sampling is lowered to a 2D-array load over an
alternate view of the same six-layer allocation, and the result is SPIR-V the
validator rejects --

    Result Type has 2 components, but 3 expected

-- which fails the whole program. Disabling the integer gather pass entirely
leaves the error exactly where it was, so it is in that older lowering and
predates this work; an integer cube *sample* is presumably as broken as the
gather. Start by disassembling the rejected module: the driver prints it as
hex, and `xxd -r -p > m.spv && spirv-dis m.spv` is what the comment in
`gex_wgpu_tint.cpp` is there for.

Also raised in passing: every texture-lowering refusal now names its
condition, and the vertex variant path prints the compile error it used to
free unread. Both of those were needed to find the above at all -- the failing
shader never reaches the dump in `gex_wgpu_compile_nir`, so "could not build a
vertex shader variant" plus a list of four possible causes was all the log
said.

## An integer cube map, and the image format wall behind EEVEE

`KHR-GL44.texture_gather` finished at 73/75 (from 29 three passes ago). The
five remaining cube cases were three separate disagreements about one fact:
WebGPU has no integer cube sample type, so an integer cube is declared as a
2D array and reached through an alternate view of the same six-layer
allocation. The **shader**, the **bind group layout** and the **bound view**
all have to say so, and only the fragment stage did.

The first was invalid SPIR-V, which fails the whole program rather than one
draw. `dxil_nir_lower_int_cubemaps` rewrites the variable's type to a 2D
array, but its gather path derives a size query from the original cube op --
`nir_get_texture_size` copies the op's shape -- so it asked for two
components where an arrayed image gives three. Widening it is safe for every
reader in that pass. d3d12, the pass's other consumer, lowers the query
further and never saw the mismatch.

The second was the vertex and compute stages binding `view->view` whatever
their layout declared. That is now one helper all three stages call, so they
cannot diverge again; the fragment stage's open-coded version is gone. The
third was compute declaring the texture as `Cube` while its variant module ran
the same cubemap lowering and came out as a 2D array.

**A note on the A/B I got wrong.** I first concluded the invalid SPIR-V
predated this work, on the grounds that disabling the integer gather pass left
the same validator error in the log. It did not: the driver prints its
rejected module **once per process**, and the CTS builds many programs, so
both runs were showing the same *other* shader's error. The disassembly said
otherwise -- four `OpImageFetch` and one size query is a gather decomposition,
not anything else. **When a diagnostic is once-per-process, an A/B on its
presence proves nothing**; compare the disassembly, or count.

### Colour targets a shader does not write

WebGPU requires a colour target with no corresponding fragment output to be
masked out, and rejects the pipeline otherwise -- which invalidates the whole
command buffer, so every batched draw is lost with it, not only the offending
one. A fragment shader that writes only to an image or a storage buffer is an
ordinary GL shader that writes no colour at all; so is one drawn for its side
effects alone. The attachment-less framebuffer had a special case for exactly
this reasoning and the general one was missing.

The mask comes from the shader's declared outputs, and two details matter:
`gl_FragColor` broadcasts to every target, and a dual-source second output
sits at `DATA0` with index 1 -- that is not target 1 and must not be counted
as one.

### What actually blocks Blender's EEVEE viewport

Worth stating plainly because it is the largest remaining capability gap and
it has a named application behind it. Blender's *rendered* viewport fails to
link six shaders and crashes, and the driver's reason is:

    shader rejected: gex_wgpu image 'out_scattering_img' has format
    r11g11b10_float, which WebGPU cannot take as a storage texture

`gex_wgpu_storage_texture_format` matches WebGPU's core storage list exactly
and completely -- there is no missing entry to add. WebGPU offers storage
textures only for the four-channel 8/16/32-bit formats and the one- and
two-channel 32-bit ones. GL 4.2 *requires* about twenty more: `rg16f`, `r16f`,
`r8ui`, `rgb10_a2`, `r11f_g11f_b10f` and the rest of the 1- and 2-channel
sizes. Thirteen of the twenty-four real failures in
`KHR-GL44.shader_image_load_store` are this one thing, and so is EEVEE.

Two routes exist and both are real work rather than a fix:

- **Pack into a supported format.** Every missing format is 1, 2 or 4 bytes,
  and `r32uint` is a supported 4-byte storage format, so a 4-byte format can
  be stored packed and unpacked in the shader. The obstacle is that WebGPU's
  `viewFormats` spans only the sRGB pair, so a texture allocated as
  `rg11b10ufloat` cannot be *viewed* as `r32uint` -- the texture would have to
  be allocated as `r32uint` from the start, and then every other use of it
  (sampling, rendering, copying) needs the same pack/unpack. This is the same
  wall as [[blender-format-reinterpretation]]. The 1- and 2-byte formats are
  worse: there is no 1-byte storage format at all, so they would need several
  texels per word and a changed addressing scheme.
- **Lower read-only images to textures.** `nir_lower_readonly_images_to_tex`
  already exists in Mesa and needs no storage format at all -- a `readonly`
  image of any format becomes an ordinary `texelFetch`. It is used by d3d12
  and CLC, where images and textures share a descriptor space; here they are
  separate binding tables fed by `set_shader_images`, so the driver would need
  to bind an image view as a texture binding. That is a bounded change and it
  would cover every `readonly` image regardless of format -- a good first
  step, and it is where I would start.

Smaller things fixed on the way: an image unit with nothing bound now binds a
1x1 stand-in, which is what GL says such a unit reads (zero, with stores
discarded) rather than the dead context it used to produce; and a view WebGPU
refuses is declined by name, with the message saying what was asked for.

`KHR-GL44.texture_cube_map_array` is 0/2 with and without the cube size-query
change, so those failures are older than this work; I have not looked into
them.

## Shared memory was indexed by byte where ntv wants words

`KHR-GL44.compute_shader` 24/42 to 28/42 and `shader_storage_buffer_object`
67/124 to 71/124, from one division. It is the most consequential single line
found in a while, because shared memory is what makes a compute shader worth
writing -- every reduction, prefix sum, tile filter and sorting network is
built on it -- and it was silently producing wrong numbers.

ntv's shared and scratch blocks are `array<uintN, size/(N/8)>` in a wrapper
struct, and `emit_load_special` / `emit_store_special` use the intrinsic's
offset **directly** as the access-chain index. NIR's `load_shared` and
`store_shared` offsets are in *bytes*. zink divides them in
`rewrite_bo_access_instr`; this driver reuses ntv without zink's pipeline
around it, and never did.

**The two halves of ntv disagree about the units**, which is the part to
remember: `emit_shared_atomic_intrinsic` divides by the element size *itself*,
so atomics must not be scaled or they divide twice. Each caller has to match
the emitter it reaches.

Nothing refused. A byte offset used as a word index addresses four times too
far, and WGSL clamps an out-of-range constant-size array index rather than
trapping -- so `shared uint lane[64]` kept only the sixteen entries whose
scaled index still landed inside it, and read back numbers that look like
arithmetic rather than corruption: a sum of 0..63 came out as **3144** instead
of 2016, and the same sum through a shared struct as **120**, which is exactly
sum(0..15). That last number is what identified it: a clean prefix means an
addressing stride, not corruption.

Scratch travels the same ntv path, which is where the storage-buffer cases
come from -- a spilled array was equally wrong, in any stage.

**`GEX_WGPU_DUMP_WGSL=<n>` now prints the first n modules Tint produces**, and
it is what found this: the SPIR-V is dumped only when Tint *rejects* a module,
so one that translates cleanly and computes the wrong thing left nothing to
read. The bug was a single `<< 2` in the WGSL, visible at a glance. Add it to
the list of first moves for a "compiles but wrong" shader.

### A memory barrier is not a control barrier

The driver turned a pure `memoryBarrier*()` into a control barrier because
Tint's SPIR-V reader has no `OpMemoryBarrier` and WGSL has no standalone
memory barrier. The justification was that a control barrier is *stronger* --
true of ordering, and false of the one requirement it adds: **it must be
reached in uniform control flow**. GLSL allows `memoryBarrier*()` anywhere,
and `compute_shader.shared-struct` puts one in each arm of a branch on
`gl_LocalInvocationID`, with a comment saying the calls are unnecessary and
are there to check the compiler accepts them. Substituting produced a module
Tint rejects, losing the whole shader.

Such a barrier is now dropped instead, which is a real weakening and worth
being explicit about. What makes it defensible: a standalone memory barrier
orders nothing observable on its own -- without a control barrier there is no
execution ordering for the fence to be relative to -- and a shader that does
pair one with `barrier()` has that barrier in uniform control flow, where it
carries the memory semantics itself. "Uniform" is judged conservatively as
"not nested in any if or loop", which errs towards dropping a fence rather
than emitting a module that cannot be read.

### Also this pass

**A read-only image needs no storage format.** WebGPU offers storage textures
for far fewer formats than GL 4.2 requires, but a read-only image is bound as
a *sampled* texture -- Tint lowers a SPIR-V storage image that is only read to
a plain `texture_2d<T>` -- so it needs only a sampleable format, and that
table is far richer. This does **not** reach Blender's EEVEE viewport, which
was the hope: its images are all written, so they still need the packing route
described in the section above.

**Buffer textures work in a compute shader.** A `samplerBuffer` becomes a
read-only storage buffer whose texel word count comes from the bound view, so
such a shader has no base module and needs the per-dispatch variant the
compute path gained for integer sampling. It also turned up a limit that read
as a capacity problem and was not one: everything about a buffer texture is
indexed by its sampler *unit*, so `GEX_WGPU_MAX_BUFFER_TEXTURES` bounded which
units could hold one rather than how many there could be, and a shader with a
few ordinary textures plus one `samplerBuffer` was refused with
"slot=6 count=0 max=4". It is one per sampler unit now, with a static_assert
saying so.

`compute_shader`'s remaining 14: three are `fp64` (WebGPU has no f64),
`resource-texture` wants a multisampled *array* texture (WebGPU has no form of
one), `shared-indexing` trips Tint's uniformity analysis on a `barrier()`
reached through a function containing early returns, and the rest are
unexamined.

## A hole in the block numbering, and an edit thrown away

Four fixes; `shader_storage_buffer_object` 71/124 to 77/124,
`shader_atomic_counters` 22/30 to 26/30, `compute_shader` 28/42 to 29/42.

**st/mesa leaves accesses whose block is gone.** It lowers an access through
an interface block into `load_ssbo` / `store_ssbo` carrying a block index, and
that leaves no deref -- so NIR removes the storage variable, and the driver's
lowering pass cannot name the slot the access asks for. The `length()` path
already had to fill that hole. It is now one get-or-create helper for every
caller, and routing the **atomic** path through it too was worth three cases
on its own: an atomic counter reaches the driver as an ssbo atomic with the
same missing block, and its refusal read word for word like the load path's --
"a 32-bit atomic on storage block 1, which this shader does not declare" --
which is what made it obvious once the load path was fixed. A recreated block
is writable only for a store, because a store to a readonly block is not
expressible in GLSL and getting it wrong the other way would make a vertex
shader that only reads a buffer declare a `read_write` storage variable, which
WGSL forbids in that stage.

**A dynamic block index unrolls.** `buffer B {...} blocks[4]` reached as
`blocks[i]` is GL 4.3 and this suite uses it. WGSL has no array of storage
buffers: every binding is its own variable and none can be selected at run
time -- but the set of blocks is known when the shader is compiled, so the
choice can be unrolled. Loads read every block and select afterwards rather
than branching, because reading a block this invocation did not choose is
harmless (an unbound one has a placeholder behind it) and it keeps the pass
free of control flow. Stores cannot do that -- writing every block would
corrupt the ones not chosen -- so they branch, and the pass now declares
`nir_metadata_none`.

**An edit through a read-write map was discarded**, and this is data loss
rather than a missing feature. A map that *reads* a GPU-written buffer cannot
be served from the shadow copy, because the shadow does not know what the GPU
wrote -- so it is served from a private block read back off the device, and
the caller's edits land there and nowhere else. Unmap then uploaded from the
*shadow*, which still held the copy taken when the map was made, before the
caller touched anything. **Both branches matched a read-write map and the
wrong one won.** `glMapBufferRange(GL_READ_BIT | GL_WRITE_BIT)` over a buffer
a shader has written is how an application reads back a counter, adjusts it
and carries on. `gl44-map-readwrite-probe` covers it, verified by reverting
the fix (4/4 to 2/4); its `neighbours_kept` case exists so a "fix" that
uploads the whole buffer from a stale shadow fails too.

**A `memoryBarrier` outside compute is dropped rather than emitted.** WGSL has
no barrier of any kind in those stages -- not even the control barrier the
compute path substitutes -- so one there was left as `OpMemoryBarrier`, which
Tint cannot read, losing the whole shader. A fragment shader with a storage
buffer and a `memoryBarrier()` is an ordinary thing to write. No CTS case
turns on it by itself, so it is in for the refusal it removes rather than for
a score.

### What is left in these families, and why

`shader_storage_buffer_object`'s remaining 47: **28 are a vertex shader
writing a storage buffer**, which WebGPU forbids outright -- confirmed by
reading the shaders rather than inferring it, since several are named for
`readonly` buffers and it is the *output* buffer that blocks them. Two are
Tint declining to take the address of an atomic through a pointer. The rest
are unexamined.

`shader_atomic_counters`' remaining 4: one is the same vertex-stage
restriction; three fail on values with nothing in any log, and
`advanced-usage-draw-update-draw` was one of them until the map fix, so the
others may be a different mechanism rather than the same one twice. Batching
is ruled out for them -- `GEX_WGPU_MAX_BATCH=1` changes nothing.

## The app sweep could not tell a driver problem from a broken machine

This pass went looking at applications rather than the CTS, and the first
thing it found was that the instrument was untrustworthy.

The sweep reported **193 of 337 programs "exited"** where the notes recorded
22, which reads as a driver collapse. It was not one. `openbox` had died --
killing an application's process group takes the window manager with it and
leaves Xvfb alone -- and `ensure-display.sh` returned early the moment the
*display* answered, so the manager was never restarted. Without one, a GLUT
program that reads window properties dies with

    X Error of failed request: BadWindow (invalid Window parameter)
    Major opcode of failed request: 20 (X_GetProperty)

which is not a GL error and looks nothing like one. `bezier` failed
**identically on llvmpipe**, which is what settled it -- and running llvmpipe
was an afterthought rather than part of the harness. Forty-five minutes went
into that.

So the sweep now runs **every program twice**, once against gex_wgpu and once
against the system's llvmpipe, and reports a `differs` column. A verdict is
evidence only when the reference disagrees with it; everything else is
context. `ensure-display.sh` checks the window manager separately from the
display, through EWMH rather than by process name (`pgrep -f openbox` also
matches the script). Both are now tracked in the repo -- `ensure-display.sh`
was not tracked at all, so the fix would have evaporated with the scratchpad.

**With the reference in place, 6 of 337 programs differ**, and only two are
about this driver:

    muffin                gex=CRASH  ref=RAN   signal 11
    texture_from_pixmap   gex=CRASH  ref=RAN   signal 11

Three more (`dotewm`, `qs`, `quickshell`) are the harness failing to execute
them at all -- the same LD_LIBRARY_PATH problem as
[[suite-buildfail-false-alarm]] -- and `eglinfo` works here and exits on
llvmpipe.

### One missing hook, two crashes

Both segfaulted at address zero. Gallium leaves
`pipe_screen::resource_get_handle` optional, and Mesa's own drisw frontend
calls it with no NULL check:

    get_image_shm (drisw.c:136)
      res->screen->resource_get_handle(res->screen, NULL, res, &whandle, ...)

so a driver that does not set it crashes instead of falling back. It is set
now and returns false -- which is the answer, not a placeholder: a WebGPU
resource has no shared-memory identifier to hand out, and the caller's next
line is `get_image(...)`, which copies the drawable through the CPU. Both
programs run to the timeout now, which is llvmpipe's verdict for them.

Worth generalising: **an optional Gallium hook may still be called
unconditionally**, so "we don't implement that" and "we set it to say no" are
different states, and only the second is safe. This one was reachable by any
GLX pixmap path.

### The cheap route to Blender's EEVEE viewport is ruled out

The previous handoff recorded a plausible shortcut for the storage-format wall:
a float format can be held in a wider float format losing nothing -- half
float has a 10-bit mantissa where `r11f_g11f_b10f` has six -- so the driver
could allocate `rgba16float` and the shader would never know, *provided nothing
else looks at the texture*. `GEX_WGPU_IMGFMT_LOG=1` now reports every texture
in an unsupported format with the binds st/mesa asked for, and EEVEE's answer
is:

    gex-imgfmt: r11g11b10_float binds=0xa SAMPLED RT 1x1
    gex-imgfmt: r16_float       binds=0xa SAMPLED RT 1x1

**Sampled and rendered to.** So a substitution would have to be undone again
for every sample and every attachment: the sampled view would need the missing
components restored to 0 and 1, the attachment format would change under the
application, and `glTexImage`/`glGetTexImage` would need pack and unpack
routines that exist for only a few formats today. The knob stays so the next
person with the same idea can check in one run instead of reasoning about it.

`gl44-image-format-probe` is committed as a marker for the gap and **is
expected to fail 4 of its 6 cases**, like `gl44-group-vote-probe`. One of its
results is already useful for building the substitution: `r32f_channels`
passes, so a genuinely one-channel storage format returns `(x, 0, 0, 1)`
without the driver doing anything -- WGSL specifies it -- whereas a
four-channel allocation would return whatever the store left in the other
components. That is the part the probe exists to keep honest.

## The unset-hook audit is finished, and it found one bug

Following the note from the `resource_get_handle` fix, I enumerated every
function-pointer member of `pipe_screen` (76) and `pipe_context` (147), diffed
them against every assignment the driver makes, and grepped Mesa for each
unset hook's call sites: 63 screen and 59 context hooks are unset, reached from
110 call sites across 47 distinct hooks.

**`resource_get_handle` was the only reachable one.** Everything else is either
guarded by a pattern static analysis cannot see, or gated behind something the
driver does not advertise. The cases worth naming, because they *look* reachable
and are not:

- `set_hw_atomic_buffers` (`st_atom_atomicbuf.c:188`) -- an atom, so it runs on
  ordinary state changes, but `st_bind_hw_atomic_buffers` returns early when
  `max_hw_atomic_counters == 0`, which this driver sets explicitly.
- `set_inlinable_constants` (`st_atom_constbuf.c:174`) -- also an atom, guarded
  by `prog->info.num_inlinable_uniforms`, which only becomes non-zero if the
  driver calls `nir_find_inlinable_uniforms`. This one does not.
- `invalidate_resource`, `emit_string_marker`, `get_device_reset_status`,
  `generate_mipmap`, `clear_buffer` -- all behind **core** GL entry points
  (`glInvalidateBufferData`, `glDebugMessageInsert`, `glGenerateMipmap`,
  `glClearBufferData`, ...), so no extension gates them. I exercised all of them
  from one program on both drivers rather than trusting a regex: no crash, no
  error, identical behaviour. st/mesa checks each before calling.
- `set_sample_locations`, `evaluate_depth_buffer`, the memory-object and
  semaphore family, the bindless handles, `create_ms_state`/`create_ts_state`
  and the perf-query hooks -- gated behind extensions an extension query
  confirms are absent.

The method mattered more than the result. Scanning back a fixed number of lines
for a guard produced false positives twice -- guards sit at function entry, or
behind a forward `goto`, or in the *caller* -- so each candidate had to be read
at function scope and, where it was cheap, actually run. And the negative result
is the useful part: it means "we left this NULL" is now a checked claim rather
than an assumption, for both structures.

Note also what *not* to do with the list: st/mesa uses "the hook is set" as a
capability signal in places (`if (screen->resource_from_memobj)` is what gates
`EXT_memory_object`), so blanket-stubbing the unset hooks would advertise
features the driver does not have. Only a hook that is genuinely unguarded and
reachable should be set, and it must answer "no" correctly.

## Shader images in formats WebGPU has no storage texture for

WebGPU offers storage textures for the four-channel 8/16/32-bit formats and
the one- and two-channel 32-bit ones. GL 4.2 requires eighteen more --
r11f_g11f_b10f, r16f, rg16f, r8ui, rgb10_a2 and every 16-bit normalised format
among them -- and this driver used to refuse a shader that wrote one. A
refusal is worse here than a wrong pixel: it arrives as a *link* error, the
application gets a null program, and Blender's EEVEE dumped core on it.

Such an image is now an ordinary **storage buffer**. The shader packs the
texel itself and the driver copies the buffer into the texture when the pass
ends. Reading the design in one line: a buffer has no format, which is the
whole point.

The parts worth knowing before touching it:

- **`nir_format_pack_rgba` does the work.** One Mesa helper converts a vec4
  into any plain format's packed words -- unorm, snorm, half float, integer,
  sRGB -- from the format's own `util_format_description`. It also handles
  r11f_g11f_b10f and r9g9b9e5, which are *not* plain layouts. Filtering on
  `UTIL_FORMAT_LAYOUT_PLAIN` alone therefore excluded exactly the format
  Blender needed, and the first version of this path shipped with EEVEE still
  refused because of it. Match the classifier to what the packer supports, not
  to a layout enum.

- **The copy goes in `gex_wgpu_end_pass`.** A texture copy cannot be recorded
  inside a render pass, and that function is the one place every pass closes
  through. The context carries the copies the pass owes.

- **Rows are padded to 256 bytes**, because that is what a texture copy
  demands of `bytesPerRow`. The shader is told the padded stride in a header
  rather than deriving one, so the module never depends on the texture bound
  to it.

- **The header needs its own buffer *and* its own type.** Both were measured.
  A read-only binding over the same buffer as a writable one is refused --
  "includes writable usage and another usage in the same synchronization
  scope" -- which is a stricter rule than the one about two writable bindings
  aliasing. And a *sub-word* format packs several texels into one word, so
  neighbouring invocations write different halves and the bits go in with
  atomics; Tint retypes an atomically-accessed array as `array<atomic<u32>>`,
  and SPIR-V gives every runtime array of uint a single type, so making the
  data block atomic made the header block atomic too. The header gets a
  fixed-size array.

- **Two atomics may not share a deref.** ntv emits one `OpAccessChain` per
  deref and reuses it; Tint cannot take the address of a pointer it has
  already retyped as atomic. A sub-word store is exactly two atomics on one
  word -- clear the field, set the field -- so it failed every time while
  either atomic *alone* compiled. That asymmetry is what identified it, since
  the message names neither the image nor the atomics.
  `gex_wgpu_split_atomic_derefs` gives each atomic its own, and it must run
  after `nir_rematerialize_derefs_in_use_blocks_impl` (which is per *block*,
  so two atomics in one block still share) and after every optimisation, or
  CSE merges the copies straight back.

The method note, because it recurred three times in one sitting: **dump the
SPIR-V.** Each of the three findings above came from `spirv-dis` on the
rejected module, and none of them was guessable from Tint's message -- the
shared runtime-array type was visible only as two access chains naming the
same pointer type. The driver prints the module as `SPIRVHEX` lines; it is a
plain byte stream, not 8-hex-digit words, and decoding it as words produces a
file the disassembler rejects for its own reasons.

### The compute stage takes the read case too

A dispatch has nowhere that must be broken open to copy the texture *into* the
buffer, so `gex_wgpu_image_is_packed` takes an `allow_read` argument: compute
passes true, a graphics stage passes false. For a read image the format need
not be missing from WebGPU's storage list at all -- rgba16f and rgba32f *are*
storage textures, what they lack is `read_write` access, which WebGPU allows
only for r32ui/r32i/r32f, and a buffer has no such rule.

Two more things had to be measured there:

- **A 16-bit float channel cannot be unpacked through NIR at all.** Every
  route ends in a 16-bit type and Tint's reader has none ("unhandled integer
  width: 16"). `nir_format_unpack_rgba` writes `f2f32(u2u16(word))`; asking
  for `nir_unpack_half_2x16` instead only moves it, because
  `nir_lower_alu_width` expands that into `nir_unpack_32_2x16` plus `f2f32` --
  the same 16-bit vector spelled differently. `gex_wgpu_half_bits_to_float`
  does it in 32-bit integer arithmetic instead, exactly, including subnormals
  and NaN payloads. The *store* side never hits this, which is why an r16f
  image could be written and not read back, and why it only surfaced once
  compute took the read case.
- **The bindings collided with the compute textures.**
  `GEX_WGPU_DRAW_PARAMS_BINDING` and `GEX_WGPU_COMPUTE_TEXTURE_FIRST_BINDING`
  are the same number -- the graphics stages and a dispatch reuse it for
  different things -- so starting one past the draw parameters put every
  packed image inside the range a dispatch numbers its textures in. Dawn
  reported an invalid bind group and nothing more, because a collision looks
  exactly like a layout that does not match its entries.

### What is still refused, and why

- **An image with no format qualifier** (`format none`). GL allows a
  `writeonly` image to omit it and takes the format from the bound texture,
  which makes the packing arithmetic per-*draw* state rather than per-shader:
  it needs a variant keyed on the bound format, like integer sampling and
  buffer textures already have. Three CTS cases.
- **Atomics on a packed image**, refused by name. An atomic on the *packed
  word* is not the same operation once several texels share one, and Tint has
  no SPIR-V image-texel-pointer support to fall back on either -- `unknown
  SPIR-V storage class: 11` is that, and it is a separate ceiling.
- **Partial writes.** The whole buffer is copied out, so a texel the shader
  did not write takes whatever the buffer held. Because the buffer is cached
  on the resource and rewritten after every pass, that is the texture's own
  content in the common case -- but not if something else wrote the texture in
  between. The compute read path seeds the buffer from the texture and does
  not have this gap.

### The fragment read case, and why the reasoning against it was wrong

A draw *can* serve an image it reads. I had ruled it out because seeding the
buffer has to submit -- the texture's own writes must reach the queue before
they are copied -- and a submit closes the open render pass. What that misses
is that the bind group is built **before** this draw's pass is opened, so the
submit ends whatever pass came *before*, which is what any state change does.
The cost is a pass break per draw that binds one, and Blender's EEVEE went
from one image refusal to none.

Two ordering bugs came with it, and both are the kind that only a value test
finds:

- **Seed before registering the copy out, not after.** Registering first meant
  the seed's submit flushed that copy immediately, so an unwritten buffer
  landed on the texture before the draw ran *and* the copy the draw needed was
  gone. The seed now happens inside `gex_wgpu_packed_image_bind`, before
  registration, so both stages share one ordering.
- **Re-read the encoder after building the bind groups.** The draw captures
  `ctx->pending_encoder` long before it builds them, and the submit releases
  it; beginning a render pass on the freed one segfaults inside Dawn's
  encoding context with this driver nowhere in the backtrace above
  `BeginRenderPass`. Nothing is lost by refreshing it -- the old encoder was
  submitted, not discarded.

### A refusal was hiding a driver bug, again

Lifting the format refusals made a *promoted* texture reachable for the first
time, and it was allocated wrong: `gex_wgpu_resource_create` decided
`StorageBinding` from the Gallium format while creating the texture in the
wider stored one, so rgba16_unorm-as-rgba32float never got the usage its own
image binding needs. That promotion is far older than this work. Nothing had
reached it because the shaders died earlier, on a format with no storage
texture at all. Two CTS cases came back from that one line.

`gl44-packed-image-probe` is the test. It checks values against llvmpipe
rather than checking that the shader linked, which is the distinction that
matters: the whole point of packing over promotion is exactness, and a
link-success test would not notice a packer that rounded.

## Image shapes WGSL has no storage texture for

WGSL has exactly four storage-texture types -- `texture_storage_1d`, `_2d`,
`_2d_array`, `_3d`. GL has more, and each extra one was its own refusal until
now. All of them are served by rewriting the *declaration*
(`gex_wgpu_lower_image_shape`) so that the shader, the bind group layout and
the view all say the same thing, and every rewrite is **exact** -- which is the
only reason to make one, per [[crashes-hide-behind-close-enough]]:

| GL declares | served as | why it is exact |
| --- | --- | --- |
| `image2DRect` | 2D | a rectangle texture differs only in being *sampled* with unnormalised coordinates; image access is by texel either way |
| `imageCube` | 2D array | the coordinate is already `ivec3(x, y, face)`, and GL's face order +X,-X,+Y,-Y,+Z,-Z is the order the six array layers are in |
| `image1D` | 2D | every non-3D texture here is created as `WGPUTextureDimension_2D` |
| `image1DArray` | 2D array | as above, rows one texel tall |

`image1D` is the one worth pausing on, because it was **not** refused -- it was
silently wrong. The driver asked for a 1D texture view of a 2D texture, which
is an invalid object, so the bind group was rejected and the draw wrote nothing
with no GL error to show for it. The sampled path had always viewed these as
2D; only the image path had not. This is the shape of defect
[[a-draw-can-vanish-silently]] describes, and a probe that checks a *named
texel* is what catches it -- a probe that only checked "the shader linked"
would have passed throughout.

Two compiler details that cost a rebuild each:

- **A NIR image coordinate is declared as `vec4`** whatever the dimension
  actually uses (`image("store", src_comp=[4, 1, 0, 1], ...)`). So inserting a
  row index moves the layer from `.y` to `.z` rather than growing the vector.
  Building a `vec3` is a validation failure -- `src->ssa->num_components ==
  num_components` -- which aborts inside the driver with the whole shader
  printed and no indication of which source was wrong.
- **`imageSize` is where a rewritten shape shows through.** On a 1D array the
  query now answers `(width, 1, layers)` where the shader asked for `(width,
  layers)`, so the first and last components are taken and the height dropped.
  Without the array no fixup is needed, because the first component of a 2D
  query is the width either way. Getting this wrong returns the height as the
  layer count, which is a plausible number and not an error.

### What the allTargets cases still stop on

`KHR-GL44.shader_image_load_store`'s `allTargets` cases walk every target in
one test, so each refusal hid the next -- the rectangle came off first, the
cube was immediately behind it, then the 1D array. Their score is unchanged
because they need every target at once, and two remain:

Both are now served, and what stops those cases is no longer a shape:
`basic-allTargets-store` reports "the number of storage textures (7) exceeds"
the device limit, which is the platform rather than the driver.

- **`imageBuffer`** turned out to be the *cheapest* shape rather than the
  hardest. WebGPU has no texel buffer, so there is no storage texture to
  compare a format against -- but there is also nothing to copy: the resource
  *is* a buffer, so the shader addresses it in place, with no staging and no
  copies at all. Read-only counts too, and unlike a read-only texture image it
  has no sampled fallback to take instead.

  Two bugs on the way there produced **no message of any kind**, which is
  worth remembering because both are easy to write again:

  - The incomplete-texture check asks whether the resource has a *texture*,
    and a buffer resource has none, so every buffer image took the
    discard-to-scratch path. The draw ran, the module was right, the stores
    went nowhere. `gex_wgpu_image_is_bound` asks the right question per
    target.
  - A read map only consults the device for a buffer it knows was written
    there, and the bind flags say `SHADER_IMAGE` rather than `SHADER_BUFFER`
    -- *and* the batch has to be told it touched the resource, or the read
    finds no conflict, skips the flush, and copies to its staging before the
    draw that writes it is submitted. See "the batch must track every
    resource".

  A probe seeded with **zeroes could not have found either**: a store that
  never happened and a store that wrote zero read back identically. Seeding
  with a pattern is what separated them.

- **A 1D array upload.** Gallium's convention puts the layer in `box->y` and
  `st_TexSubImage` converts to it -- but not on every path, and a two-layer 1D
  array arrives as a plain 3D box, `16x1x2`. That was declined for having
  depth 2 on a target whose depth must be 1: an upload dropped for good, on a
  target nothing else in the suite exercises. Both readings are accepted now
  rather than one being declared correct; they differ only when an axis
  carries something to disambiguate on.

### Small floats cannot be unpacked through NIR at all

`nir_format_unpack_rgba` writes a half channel as `f2f32(u2u16(word))`, and
`nir_format_unpack_11f11f10f` writes the identical pattern -- so **both**
formats Blender's EEVEE cares about hit `unhandled integer width: 16` from
Tint, which has no 16-bit integer type. Asking for `nir_unpack_half_2x16`
instead only moves it: `nir_lower_alu_width` expands that into
`nir_unpack_32_2x16` plus `f2f32`, the same 16-bit vector spelled differently,
and the driver already asks for that lowering.

`gex_wgpu_small_float_to_float` does it in 32-bit integer arithmetic. Every one
of these formats has a 5-bit exponent biased by 15, so one function serves
f16, the 11-bit pair and the 10-bit one, differing only in mantissa width and
whether there is a sign: bias 15 to 127 is +112, the mantissa moves to bit 23,
all-ones stays all-ones so infinities and NaN payloads survive, and a
subnormal is `mantissa * 2^-(14 + mantissa_bits)`, exact in f32.

The asymmetry is the thing to recognise: **the pack side never hits this**, so
such an image can be written and not read back. If a half or packed-float
format works one way only, this is why.

## Blender's EEVEE now compiles every shader

Four things stood between EEVEE and a link, none of them about image formats,
and all four are fixed:

1. **Dual-source blending.** The device has the feature and the SRC1 blend
   factors were already allowed once granted, so the blend state was right and
   the *shader* was rejected: `'@blend_src' and '@location' are used on one
   member while another member with '@location' doesn't use '@blend_src'`.
   ntv omits the Index decoration on index 0 because Tint turns one into
   `@blend_src(0)` and WGSL allows that only on a pair -- but omitting it on a
   shader that *does* have an index 1 output is the mirror of the same rule.
   The flag is conditional now, decided from the output variables (this runs
   after passes that rewrite the outputs, so `info.fs.color_is_dual_source` is
   only as fresh as the last `nir_shader_gather_info`).
2. **Image atomics.** `imageAtomic*` reaches SPIR-V as `OpImageTexelPointer`,
   whose result lives in storage class Image, and Tint's reader has none. So
   an atomic decides the **binding**: the image goes through the packed path,
   where a texel that is a whole 32-bit word makes it an ordinary buffer
   atomic. Only at 4 bytes -- below that several texels share a word and an
   atomic on the shared word would carry its neighbours -- which costs nothing,
   since GL defines image atomics for r32i and r32ui only.
3. **`maxStorageTexturesPerShaderStage`.** WebGPU defaults to four where GL
   4.2 asks for eight image uniforms, and Dawn's own message says "this
   adapter supports a higher maximum". Nobody had asked. Same shape as the
   compute-invocation limit already handled in `gex_wgpu_native.c`; when a
   limit looks like a device restriction, check the request first.
4. **A depth view read by a plain sampler in a *dispatch*.** The draw path
   works this out per draw and drops the filter rather than the draw; the
   compute path used the shader's declared type unchanged, so a dispatch
   sampling a depth buffer lost its bind group and wrote nothing.

### EEVEE's rendered viewport is blocked on the platform

Every WebGPU **validation error** is gone -- the run is clean -- and the
viewport still does not come up, for a reason the driver cannot fix:

    gex-limits: adapter colour bytes/sample 32 (asked 32)
    gex-wgpu: colour target 4 dropped: the attachments exceed 32 bytes per sample

EEVEE binds **five** colour attachments. WebGPU bounds their combined bytes
per sample, WebGPU's own arithmetic charges an rgba8 target eight bytes rather
than four, and this adapter reports 32 as its *maximum* -- so the driver asks
for 64, is given 32, and four targets is all there is. GL allows eight draw
buffers and a fragment shader writing five cannot be split across passes, so
there is nothing to trade. Use `GEX_WGPU_LIMIT_TRACE` before treating any
limit as a wall: it prints the adapter's own maximum beside the granted one,
which is the difference between this and the three limits that turned out to
be nobody having asked.

Blender itself is fine. Its **default viewport matches llvmpipe exactly**
(1280x1024 mean 61, 336 colours against 338), which is the app-facing claim;
it is the rendered/EEVEE shading mode that needs the fifth attachment.

Two driver-side refusals remain in that path, and neither would bring the
viewport up on its own:

- A compute shader calling **`workgroupBarrier` from non-uniform control
  flow**. Read the SPIR-V before spending anything on this: **the shader is
  invalid GLSL and WebGPU is the only implementation that checks.**

  The module has five control barriers. Four are in uniform flow -- after a
  selection merge, which is the ordinary `if (...) { shared[i] = 0; }
  barrier();` shape. The fifth sits at the top of a loop body whose bound
  comes from

      %155 = (gl_WorkGroupID << 5) + gl_LocalInvocationID

  a per-invocation coordinate, so the trip count differs per invocation and
  the barrier is genuinely non-uniform. GLSL says `barrier()` must be in
  uniform control flow too; other drivers simply do not enforce it, so the
  shader works everywhere else. There is nothing to fix here -- hoisting the
  barrier out of the loop is not semantically equivalent, and dropping it is
  worse.
- One **declined draw**: a *read-only* image at a promoted format over a
  differently-formatted texture. A read-only image normally falls back to a
  sampled texture, whose format table is richer, but that needs the formats to
  agree and the promotion breaks it. This is the bind-time case only a
  bound-format-keyed variant of the packed path can serve -- the same answer
  as for the writable version.

to hit them in.

## The pixel comparison, and a confound in it

`tests/gl-app-compare.sh` photographs each program under llvmpipe and under
this driver and ranks them by RMSE. It answers the question the app sweep
cannot: **a program that runs, draws every frame it was asked to, and produces
the wrong pixels is a `RAN` in the sweep and a silent defect.**

Before acting on any row, three things:

1. **The reference runs a different Mesa.** This driver loads the tree's
   Mesa (26.3-devel); the system llvmpipe is whatever is installed (26.1.6
   here). A difference can therefore be a *Mesa version* difference and
   nothing to do with the driver. The way to isolate is
   `GALLIUM_DRIVER=softpipe` with the tree's `LIBGL_DRIVERS_PATH` -- same
   Mesa, different driver. Do this first; it is one run and it decides
   whether the row is yours.
2. **`self` is not a complete animation floor.** It measures run-to-run
   variation of the *same* driver, so a program that is deterministic in
   *frame count* but whose output depends on how many frames elapsed shows
   `self = 0` and still differs between drivers purely by speed. `clearspd`
   is exactly this -- a clear-speed benchmark that changes colour per frame,
   flat-coloured in both images and 25/255 apart, with `self = 0`. A direct
   probe confirmed both drivers clear a window to the same value.
3. **Two references can disagree.** `drawarrays`, `draw2arrays`,
   `drawelements` and `drawrange` all differ by *exactly* 0.0201116 -- one
   shared cause, and 225 differing pixels all on a single row. gex and
   softpipe draw that row; llvmpipe and radeonsi do not. So the hardware says
   exclude it, but it is a one-row edge-inclusion difference on an edge that
   lands on a pixel boundary, and the mechanism is the y-flip: WebGPU has no
   negative viewport height, so the flip is done in the shader, which inverts
   which boundary edges a top-left fill rule includes. Worth knowing about for
   pixel-exact 2D work; not worth a sub-pixel position hack.

### `dlist-degenerate` renders black, and it is ours

The one row so far that survives all three checks.

- This driver renders an **entirely black frame** (1 colour, mean 0);
  llvmpipe renders content. `self = 0`, so it is not animation.
- **Softpipe on the same Mesa is clean**, so it is not a version difference.
- Exactly one GL error, `GL_INVALID_OPERATION in glEnd`, and it comes from
  *Mesa's own* `execute_list` (`dlist.c:12426`) rather than from the driver.
- **No draw reaches the driver at all** -- the per-shape vertex-input trace
  prints nothing, and the only driver output for the whole run is one depth
  clear.
- At the error: the framebuffer is complete (`GL_FRAMEBUFFER_COMPLETE`),
  `ValidPrimMask` is full (`0x3fff`) so mode validation is not the cause,
  `CurrentExecPrimitive` is 15 (outside begin/end), and `DrawGLError` is
  already `GL_INVALID_OPERATION`.

An `LD_PRELOAD` interposer over `glBegin`/`glEnd`/`glNewList`/`glCallList`
settles what the program actually does, and it is not what the name suggests:

    glNewList(list=1)  glBegin(GL_POLYGON)  glVertex2f  glVertex2f  glEndList
    glNewList(list=2)  glVertex2f  glVertex2f  glEnd()              glEndList
    glCallList(1)  glCallList(2)

It splits **one begin/end pair across two display lists** -- a dangling
`glBegin` in the first, the matching `glEnd` in the second -- which GL permits
and which is the whole point of the test. Replaying them in order should give
one four-vertex polygon.

A minimal reproducer of just that (compile the two lists, call both, check
`glGetError`) gives `GL_INVALID_OPERATION` on this driver and `GL_NO_ERROR`
on softpipe **with the same Mesa**, so the divergence is real and isolated.

**But it probably is not ours to fix.** Extending that reproducer to try each
primitive mode in turn, with a whole-pair control list first, trips an
assertion inside Mesa on *every* driver:

    vbo_save_api.c:1166: upgrade_vertex: Assertion `save->copied.nr == 0' failed

So Mesa's own display-list vertex compiler barely supports this pattern, and
the gex/softpipe difference is far more likely to be `vbo_save` reaching a
different state than a defect in the driver's own code -- nothing in the
driver is even called. If this is picked up again, start in `vbo_save_api.c`
with the two drivers side by side, not in `gex_wgpu_screen.c`; and weigh it
against how rare the pattern is, since Mesa asserts on mild variations of
it.

Note also: `glGetError` *between* `glBegin` and `glEnd` is itself
`GL_INVALID_OPERATION`. The first version of the reproducer did that and
reported an error on every driver, which looked like agreement that something
was broken.

## The y flip puts a horizontal line one row off, and only that

The `glEdgeFlag` cluster in the pixel comparison -- `tri-edgeflag`,
`tri-edgeflag-array` (identical RMSE) and `tri-unfilled-edgeflag` -- resolves
to one precisely bounded difference, and it is worth recording exactly because
the symptom looks much bigger than the cause.

**The measurement.** A width-1 line at exact device coordinates in a 64x64
viewport, reported as GL rows (row 0 at the bottom):

| line | gex_wgpu | radeonsi | llvmpipe |
| --- | --- | --- | --- |
| horizontal at clip y=0 | row **32** | row 31 | row 31 |
| vertical at clip x=0 | col 31 | col 31 | col 31 |
| diagonal 1:1 | identical | identical | identical |
| diagonal 2:1 | identical | identical | identical |

A **filled** rectangle spanning device 16.0 to 48.0 covers rows 16..47 on all
three, identically. So this is not a global offset: geometry is placed
correctly, and only a *zero-height* primitive lying exactly on a pixel
boundary resolves the other way. The x axis is unaffected because it is not
flipped.

**The cause.** WebGPU has no negative viewport height, so the flip is done in
the shader (`gex_wgpu_lower_position_y`, a plain `y = -y`). Negating y maps a
GL coordinate on a pixel boundary onto a device coordinate on a pixel
boundary -- the same place -- but the rasteriser's tie-break then awards the
boundary to the pixel on the opposite side in GL terms.

**Why it is left alone.** Two fixes exist and both cost more than a pixel:

1. *A sub-pixel bias in the flip.* It cannot be a constant. An NDC epsilon
   moves device y by `epsilon * H / 2` pixels, so staying sub-pixel at
   H = 16384 needs `epsilon < 6e-5` while exceeding the rasteriser's ~1/256
   pixel quantum at H = 64 needs `epsilon > 1.2e-4`. The ranges do not
   overlap, so the bias has to scale with viewport height -- coupling the flip
   to viewport state and needing a uniform or a variant key.
2. *Flip at present instead of in the shader*, so geometry rasterises in GL
   orientation and the tie-break is right. Architecturally the better answer,
   and it costs a full-frame blit unless one is already happening.

Neither is worth a one-pixel difference on boundary-aligned horizontal lines
on its own. But if the flip is ever revisited for another reason, this is the
correctness argument for doing it at present time.

**What it explains.** Horizontal lines at integer device coordinates are
everywhere in 2D and wireframe drawing -- plot axes, table rules, borders,
axis-aligned mesh edges -- so this accounts for a family of small
deterministic differences in the comparison, all internally consistent and
none of them an artifact a user would call broken. `radeonsi` and `llvmpipe`
are **pixel-identical** on `tri-edgeflag` (RMSE 0.0), which is what made this
worth running down rather than dismissing as rasteriser noise.

## Flat shading an unfilled polygon gives every edge its own colour

The other cluster the comparison turned up, and unlike the y-flip this one is
a visible wrong answer rather than a pixel.

`poly-flat-unfilled-clip` -- `glShadeModel(GL_FLAT)` with
`glPolygonMode(GL_LINE)` -- renders with **five** colours here and **two** on
both references. The histograms say it plainly:

    reference   60404 blue (background)   2096 red
    gex_wgpu    60662 blue               557 red  508 magenta  389 cyan  384 yellow

GL flat-shades an unfilled polygon with **one** colour, its provoking
vertex's, across every edge. This driver gives each edge the colour of one of
*its own* two vertices, so a four-sided polygon comes out in four colours.
`trifan-flat-unfilled-clip` is the same, and it accounts for the `-flat`
family in the deterministic ranking (`poly-flat`, `quad-flat`, `trifan-flat`,
`tristrip-flat`, `line-flat`, `linestrip-flat-stipple`).

**Not the provoking-vertex rewrite.** That was the first hypothesis --
`u_primconvert` is told `flatshade_first` *and* the driver runs its own
provoking-last rewrite, so a double application looked likely. Running with
`GEX_WGPU_NO_PROVOKING=1` changes nothing: the four colours are identical to
the pixel. Rule it out before spending anything there.

**The mechanism.** `util_primconvert` rewrites *indices*. It can choose which
end of a generated line supplies the flat value -- which is what
`util_primconvert_save_flatshade_first` is for, and what the comment beside it
already describes -- but every generated line still references its own two
vertices. No index reordering can make four different edges share one colour;
that needs the provoking vertex's attributes **duplicated** into each
generated line, which is what Mesa's `draw` module unfilled stage does for the
software drivers and what the hardware does natively.

**It is not legacy-only, which is what settled whether to build it.** The
paragraph that used to close this section guessed that the audience was
fixed-function flat shading and that modern shaders would not hit it. That
guess was wrong, and a probe is what said so: `gl44-flat-unfilled-probe`'s
`shader_flat_edges` draws the same triangle with a `flat out` varying from a
real GLSL program and fails identically. Flat varyings plus a wireframe is
ordinary CAD and modelling drawing, not a compatibility-profile relic.

**The fix: redraw the edges from a capture.** `gex_wgpu_flat_unfilled_draw`
runs the vertex shader as compute into a capture buffer -- the same first pass
the wide-line and facing-cull rewrites use -- and redraws the edge list with
the geometry stage's own passthrough vertex shader.

What keeps it to one pass and one buffer is the **slot table at double
length**. The capture reads the vertex it runs for out of a table, so the
table is built with twice as many entries as there are edge vertices: the
first half is the edge endpoints, the second half is each edge's polygon's
provoking vertex, repeated. One capture buffer then holds both sets
contiguously, and it is bound *twice* as two vertex buffers a half apart. Only
the element table differs from the geometry stage's: a flat slot reads binding
one and every other slot reads binding zero. So each vertex draws at its own
position carrying the provoking vertex's flat varyings, the passthrough shader
needs no modification at all, and the cost is one extra vertex-shader
invocation per edge vertex.

`line_rewritten` is deliberately *not* set around that draw, so a wide, smooth
or stippled wireframe still picks up its own rewrite on the way through -- and
that rewrite blends the flat varying between two endpoint records which now
hold the same value, so it stays correct.

**Finding the provoking vertex.** For all six of `u_unfilled`'s topologies it
is the **last vertex of the emitted edge ring**, so no topology needs a case
of its own. The quad strip is the one worth checking rather than assuming: it
emits the ring `(v2,v0,v1,v3)`, whose last entry is the same `v3` that GL's own
quad-strip ordering ends on. Down the cull/edge-flag route it is taken from the
resolved source list instead, because edge flags drop edges and a polygon
cannot then be recovered by counting emitted ones.

**Two things had to change besides.**

*The provoking-vertex rewrite must be skipped for unfilled draws.* It ran
first, and it was doing two things wrong at once: it rotates each primitive so
GL's provoking vertex comes first, which moved the per-polygon provoking vertex
out of the position the edge list would report it at; and for a quad the
rotation goes through `u_index_translator`, which emits **triangles**, so the
wireframe gained the diagonal `glPolygonMode` is documented not to draw. Both
appeared the moment a flat quad was drawn unfilled. The unfilled path serves
the convention itself now, so skipping it there loses nothing.

*There are two routes to the edge list and both needed wiring.* The
`u_unfilled_translator` route, and `gex_wgpu_cull_polygons`, which emits the
final LINES list itself whenever culling or edge flags are in play. Fixing only
the first leaves every culled or edge-flagged wireframe still wrong, which is
what `culled_edges_agree` is in the probe to catch -- and it caught a second,
older bug behind it; see the next section.

All six probe cases now match llvmpipe, and the whole polygon-mode probe family
still passes.


## The facing cull was culling the wrong face into an FBO

Wiring the flat-unfilled replay into the culling route turned up a defect
older than either: with `GL_CULL_FACE` on, a flat unfilled triangle drew
**nothing at all**, and `GEX_WGPU_NO_FACING_CULL=1` brought it straight back.

The instrumentation says it in one line:

    gex-wgpu: facing p=0 area=2.5600 usable=1 front=0 culled=1 cull_face=2 front_ccw=0 flip=1

The area is positive -- the triangle is counter-clockwise, which is what it was
written to be -- and `front_ccw` is **zero**, so `(area > 0) == front_ccw` made
it back-facing and `cull_face=2` (`PIPE_FACE_BACK`) threw it away.

**The two sides of that comparison were in different spaces.**
`st_atom_rasterizer.c` flips `front_ccw` when the framebuffer is `Y_0_BOTTOM`
-- its own comment says "Drawing to an FBO. The viewport will be inverted." --
so `front_ccw` describes the space the *rasterizer* sees. The driver pairs that
with a y-flipped vertex module, chosen by `ctx->viewport_set &&
ctx->viewport.scale[1] > 0.0f`, and maps `front_ccw` to WebGPU's `frontFace`
directly on that basis. But `gex_wgpu_cull_polygons` decides facing from the
*capture* variant, which is compiled `flip_y = false`. So its area sign is the
opposite of the rasterizer's exactly when the flipped module is in use -- which
is every FBO.

The fix is to read `front_ccw` through the same flip:
`(area > 0) == (front_ccw != vertex_flip)`, using the predicate the draw state
already uses. Any future CPU-side geometry decision taken from a capture needs
the same correction: the capture is in Gallium clip space, not in the space the
rasterizer state describes.

**Why nothing caught it.** `polygon-mode-probe`'s cull case culls *both* faces,
so the sign cannot matter to it, and `polygon-mode-faces` passes with either
sign. It took a case that culls one face of a polygon whose winding is known
and asserts something is still drawn.

## An unfilled quad draws its decomposition diagonal

Found by the `load` column's own housekeeping rather than by looking for it.
With `gltestperf` retired, the cleanest remaining signal in the comparison was
`quad-unfilled`: **refdiff 0** -- llvmpipe and softpipe pixel-identical -- and
gex 0.0446 away from both. `quad-unfilled-clip` has the same rmse to seven
digits, so it is one defect, not two.

The differing pixels say what it is without any guessing. There are 224 of
them, in 224 distinct rows *and* 224 distinct columns, which is a line from
corner to corner; and gex has exactly 224 more lit pixels than the reference.
Reduced to 64x64 it draws like this, with `X` for the pixels only gex lights:

    .........##############################################.........
    .........#X...........................................#.........
    .........#.X..........................................#.........
    .........#..X.........................................#.........
     ... 40 more rows ...
    .........#...........................................X#.........
    .........##############################################.........

GL draws a quad's **four** edges. This driver does not advertise
`MESA_PRIM_QUADS`, so st splits every quad into two triangles before the draw
arrives, and by the time `glPolygonMode(GL_LINE)`'s rewrite runs the quad's
identity is gone: it emits six edges, and the two extra ones are the
decomposition diagonal drawn twice. `GEX_WGPU_UNFILLED_LOG=1` confirms it
plainly -- `unfilled mode=4 fill=1 count=6` for a four-vertex quad, mode 4
being `MESA_PRIM_TRIANGLES`.

It is not a flat-shading problem: it happens with `GL_SMOOTH` too, where
neither the provoking rewrite nor the flat-unfilled replay runs at all. Both
`quad_no_diagonal` and `smooth_quad_no_diagonal` in
`gl44-flat-unfilled-probe` fail on this, at 34 interior lit pixels against the
reference's 0, and they are **left failing on purpose** -- they document a real
defect rather than a regression.

**A warning about measuring it.** The first version of those two cases sampled
the two diagonals directly and reported *no* diagonal for a driver that was
drawing one: the diagonal runs along `x + y = 63` and the probe sampled
`x + y = 64`. Both cases now count lit pixels in a band strictly inside the
quad, which no real edge comes near and any spurious edge must cross. Count an
interior, do not sample a computed line.

**Resolved: it was a Mesa bug, not the cap.** See the next section. The cap
change is in, `quad-unfilled` and `quad-unfilled-clip` are down from 0.0446 to
6.8e-05 and 6.6e-05, and both quad-diagonal probe cases now pass. What follows
is the diagnosis as it stood before that, kept because the reasoning is what
led to the real cause.

**The obvious fix appeared to crash primitive restart.**
The driver's own `u_primconvert` is created with the narrow WebGPU-native mask
and the draw dispatch converts against that same mask, so advertising quads in
`caps->supported_prim_modes` *alone* looked free: the unfilled rewrite runs
first and would see the real quad, while a filled quad would still be split
exactly as before. It works, precisely -- the smooth unfilled quad becomes
**pixel-identical** to llvmpipe, 224 lit pixels down to 180.

But it segfaults `gl44-primrestart-probe` at its `triangle_fan` case, with the
vertex buffer reported unbound and an index past its end:

    vertex element reads past its buffer (element 0 reads to 104, buffer slot 0
    is 64 bytes) ... draw=6 indexed ... bounds=1/0..12 bias=0 start=7

Four variants all crash the same way: all of quads+quad-strip+polygon+fan;
dropping the fan (which is not needed anyway -- a fan's every triangle is
already its own polygon, so splitting one loses no edges); quads alone; and
quads with `supported_prim_modes_with_restart` widened to match, on the theory
that the two masks disagreeing moved which layer resolves restart. None of them
helped. The index bound of 12 against an eight-vertex buffer was a red
herring: it is a stale bound printed by a diagnostic, not the fault.

## The quad cap's segfault was a Mesa bug in u_index_prim_type_convert

The previous section reported the quad-diagonal fix blocked on a segfault in
`gl44-primrestart-probe` that four variants of the cap change could not get
past. That was the wrong conclusion, and one backtrace was enough to show it.

    #0  0x0000000000000000 in ?? ()
    #1  primconvert_init_draw (...) at u_primconvert.c:251
    #2  util_primconvert_draw_single_vbo (...)
    #4  u_vbuf_draw_vbo (...)
    #5  _mesa_validated_drawrangeelements (... mode=6, count=13 ...)

The crash is a jump to address zero **inside Mesa**, not in the driver, and
line 251 is `trans_func`, not the `direct_draw_func` on the line above it that
the earlier read had assumed. Inspecting the frame settles it in one look:

    $1 = (u_translate_func) 0x0                 <- trans_func
    $2 = translate_memcpy_uint                  <- direct_draw_func, fine
    $3 = MESA_PRIM_QUADS                        <- mode
    $6 = MESA_PRIM_TRIANGLE_FAN                 <- info->mode
    $4 = 0x40bb  primtypes_mask (with quads)
    $5 = 0x403b  restart_primtypes_mask

`u_index_prim_type_convert` answered **QUADS for an input of TRIANGLE_FAN**. It
lumps the whole triangle family in with the quad family:

    case MESA_PRIM_TRIANGLES:
    case MESA_PRIM_TRIANGLE_STRIP:
    case MESA_PRIM_TRIANGLE_FAN:
    case MESA_PRIM_QUADS:
    case MESA_PRIM_QUAD_STRIP:
       if ((hw_mask & (1<<MESA_PRIM_QUADS)) && pv_matches)
          return MESA_PRIM_QUADS;

and no fan-to-quads translator exists: `u_indices_gen.py` populates
`translate_quads[]` from `quads()` and `quadstrip()` alone, so
`translate_quads[..][MESA_PRIM_TRIANGLE_FAN]` is NULL and `u_primconvert`
calls it. Nor could one exist -- a fan is not expressible as quads.

**Why it has stood upstream.** Reaching that switch at all requires a driver
that advertises `MESA_PRIM_QUADS` but *not* `MESA_PRIM_TRIANGLE_FAN`. Every
driver that supports quads today supports fans too, so the
`(hw_mask & (1<<prim))` early-out above the switch returns first. This driver
advertises triangle strips and quads and not fans, which is the combination
that finds it.

The fix is to return `MESA_PRIM_TRIANGLES` for the triangle family and leave
the quad family's behaviour, `pv_matches` gate included, exactly as it was.
With it in, `primrestart` is back to 22 of 22 and the cap change is clean.

**The lesson worth keeping** is about the earlier pass, not the bug: four
rebuild-and-measure cycles went into varying the cap, on the assumption that a
crash appearing when a cap changed was caused by the cap. One backtrace, which
cost less than any of those cycles, named the real caller and the real NULL. A
segfault has a stack; read it before varying the input that exposed it.

## GL_POLYGON's provoking vertex is its first, not its last

Keeping a polygon intact immediately painted `poly-flat-unfilled-clip` **cyan**
where both references draw red, and the row went from 0.147 to 0.174 -- worse
than before the cap change.

GL's flatshading table gives every topology its primitive's *last* vertex and
makes exactly one exception: a single polygon takes vertex **one**. It is not
affected by `glProvokingVertex` either; the table names vertex 1 outright. The
driver had never had to know this, because until the cap change st split every
polygon before the driver saw it and Mesa's own split carried the right colour.

Both routes needed it -- `gex_wgpu_cull_polygons` and the translator hook in
`gex_wgpu_draw_unfilled` -- and `polygon_edges_agree` in
`gl44-flat-unfilled-probe` pins it, with three different colours behind v0 so
that taking any other vertex fails.

With it fixed the row is 0.133, better than the 0.147 it sat at before any of
this, and its histogram matches the reference exactly: two colours, red on
blue. The residual is coverage -- 1477 outline pixels against 2096 -- and it is
distributed across the whole frame rather than being one missing edge, on a row
whose two references disagree with each other by 0.0597. Clipped-wireframe
coverage is where the remaining `-clip` rows live (`poly-flat-unfilled-clip`
0.133, `tri-unfilled-userclip-stip` 0.109, `trifan-flat-unfilled-clip` 0.105);
none of them is a clean signal the way `quad-unfilled` was at refdiff 0.

## The clipped wireframe, and a second rewrite hiding behind it

The previous pass left the comparison's three `-clip` rows unattributed, with
the note that the differences were "distributed across the whole frame rather
than being one missing edge". That reasoning was wrong, and wrong in a way
worth recording: those programs draw *many* primitives, so a defect that costs
one edge per primitive is distributed too. Spatial spread does not distinguish
a global difference from a per-primitive one.

A single-primitive reproducer settled it immediately -- one unfilled quad, one
user clip plane -- and the difference was exactly one edge, 44 pixels, with
everything else matching to the pixel:

    .........#######################................................
    .........#.....................o................................
    .........#.....................o................................     o = only
     ... 40 more rows ...                                                 llvmpipe
    .........#.....................o................................      draws it
    .........#######################................................

**GL clips a polygon and then applies polygon mode.** The clipped polygon's
outline therefore includes the edge clipping introduced along the plane. This
driver applied polygon mode first and let the GPU clip the resulting lines,
which gets every original edge right and draws no boundary edge at all. Both
references agree exactly (llvmpipe and radeonsi pixel-identical, 134 lit
pixels against 90) and the unclipped control matched perfectly, so the
boundary edge was the whole of it.

**Frustum clipping is not the same question, and needed measuring rather than
assuming.** GL does *not* introduce a drawn edge where a polygon leaves the
view volume -- a quad extending past clip x = 1 gives 114 lit pixels on
llvmpipe and 114 on this driver, identical. Only user clip planes matter here.
Two of the three rows have no user clip plane at all, which is why the fix
moved one of them and not the others, and which is what pointed at the second
defect below.

**The fix clips on the CPU from the capture.** st has already run
nir_lower_clip_vs, so the vertex shader writes one gl_ClipDistance component
per enabled plane and the captured distances *encode* the planes:
Sutherland-Hodgman runs on those numbers with no geometry to re-derive and no
plane state to read. `gex_wgpu_cull_polygons` already takes that capture for
the facing cull, so the clip costs no extra pass.

A vertex of the clipped ring is a point along an original edge, which no index
list can name, so the outlines go through the replay the flat rewrite already
had -- with a third slot-table section and a shader that mixes two records by
a per-vertex factor. Mixing in clip space is what clipping does, so the
position lands where GL puts it. Two cases decline rather than approximate: a
vertex where one plane's boundary edge is cut by a second plane is a mix of
two already-mixed vertices, and edge flags are not combined with clipping.

**Then the two remaining rows turned out to be a different bug entirely.**
Their outlines had the right colours and about 70% of the reference's lit
pixels, and the images showed why at a glance -- the reference draws thick
solid edges, this driver thin broken ones. The apps call `glLineWidth(4)`.

The wide-line rewrite draws each segment as two triangles, and polygon mode
was **still GL_LINE** when that draw came back round to `gex_wgpu_draw_vbo`.
So the unfilled rewrite turned every quad into its own wireframe: a one-pixel
outline plus a sloped band per edge, which is each quad's outline and its
diagonal. 520 lit pixels against llvmpipe's 696.

The driver's own log named it, not the pixels: `GEX_WGPU_UNFILLED_LOG` printed
`unfilled mode=4 fill=1 count=6` immediately after the line rewrite reported
`instances=4` -- an unfilled draw of six triangle vertices that no application
had issued. `ctx->line_rewritten` already existed for exactly this and the
unfilled gate simply did not consult it.

It predates both the flat-unfilled and the clipping work: smooth shading,
which takes neither path, reproduces it identically. Worth remembering when a
rewrite's output re-enters the draw path -- **polygon mode has already been
applied by the time that draw exists.** `gs_rasterizing` is deliberately not
excluded with it, because GL *does* apply polygon mode to a geometry shader's
output primitives.

**Where the family stands now.** Every row of the unfilled and flat family is
inside the disagreement between the two references:

    tri-unfilled-smooth         0.078  -> 0.0031   (refdiff 0.0402)
    poly-flat-unfilled-clip     0.147  -> 0.0163   (refdiff 0.0597)
    trifan-flat-unfilled-clip   0.105  -> 0.0120   (refdiff 0.0487)
    tri-unfilled-userclip-stip  0.109  -> 0.0306   (refdiff 0.0316)
    quad-unfilled               0.045  -> 6.8e-05  (refdiff 0)
    quad-unfilled-clip          0.045  -> 6.6e-05  (refdiff 0.0058)

The only rows still above their refdiff are `tri-edgeflag`,
`tri-edgeflag-array` and `tri-unfilled-edgeflag` at ~0.029 against ~6e-05,
which are the y-flip horizontal-line tie-break documented earlier -- a
platform limitation, since WebGPU has no negative viewport height, and one a
constant epsilon provably cannot fix.

## The GL45 CTS was unrunnable, and what it found once it ran

The pixel comparison had nothing attributable left, so the next instrument was
the native CTS. It did not work at all -- every family died instantly:

    FATAL ERROR: GLX extension "GLX_ARB_create_context_robustness" not
    supported at tcuLnxX11GlxPlatform.cpp:315

That reads as an environment problem and is half one. **Two** things were
needed, and the distinction matters because fixing either alone changes
nothing:

1. The driver must set `caps.device_reset_status_query`. Mesa gates
   `GLX_ARB_create_context_robustness` on that cap alone
   (`__glXEnableDirectExtension` in `src/glx/glxext.c`), and this driver never
   set it. Real applications that ask for a robust context were refused the
   same way, so this is not only a test concern.
2. The X server must advertise it as well. Mesa only puts an extension in the
   string if `direct_support & (server_support | direct_glx_only)`, and
   robustness is not marked `direct_only`, so a *direct* context still needs
   server support it has no protocol for. Xvfb does not advertise it.
   `tests/gl-cts-native.sh` therefore force-enables it through Mesa's own
   `glx_extension_override` driconf, scoped with `DRIRC_CONFIGDIR` so no
   user's `~/.drirc` is touched.

The instrument that settled where the fault lay was a nine-line program
calling `glXQueryServerString` and `glXQueryExtensionsString` exactly as the
CTS does. `glxinfo` was actively misleading here: it prints an extension in
its *client* list that `glXQueryExtensionsString` does not return.

**A trap worth knowing:** Mesa's driconf parser silently ignores a `.conf`
file that contains an XML comment. The only symptom is the FATAL ERROR coming
back, which reads as the option not existing.

`KHR-GL45.limits` runs 135/140 with both in place. The list of cases is the
CTS build's own `khronos_mustpass/main/gl45-main.txt` -- 19714 of them; the
`khronos_mustpass_single` list is a different and much smaller set that
contains almost no families, which is a good way to waste an hour.

### What it found

A family's score is meaningless without the reference run. `copy_image` came
back 31/31; `shading_language_420pack` came back 47/329 and llvmpipe scores
**329/329**, which is what made it worth reading.

**gl_CullDistance stopped working past its fourth component.**
`KHR-GL45.cull_distance` scored **1 of 218** against llvmpipe's 202 with zero
failures. `GEX_WGPU_CULL_TRACE` said why in one line: every distance above the
third read back `k0.000` while the shader had written real values, and zero
does not cull.

gl_ClipDistance and gl_CullDistance share *one* array of up to eight
components across two varying slots, and a shader with more than four between
them declares a single variable at `CLIP_DIST0` -- there is no variable at
`CLIP_DIST1` to find. The capture matched slots by location alone, found
nothing for the upper slot and filled it with the zeros an unwritten input is
worth. 1/218 → 145/218.

Behind that, the geometry and tessellation passthrough declared every output
as a `vec4`, and a clip distance may not be one -- SPIR-V wants "BuiltIn
ClipDistance ... a 32-bit float array" and Tint enforces it, so the module was
rejected and the link failed with nothing but "Program linking failed" to go
on. ntv already carries the same workaround for `FRAG_RESULT_SAMPLE_MASK` a
few lines away. Fixing it converted 48 link failures into real behavioural
ones: no change in score, and the whole difference between a shader that
cannot build and a feature that is missing.

What remains in that family is `gl_CullDistance` written by a geometry or
tessellation stage. GL takes it from the last stage before rasterization and
this filter reads the vertex shader's; that is items 5, 6 and 8, and it is a
feature rather than a defect.

**The entire `shading_language_420pack` family failed on one cause, and the
giveaway was `utf8_characters_case_in_comment_character_two_bytes`.** A test
about a UTF-8 character in a comment has no business depending on the driver,
and it did not: every program in that family binds a sampler in *every* stage,
and the tessellation and geometry shaders were refused before the test began.

Three shader creators -- `create_tcs_state`, `create_tes_state` and
`create_gs_state`, each of which builds its NIR itself instead of going
through `create_shader_state` -- never called
`gex_wgpu_split_combined_samplers`, so a shader that samples reached the
SPIR-V emitter with a tex instruction carrying no texture deref. That is the
third stage to need that pass after compute and the fragment path.

Running the split is necessary and not sufficient, and the second half is the
more useful lesson. Nothing binds textures for those stages:
`gex_wgpu_bind_sampler_states` takes fragment, vertex and compute only, and a
real sampler bound anywhere else calls `gex_fail` -- which latches the context,
surfaces as `GL_OUT_OF_MEMORY`, and in the CTS **aborts the entire run at the
first case**. With the split alone, 329 cases became 1 case and "Test run was
ABORTED!" -- strictly worse than the 282 clean failures it started with.

So those shaders are now refused where they are created, by name. The score is
unchanged at 47/329 -- this fixes no case -- but the run completes, every later
case is judged on its own merits, and the message says which feature is
missing rather than reading like an internal error about derefs. **A refusal
must not latch, and a refusal that aborts the harness is worse than the
failure it replaces.**

### Where the CTS stands, and what is worth doing next

Measured against llvmpipe, with the reference number where it was taken:

    family                        gex        llvmpipe
    copy_image                    31/31      -
    gpu_shader_fp64               558/659    -
    shader_bitfield_operation     300/324    -
    tessellation_shader           132/140    -
    geometry_shader               117/136    -
    limits                        135/140    -
    cull_distance                 145/218    202/218
    shader_storage_buffer_object  78/124     -
    es_31_compatibility           235/355    355/355
    direct_state_access           288/371    370/371
    sample_variables              125/190    190/190
    shading_language_420pack      47/329     329/329

**Sampling in the geometry and tessellation stages is now built** -- see the
section below, which also records what it did and did not buy.
`direct_state_access` reported 267 results that were neither pass nor fail,
which is its own thing to look at.

### gl_SampleMask could not be translated at all, and three of four shapes are wrong

`sample_variables` was 8/190 against a reference that passes all 190, and 117
of the failures were one Tint rejection: `cannot assign 'array<i32, 1>' to
'i32'`. This one needed the disassembly rather than a guess, because every
obvious repair is also invalid:

| declaration | store | verdict |
|---|---|---|
| array | array value | valid SPIR-V, what Vulkan mandates -- **Tint refuses**: WGSL's `@builtin(sample_mask)` is a u32 *scalar*, so it flattens the declaration and has nothing an array can be assigned to |
| array | scalar | **invalid**: "OpStore Pointer's type does not match Object's type" |
| scalar | scalar | **invalid**: "[VUID-SampleMask-SampleMask-04359] BuiltIn SampleMask variable needs to be a 32-bit int array" -- Tint runs the Vulkan validation rules too |
| array | scalar **through an access chain to element zero** | valid SPIR-V, and the form the flattened builtin accepts |

ntv emitted the first. It now emits the fourth behind a new `ntv_info` flag,
so Vulkan consumers keep the composite-construct they require and only a
driver that has declared its consumer to be WGSL opts in -- beside
`omit_fragment_output_index_zero` and `avoid_spv_khr_fma`, which exist for the
same reason. 8/190 → 40/190.

What is left there is behavioural rather than translation: `gl_SampleMask`,
`gl_SampleID` and `gl_SamplePosition` semantics under real multisampling, plus
27 cases reporting `GL_INVALID_OPERATION` from `glCreateProgram`, which is a
separate thread.


## Sampling in the geometry and tessellation stages, and the bug behind it

GL lets every stage sample. These three run here as compute passes and nothing
bound their textures, so such a shader was refused at creation and the link
failed. Four things were missing:

- **Per-stage sampler state.** `ctx->stage_sampler_views` and
  `stage_samplers`, indexed by `gex_wgpu_compute_stage_index`. Both setters
  used to *latch* the context on those stages.
- **The binding range.** `gex_wgpu_split_combined_samplers` numbered the
  texture/sampler pair from zero for anything that was not literally
  `MESA_SHADER_COMPUTE`. These stages are *compiled* as compute and dispatched
  with one bind group, so from zero their textures landed on top of the pass's
  own blocks. They take the compute range now, which starts at 68 against the
  geometry pass's 0..4.
- **Texture entries in the group.** `struct gex_wgpu_bindings` could describe
  only buffers. Its two new kind masks are **64-bit**, because a dispatch's
  own blocks plus a texture and a sampler per unit no longer fit in 32.
- **Two helpers, not a fourth copy.** `gex_wgpu_collect_stage_textures` and
  `gex_wgpu_bind_stage_textures` serve all three stages, and build the layout
  entries and the group entries from the same numbers -- Dawn reports a layout
  that disagrees with its entries as an invalid bind group and nothing more
  specific.

What is still refused is refused by name *and reason*: a buffer texture in one
of these stages needs a storage-buffer binding of its own, and the message
says that rather than reading like an internal error.

### The bug this found is worth more than the feature

**A tessellated draw whose fragment shader had a flat varying rendered nothing
at all.** The provoking-vertex rewrite fired on it; that rewrite makes the draw
indexed; and `gex_wgpu_tess_draw` then declined "an indexed or instanced patch
draw" -- naming the index buffer the rewrite had created one frame earlier in
the call stack.

    #0  gex_wgpu_tess_draw
    #1  gex_wgpu_draw_vbo_inner
    #3  gex_wgpu_draw_provoking_last        <- made it indexed
    #6  u_vbuf_draw_vbo
    #7  _mesa_draw_arrays

The rewrite is wrong there twice over. A patch's vertex order is meaningful --
the control shader indexes `gl_in[]` -- so rotating it does not give back the
same primitive the way rotating a triangle does; and the provoking vertex
belongs to the primitives that are *rasterized*, which tessellation and
geometry stages generate downstream. Patch draws, and any draw with those
stages bound, are excluded now.

**Read the stack first.** Three rounds went into the probe's own shape before
that -- a one-vertex patch, then a three-vertex one, then a core profile
instead of compatibility -- and none of them was the cause. One gdb breakpoint
(with `set breakpoint pending on`, since the driver is a shared object) named
it outright.

### What it bought, and what it did not

`es_31_compatibility` went **117/355 to 150/355**. `gl44-stage-sampling-probe`
covers all three stages and passes 3 of 3, matching llvmpipe to the pixel.

`shading_language_420pack` went 47/329 to **48/329**, and that is the honest
number. Its programs bind a sampler in every stage, which is how the gap was
found, but they also want 64-bit integers (17 shaders: "unhandled integer
width: 64") and vertex-stage writable storage, and WGSL has neither. Lifting
one refusal reveals what was behind it, and a family gated on several platform
limits at once will not move for any single fix. The remaining refusal reasons
there are worth reading before picking the next thing:

    17  unhandled integer width: 64                       (no i64/u64 in WGSL)
     3  storage ... read_write ... vertex pipeline stage   (WGSL forbids)
     2  tessellation evaluation shader writes nothing
     1  a geometry shader after tessellation
     1  a buffer texture in the tessellation control stage


## 266 InternalErrors, and none of them were about what they said

`direct_state_access` scored 76 of 371 and reported **266 InternalError**
results -- a status neither pass nor fail, and the most anomalous number in
the whole sweep. Run individually, most of those cases pass.

The whole family is now **288/371** against llvmpipe's 370, and no latch fires
anywhere in it. Nothing about that came from making a feature work.

### The shape of the bug is worth more than the fixes

**A latch in a shared context is not one failure. It is every failure after
it, and the ones it causes look nothing like the one it came from.**

dEQP reuses a single GL context for a whole family. This driver latches
`ctx->failed` when it hits something it cannot do; every later call then
declines, which reaches the test as an unexpected GL error and is reported as
InternalError. So one case that legitimately could not run cost the two
hundred after it -- all of them for features they never used, all of them
passing when run alone.

Finding it needed the *order*, not the totals. The method was:

1. Count result kinds, not just pass/fail. `InternalError` being the majority
   is what said the failures were not about the features being tested.
2. Run one failing case alone. It passed -- so the failures were collateral.
3. Bisect on the prefix. Take one victim case, prepend each earlier group in
   turn, and see which prefix turns it from Pass into InternalError. That
   named `buffers_*`, then `buffers_functional`, in four runs.
4. Read the driver's own first-refusal line, which names the file and line
   that latched.

Steps 2 and 3 are the ones to remember. A family's score is not a measurement
until you know whether its cases are independent.

### Four faults, in the order they were peeled

**A buffer copied into itself.** GL allows it when the ranges do not overlap
-- `glCopyBufferSubData` says so, and a program compacting a buffer in place
does exactly that. WebGPU refuses it whatever the ranges ("Source and
destination are the same buffer"), and an invalid copy invalidates the command
buffer, fails the submit and latches the context. It goes through a staging
buffer now.

**An integer vertex attribute could not be captured.** The transform feedback
capture runs the vertex shader as compute and fetches attributes itself, and
it handled the four float formats and `R8G8B8A8_UNORM` and nothing else -- so
an `in int` (`r32_sint`) failed outright. A NIR value is an untyped bit
pattern, so the 32-bit integer formats are the same code as the float ones;
only the fill for absent components has to be an integer one. The narrow and
normalised formats still need unpacking, and are now declined *by name with
the format in the message* rather than refused blankly.

**Four capture exits latched for things that are not device errors** -- an
instanced draw, an unreadable index buffer, a stream output target with no
buffer, a vertex buffer the capture reads being unbound. They decline by name
now, and the **draw still goes ahead**: abandoning it as well made one wrong
answer into two, and the second was silent.

**A fragment sampler variant that cannot be built latched too**, though its
caller already dropped the draw on a NULL module, which is the right answer on
its own.

### What it did not do

The other families measured -- `sample_variables`, `es_31_compatibility`,
`shader_storage_buffer_object`, `shader_bitfield_operation`, `cull_distance`
-- are **unchanged**. This latch was specific to what `direct_state_access`
exercises. Worth stating, because "a latch poisons everything" invites the
assumption that fixing one lifts every family, and it did not.


## The driver was stricter than GL, and that looked like missing geometry

`sample_variables` and `es_31_compatibility` turned out to be the same tests
under two names -- 112 of the latter's 160 failures are `sample_variables.*`
nested inside it -- so they are one target, not two.

Checked for a latched context first, on the lesson from `direct_state_access`:
there is none here, 160 clean failures against a reference that passes
**355/355**. Its 45 NotSupported results are `samples_8`, which WebGPU caps at
4x; those are a genuine platform limit and llvmpipe passing them is expected.

**gl_SampleMask has no effect on a single-sample framebuffer.** GL performs
the multisample fragment operations -- where the shader's mask is ANDed into
the coverage -- only when `SAMPLE_BUFFERS` is one. WebGPU honours
`@builtin(sample_mask)` whatever the sample count, so a shader writing zero had
its fragments discarded here and drawn everywhere else.

The direction is the thing to remember. A driver bug usually shows up as
something drawn that should not have been; **this one was stricter than the
spec**, so it showed up as missing geometry, which reads like a lowering or a
binding problem rather than an over-eager one. The one-line reproducer
(`gl_SampleMask[0] = 0` on an ordinary FBO, expecting the fragment to survive)
settled it against llvmpipe in a single run.

**The fix is one lowering and one placement, and the placement was the hard
half.** `gex_wgpu_fragment_module` has *two* early returns that hand back the
base module when no variant is wanted. The decision to neutralise the mask has
to sit above both. Putting it below the first left a shader that needs no other
variant taking that path and going on discarding -- and then, having found and
moved past that one, below the second, with exactly the same symptom. If a
per-draw variant reason ever seems not to take effect, check how many ways the
function can return before it.

53/190 and 163/355. What remains there is format-specific: the failures are
`rgba8i`, `rgba8ui` and some `rgba32f`, and the first one examined declines
with

    draw declined (sample type differs from the shader's) slot=1
    shader_type=5 view_format=r8g8b8a8_unorm(84)

on the *verification* draw's second sampler, not on the integer texture it
renders to (which is created correctly as `r8g8b8a8_sint`). Multisampled
integer targets are a WebGPU limit and part of that tail is genuinely
unreachable, but the `samples_0` integer cases are not, and this is where to
resume. The binding diagnostic now prints the format's name, which is what made
that line readable.


## A sampler that is declared, not executed, and has nothing valid bound

The resume point from the previous section turned out to be worth more than
the section itself. `sample_variables` is **125/190** and
`es_31_compatibility` **235/355** -- the same tests nested -- from one change.

The failing draw was the CTS's *verification* shader, not the one under test:

    uniform highp isampler2D   u_tex;
    uniform highp isampler2DMS u_texMS;
    o_color = u_samples > 0 ? texelFetch(u_texMS, coord, sampleId)
                            : texelFetch(u_tex, coord, 0);

With one sample the multisampled half never runs, and its unit holds whatever
was left there. GL calls such a sampler **incomplete** and says it reads
(0,0,0,1); it does not say the draw is an error. WebGPU requires every
declared binding to match the layout and every layout entry to name a real
view, so the driver declined the draw -- and the frame lost geometry that had
nothing to do with the sampler in question.

**The usual answer does not work here.** An *unbound* unit already gets a 1x1
stand-in of the declared type. This one is bound, and the declaration to match
is a multisampled **integer** texture -- which WebGPU has no format for at
all, since integer formats have no multisample capability. There is no view
that can satisfy it.

So the sampler is removed from the variant and its uses fold to GL's own
incomplete-texture value. Two mismatches count as unsatisfiable -- integer
against non-integer, and multisampled against single-sampled -- and nothing
else: a depth view read as an ordinary float texture is a working shape that
other code deliberately relaxes, and a slot with *nothing* bound keeps its
stand-in, because there the shader may legitimately use the result.

**The binding side keeps the slot rather than compacting around it.** A bind
group layout may carry entries the shader does not use, so the omitted slot is
declared as a plain float 2D texture and bound to the ordinary stand-in. That
is three values changed instead of every index in two loops -- and index
arithmetic that disagrees between a layout and its entries is reported by Dawn
as "invalid bind group" and nothing more specific, which is a bad thing to go
looking for.

`gl44-unused-sampler-probe` pins both halves: the unused mismatched sampler,
and -- deliberately -- the same program with that sampler *selected* and a real
integer texture bound, so that "always ignore the second sampler" cannot pass.
Verified to discriminate: with the omission removed the first case fails and
the second still passes.


## fp64: the arithmetic was fine, the load was not

`gpu_shader_fp64` is 659 cases and had never been measured. It was **482/659**;
it is **558/659** now, from two fixes that both come down to a 64-bit type
reaching Tint, which refuses a module for it outright -- so one survivor costs
a whole shader, which fails the link and takes the program with it.

**The diagnostic that mattered was a pattern in the case names.** Every
`dmat*` variant passed and every `double` and `dvec*` variant failed, for all
48 builtins. That says the fault is not in the arithmetic -- a matrix does the
same arithmetic -- and it pointed straight at the load:

    gex_wgpu requires vec4-backed, 16-byte-aligned Gallium UBO loads:
    load_ubo bit_size=64 block-index-const=1

st turns every double *operation* into software 32-bit arithmetic, but a
`double` in a uniform block is still loaded as one 64-bit value, and this
driver's UBO lowering takes 32-bit loads only. Each component is now a two-word
load packed back into a 64-bit value -- which reintroduces nothing, because the
consumers are already `unpack_64_2x32` pairs from the doubles lowering and the
optimiser folds pack straight into unpack.

**That fold needs somewhere to happen, which is the other half.** st lowers in
the right order -- `nir_lower_doubles`, then `nir_lower_int64` -- but runs
`gl_nir_opts` *after* both, and an optimisation puts a 64-bit value back: an
if/else assigning a double becomes a `bcsel` on the packed value, with no
earlier pass left to see it. The driver now sweeps once more at the last point
before the emitter. Three things make that work rather than merely run:

- **The optimiser loop afterwards is not optional.** `nir_lower_int64`
  *represents* its results with `pack_64_2x32_split`, so it leaves 64-bit
  values behind by construction and relies on something folding the pairs.
- **A 64-bit local variable is not an operation.** No ALU lowering touches it;
  it reaches the emitter as `OpTypePointer Function %ulong`. It survives SSA
  conversion when it is an array indexed dynamically -- which the `dvec3` tests
  do and the `dvec2` ones do not -- so the indirect indexing is flattened and
  vars-to-ssa re-run first.
- **The sweep is gated on the shader containing something 64-bit at all.**
  Almost none do, and they pay one walk rather than a deref flattening that can
  grow a large shader.

`shading_language_420pack` went from 17 shaders refused for this to none. Its
*pass rate did not move* -- those cases fail for other reasons too -- and that
is worth stating rather than quietly folding into a headline number.

### The three constructs an ALU lowering cannot reach

That first sweep left 97 link failures, and my note here said some of them were
now *float* rather than integer -- that `nir_lower_doubles` would have to be
re-run from a software fp64 shader the driver does not have. **That was wrong,
and counting said so.** Grouping the surviving 64-bit values in the rejected
shaders by type gave one case with an f64 and a hundred with an i64: the
dominant problem was the kind the driver *can* sweep, and the sweep was missing
it. The instrument was a `GEX_WGPU_INT64_LOG` that prints every 64-bit
construct left at the end of the sweep -- and then, for a surviving ALU
instruction, what consumes it, which is what turned each round into a name.

`nir_lower_int64` rewrites **operations**. Three things in a NIR shader carry a
bit size without being one, so no amount of re-running it reaches them, and
each reaches the emitter as a 64-bit type Tint refuses outright:

- **A register.** The consumers said `store_reg`, and ntv gives every register
  an `OpVariable` in the Function storage class -- so a 64-bit one is the
  `OpTypePointer Function %ulong` in the disassembly. `nir_lower_int64` only
  rewrites SSA defs, and a register is not one.
- **A phi.** Putting the shader back into SSA turned those registers into
  phis, and a phi merging two 64-bit values stays 64-bit.
  `nir_lower_64bit_phis` splits each into a pair of 32-bit phis; it belongs
  *inside* the optimiser loop, because its sources are unpacks of the packs the
  lowering leaves behind and only the fold cancels the two.
- **An undef.** The last survivor was an `OpUndef %ulong` read by an
  `unpack_64_2x32_split` -- which is what kept it alive through DCE. Rebuilt as
  a pack of two 32-bit undefs it says the same thing in a type the emitter can
  spell, and the same fold removes it.

**The register form is borrowed, not discarded.** `nir_to_spirv` has no case
for a phi -- it asserts -- so a shader reaches it out of SSA, which is why the
64-bit values were in registers to begin with. Taking them out of that form is
only for the length of the lowering; the sweep ends with `nir_convert_from_ssa`
and hands the emitter back what it requires. Skipping that step swapped one
crash for another, in the same case, which is the clearest possible statement
that the conversion was load-bearing.

`gpu_shader_fp64` went **558/659 to 657/659**. The two that remain are not
64-bit problems at all: `named_uniform_blocks` wants a geometry shader after
tessellation, and `fp64.varyings` wants a `double` on an interface variable
split into two 32-bit halves across the stage boundary -- a construct that is
not an operation either, but one that lives in the linker rather than here.

## Three refusals in a row, and 261 cases behind them

`shading_language_420pack` sat at 48 of 329 for a long time, and the reason it
never moved is worth more than the number: **its whole failure was three
refusals stacked on one another, and lifting any two of them changes nothing.**
The first pass lifted one and the score stayed at 48 exactly. That is the
expected shape, not a disappointment -- but it is only readable if you know to
look for the next refusal instead of concluding the first was not the cause.

What made this family a single lever is how its harness is built. Every case
runs `testDrawArray` twice and `testCompute` once, and `testDrawArray` links
**vertex, tessellation control, tessellation evaluation, geometry and
fragment** with a sampler in each. So all 329 cases exercise the same five-stage
program, whatever the case is nominally about -- and 250 of them additionally
write their result through `writeonly uniform image2D`. Reading the CTS source
was what turned "281 failures" into "one shape".

**A caution that cost a wrong count here.** Most decline messages carry a
`static bool said`, so they print *once per process* and the whole family runs
in one process. One occurrence in the log does not mean one case. The
format-none refusal was countable at 250 only because it is printed per shader
at creation; the tessellation decline showed up once and covered all 329.

The three, in the order they surfaced:

**An image with no format qualifier.** GLSL permits `writeonly uniform image2D`
with no `layout(...)` on it, and GL then converts the store to the *image
unit's* format. WebGPU's storage texture binding carries a concrete format, so
the driver refused the shader. But the format is not unknown -- it is bound
state, exactly like a buffer texture's texel width, and the compute path
already has the machinery for that: no base module, one variant per resolved
configuration. Resolving the bound `pipe_image_view::format` at the top of the
dispatch, keying the variant on it and stamping it back onto the NIR variable
is the whole fix. An unbound unit resolves to rgba8, because GL says its stores
are discarded and the stand-in view is that already; a bound format WebGPU
cannot store is declined by name rather than approximated.

**A geometry shader after tessellation.** The tessellated draw ends by
rebinding a passthrough vertex shader over the tessellated mesh and re-entering
`draw_vbo`, and it already cleared `ctx->tcs` and `ctx->tes` before doing so --
so the geometry path, which sits immediately below the tessellation check in
that function, would have taken the draw if it had ever been allowed to reach
it. The refusal was older than the machinery that made it unnecessary. The one
thing that did need saying is which stage transform feedback records: with a
geometry shader bound *it* is the last stage before rasterization, so the
evaluation shader's capture and its `PRIMITIVES_GENERATED` count are skipped and
the geometry path's own are used. Doing both would have recorded the wrong
stage and counted it twice.

**A vertex shader that samples, in a capture pass.** Both emulated stages run
the vertex shader as a compute pass to capture its varyings, and that pass
declined any vertex shader with a texture. The comment on the refusal already
named the fix -- "the variant's textures remapped into group zero and bound by
the dispatch" -- and the reason it was real: the combined-sampler split stamps
a *vertex* shader's texture into descriptor set 3, a dispatch has only group
zero, and a pipeline using a group with no layout is invalid, which loses the
whole command buffer rather than one draw. The variant already sets
`info.stage = MESA_SHADER_COMPUTE`, so it need only renumber the texture and
sampler variables with the same arithmetic the split uses for a compute stage,
collect them with `gex_wgpu_collect_stage_textures`, and bind them from the
vertex stage's own sampler state at each of the five dispatch sites.

48 -> 309 of 329.

### A uniform buffer is padded to 16, and the buffer must cover it

One real device error surfaced behind the three: a 12-byte buffer bound to a
uniform block, where WebGPU rounds the binding size up to 16 and then refuses
the buffer for not covering it. Buffers *created* as constant buffers were
already padded; a GL buffer object is retargetable, so one made as an array
buffer and bound to a uniform block later was not. Every buffer allocation is
now 16-aligned -- the same reasoning the usage union already used -- and the
binding size is rounded up and clamped to what the allocation holds.

### A spilled uniform block is read-only, and nobody had said so

The last case with a device error, `binding_uniform_block_array`, took four
more steps, each one revealed by the last -- the same shape as the three
refusals, but in validation rather than in refusals.

A shader with more uniform blocks than the device's uniform-buffer limit has
the overflow **spilled into storage buffers**. Nothing marked them read-only,
so the emitter declared them `var<storage, read_write>`, and WGSL forbids that
in the vertex stage outright: such a shader did not compile at all, however
little it did with the blocks. Marking `ACCESS_NON_WRITEABLE` at the spill
fixes the module, and then three things that must agree with it had to be told:
the dispatch's bind group (`readonly_mask`), and the graphics layout for both
the vertex and fragment stages, whose comment still said "Storage, not
ReadOnlyStorage ... the emitter declares every storage buffer read_write".

Two more sat behind those. `maxDynamicUniformBuffersPerPipelineLayout` defaults
to 8 and this driver binds every uniform block with a dynamic offset, so eleven
blocks across two stages exceeded it -- with Dawn again saying in the message
that the adapter supports 16. And a spilled block in a compute-emulated stage
was still stamped with the *graphics* descriptor set, which a dispatch has no
group for.

**That last fix is the one that bit.** Moving "storage blocks below the storage
binding range" to group one also moved the pass's *own* blocks -- the capture
parameters, the emitted vertices, the per-primitive counts all number from
zero and are storage blocks too. Every emulated-stage pipeline in the run went
invalid and the family fell straight back to 48. The predicate has to be the
spilled-uniform range exactly, `[STORAGE_UBO_FIRST, SSBO_FIRST)`, bounded on
both sides. **A single case passing is not evidence a shared path is intact**:
`binding_uniform_block_array` was the case under test and it improved at every
step, including the step that broke 261 others.

`shading_language_420pack`: **310 of 329**, with no WebGPU validation errors
left anywhere in the run.

### What is left in this family

19 cases, and 16 of them are one missing feature: **images in the stages that
are not compute.** `binding_image_single_stage_vertex`, the eight
`binding_images_texture_type_*` shapes and the rest all bind an image to the
vertex, tessellation or geometry stage, which this driver serves nowhere but
the fragment and compute paths. That is the next lever here, and it is a
feature rather than a refusal to lift. The other three are `line_numbering`,
`binding_sampler_array` and `binding_samplers_texture_type_buffer`.

## Dawn does not return null

Two image paths declined "a slice of a 3D texture bound as a non-layered
image2D" by testing whether `wgpuTextureCreateView` returned null. It never
does. The WebGPU C API reports validation through the device error scope and
hands back an *invalid* object, which then invalidates the bind group, the
pipeline, the command buffer, and everything recorded with it -- so one case
asking for an impossible view took unrelated work down with it. A null check
reads like a guard and is not one.

The combination has to be recognised from state already held, before the object
is asked for: `wgpuTextureGetDimension` and `wgpuTextureGetDepthOrArrayLayers`
on the target. **Expect more than one rule per combination.** The 3D-slice case
is rejected twice over -- once for its layer range and once for its view
dimension -- and fixing only the range simply moved the error:

    Texture view array layer range (baseArrayLayer: 1, arrayLayerCount: 1)
    exceeds the texture's array layer count (1).
    -> The dimension (TextureViewDimension::e2D) of the texture view is not
       compatible with the dimension (TextureDimension::e3D) of [Texture]

The same shape appears in bind group layouts. `max_shader_buffers` is
deliberately over-reported (a shader using the maximum of *both* halves at once
is a combination nothing does), so a shader combining storage blocks, buffer
textures and packed images can exceed the device's real per-stage limit --
which WebGPU reports as a validation error rather than a failed create. Both
the draw path and `launch_grid` now count the storage entries first and decline
by name. **The draw path declines without latching**: this is a device limit,
not a driver failure, and a latch would cost every case after it.

`shader_image_load_store` did not move (14/51) -- these were all cases already
failing for want of the feature -- but the run no longer emits WebGPU
validation errors, which is the thing worth having: an error there was never
confined to the case that caused it.

### Clearing the rest of the validation errors, and the bug underneath

Chasing `shader_image_load_store`'s remaining WebGPU errors to zero turned up
four more of the same shape and one that was not.

- **A texture created for image use alone had no `TextureBinding`.** A
  *read-only* image is not a storage texture in WGSL at all -- Tint lowers one
  to a plain `texture_2d<T>` read with `textureLoad` -- so it wants a sampled
  binding. `StorageBinding` is already granted to every storable single-sampled
  texture, because `glBindImageTexture` arrives long after creation; the same
  argument gives `TextureBinding`, and the two now travel together.
- **The 1x1 stand-in for an unbound image unit** had the same gap, and then a
  second one behind it: the cache was keyed on dimension and format, so one
  texture served both a read-only unit and a write-only one in the same pass --
  "writable usage and another usage in the same synchronization scope". The
  access is part of the key now.
- **Two writable storage bindings over one range** cannot be repaired: it is
  one texture bound to two image units, and the packed buffer lives on the
  resource, so both bindings are the same buffer. The existing
  `GEX_WGPU_ALIAS_TRACE` helper already walked the pairs -- it deliberately
  ignored same-type pairs, because the mixed pair was the case being chased
  then -- so it now returns whether it found the unfixable one and the draw
  declines. It stays cheap by counting writable bindings first; almost every
  draw has none.

**And then the one that was not a validation error.** Adding that decline made
the run *crash*, in a later case, inside Dawn's `AbslFormatConvert` --
formatting a dangling object's label for a validation message. A packed image's
copy back into its texture is deferred to when the render pass closes, and the
pending copy held its `pipe_resource` as a **bare pointer**; what normally kept
it alive was the batch, which a draw touches at its *end*. A draw that declined
after registering the copy left nothing holding the resource, so it died with
the test case and the flush copied into freed memory. The pending copy takes a
reference now, released on every path that clears the queue.

That bug was already there -- two older declines on the same path have the same
shape and had simply never been reached. It is worth noting how it presented:
the case crashed only in company, passed alone, and the stack named neither the
draw that declined nor the image. One `gdb -batch -ex run -ex bt` on the pair
gave the whole thing.

`shader_image_load_store` 14 -> 15 of 51, and its run went from about forty
WebGPU validation errors to **none**.

### A decline that became a latch, and what it was hiding

The capture pass learning to sample had a consequence I did not see at the
time. A vertex shader with a texture used to be refused when its *variant* was
built, and that path declines. Now the variant builds and the binding can fail
later instead -- inside a chain that ends in `gex_fail`. So a sampler view the
driver would not make turned into a **context latch**, and `KHR-GL45.texture_view`
died on its first case: three cases ran where there are seven.

Both remaining sites now test the binding separately and decline by name. The
rule is worth restating because it is easy to lose in a refactor: **when
lifting a refusal, check what the newly reachable path does on failure.** A
decline moved into a chain whose failure latches is a regression even though
nothing about it looks wrong.

Behind the latch were two more layout gaps in the compute-emulated stages,
both invisible until the family could run:

- **`texture.multisampled` was never set** in a dispatch's bind group layout,
  so a `sampler2DMS` in a geometry or tessellation shader produced "Binding
  multisampled flag (0) doesn't match the layout's multisampled flag (1)". The
  collector had worked the flag out for the sample type and thrown it away.
- **The non-filtering sampler twin was used only for depth.** An integer or
  multisampled texture is `UnfilterableFloat`/`Uint`/`Sint`, so its sampler
  binding is NonFiltering, and the application's ordinary sampler is refused
  against it. Depth was simply the first shape to reach that code.

The same deferred-copy reference bug was in `alias_copies` as well as
`packed_copies`; both hold a reference now.

`texture_view` still fails 6 of 7 (llvmpipe passes all seven) and the run is
clean of validation errors -- those six want format-class view sampling, which
is the next lever there.

### texture_view, characterised

Seven cases, llvmpipe passes all seven, this driver passes one. Now that the
run is clean of validation errors the six are separable, and most are not
driver gaps:

- **`gettexparameter` and `errors`** fail at `glTexStorage*Multisample`.
  WebGPU has no multisampled *array* texture (a texture with `sampleCount > 1`
  must have one layer), and no multisampled `rgba32float` either. Both are
  what the CTS asks for. Platform-blocked.
- **`view_sampling`** wants `sampler2DMSArray`, and WGSL has no
  `texture_multisampled_2d_array`. Platform-blocked -- and it now says so:
  that refusal used to read "gex_wgpu invalid or duplicate texture binding",
  one message shared by five conditions, and a link failure is the only thing
  the application sees. Each condition names itself now.
- **`view_classes`** wants an `r16g16b16a16_unorm` view of an
  `r16g16b16a16_float` texture. Same texel size in GL, different *stored* size
  here, because the driver promotes `rgba16_unorm`. The format promotion
  fighting views again.
- **`coherency`** ("Invalid data was sampled in vertex shader") and
  **`reference_counting`** ("Mip-map sampling failed") are real driver bugs and
  the two worth chasing next.

Three view rules were stricter than WebGPU requires and are relaxed:

- A **1D texture** could only be viewed as 1D. `VIEW_CLASS_1D` is symmetric and
  the array half was already there; st builds a one-layer array view of an
  immutable 1D texture exactly as it does for 2D.
- A **cube view** demanded layers 0..5 of its parent, and a **cube-array view**
  a first layer that was a multiple of six. WebGPU asks only for a layer
  *count* that is a multiple of six; where the faces begin in the parent is
  the parent's numbering, not the view's.

And the refusal message printed the texture's target without the view's --
which is the half that decides `gex_wgpu_view_target_ok`. It prints both now.

### The feedback capture can sample too

Two of `texture_view`'s failures -- `coherency` and `reference_counting` -- turn
out to be the *other* capture refusal, the one the last pass did not lift.
Both sample a texture view in the **vertex shader** and read the answer back
through transform feedback, and `gex_wgpu_capture_variant_get` declined any
vertex shader with a texture for exactly the reason the varying variant did:
the split numbers a vertex stage's texture into descriptor set 3 and a dispatch
has only group zero.

The lift is the same -- renumber into the compute range, collect with
`gex_wgpu_collect_stage_textures` -- but the binding is not, because this
dispatch builds its bind group layout by hand rather than through
`gex_wgpu_bindings`. Its arrays grew by two entries per sampler unit and the
pair is appended there, with the same depth and non-filtering rules the other
binders apply.

**And it immediately produced 808 validation errors.** Not new ones: two
feedback targets on one buffer is writable-storage aliasing, and this was the
one dispatch the aliasing check did not cover, because it was also the one
building its layout by hand. It had never mattered while a capturing shader
that samples was refused outright. With the check wired in, the run is clean.

`texture_view` 1 -> 3 of 7. `transform_feedback` measured at 6/21 before and
after -- by stashing the change and rebuilding, rather than by argument.

## Padding is not size

`KHR-GL45.transform_feedback` sat at 6 of 21 (llvmpipe: 21) and the failures
looked general -- most cases reporting "context already failed". They were not
general. One case latched, and the thirteen after it were collateral.

The latch was an index-range check in `draw_vbo_inner`: a geometry draw reading
four indices out of a two-index buffer. The buffer was the driver's own
`gs_indices`, grown on demand by `gex_wgpu_ensure_buffer` -- which compares the
request against `allocation_size`. **That is the padded number**, and every
buffer allocation was made 16-byte aligned two sessions ago (WebGPU rounds a
uniform binding up to 16, and a GL buffer object is retargetable). So a buffer
created for 8 bytes reported 16, satisfied a request for 16, and came back with
its `width0` still 8.

Comparing `width0` instead took the family from **6 to 19 of 21**.

Two things about this are worth keeping.

**The data was never wrong.** `allocation_size >= size` did hold, so the memory
was there and the geometry pass wrote into it correctly; only the code that
reads `width0` disagreed. That is the index check -- and
`gex_wgpu_buffer_subdata`, which bounds-checks the same field and would have
*silently dropped* a tessellation index upload rather than complaining. A
latch that shouts is the lucky version of this bug.

**The standing gates all missed it, for two sessions.** The probe suite, STK
and Blender were clean throughout, because none of them grows a driver-internal
buffer past a 16-byte boundary. `scratchpad/ctsgate.sh` now runs a few fast
families that lean on those buffers -- transform feedback and texture views --
as a gate beside the probes. One run of it would have caught this the day it
landed.

The index check declines now rather than latching. It still names the problem
loudly, which is all it needed to do: latching is what turned one bad draw into
a family that looked broken from the start.

## Layered rendering

`KHR-GL45.geometry_shader` was 118 of 136 against llvmpipe's 136, and half the
gap was one feature: a geometry shader writing `gl_Layer`, which the driver
answered with a message saying it rasterized the bound layer only.

WebGPU attaches one array layer at a time, so layered rendering has to be one
pass per layer -- which is **exactly the shape the viewport routing beside it
already had**. `gex_wgpu_gs_route_layers` is that function against a different
piece of state: sort the emitted triangles by the layer their first vertex
asked for, then draw each run with that layer installed on every attachment.
The render pass key already includes the attachment layer, so moving
`ctx->color_layers` between draws opens the next pass by itself, and `loadOp`
is always Load so a later layer does not disturb an earlier one.

Three things had to be true besides the routing.

- **The framebuffer had to admit it was layered.** `set_framebuffer_state`
  recorded only `first_layer`; it now also records how many layers every
  attachment has in common, which is what bounds the routing. One for every
  ordinary framebuffer, so nothing else changes.
- **A layered depth attachment had to be accepted.** It was rejected outright
  -- `first_layer == last_layer` or decline -- which took the context with it.
- **`is_format_supported` had to allow an array as a render target at all.**
  It granted `PIPE_BIND_RENDER_TARGET` only for `PIPE_TEXTURE_2D`, so st was
  told the *texture* could not exist long before anything drew. WebGPU renders
  into one layer of an array through `baseArrayLayer`; the restriction was
  simply wrong. Not 3D, which genuinely has no 2D view of a slice.

And then `glClear` had to clear every layer, which GL says and the driver did
not: it cleared the bound one and left the other cascades or cube faces holding
whatever was there. That alone was two more cases.

118 -> **121 of 136**, with no validation errors. What remains is four cases
that create a `GL_TEXTURE_2D_MULTISAMPLE_ARRAY` before doing anything else --
WebGPU has no multisampled array texture, so they are blocked -- geometry
shader *instancing* (two cases, refused by name), and two shaders whose
combined-sampler split fails at the texture-unit limit.

### Geometry shader instancing

`layout(invocations = N) in;` runs the geometry shader N times per input
primitive, with `gl_InvocationID` saying which run this is. It was refused by
name.

The compute pass is already indexed by a flat invocation id, and that id was
doing two jobs at once -- picking the input primitive *and* choosing the output
slice. Separating them is the whole feature: dispatch `prims * N` invocations,
and the quotient `id / N` picks the primitive while the remainder `id % N`
**is** `gl_InvocationID`. The output slice stays one per compute invocation, so
the emit path did not change at all. With N = 1 the division folds away and
every existing shader compiles to what it compiled to before.

On the host, the distinction to keep straight is which counts mean *input
primitives* and which mean *runs of the shader body*. Everything the pass
writes -- the output slices, the emitted-count slots, the index runs -- is
sized in runs; `gl_PrimitiveIDIn` and the primitive-id base advance in
primitives. `gl44-gs-instancing-probe` pins the division down in the direction
that matters: getting quotient and remainder the wrong way round is not a
crash, it draws the same number of triangles in the wrong places, so one case
puts the invocation id in the *position* and the other puts the primitive in
the *colour*.

`geometry_shader` 121 -> **123 of 136**.

### A sampler array indexed by a variable

`texture(samplers[i], c)` with a variable `i` -- a texture atlas, a material
array, any shader looping over its inputs -- was refused, and the refusal
reached the application as a failed link with no reason attached: *five*
different conditions shared the message "could not split a combined sampler".
Naming each one is what found this.

WGSL has no sampler arrays in any form, so the index cannot survive into the
module. It can be **unrolled**: the array is a known, small number of units, so
the instruction becomes one copy per unit under a comparison against the index,
each copy naming its unit as a constant. The result lands in a function
temporary rather than a phi web -- the builder makes that far easier to get
right -- and vars-to-ssa turns it back into values immediately.

Three things about it were not obvious, and each cost a build:

- **A clone's sources are not in any use list until the instruction is
  inserted.** Removing one from an uninserted clone walks a list node whose
  `prev` is null. Insert first, then edit.
- **The index arrives in two spellings.** The graphics stages have it as a
  `texture_offset` src (st has lowered samplers to indices); the compute and
  emulated stages still have a deref with a dynamic array index. An
  instruction can carry *both*, and fixing only one leaves it dynamic as far
  as the split is concerned -- which is what kept refusing after the first
  version.
- **The texture and sampler offsets are different SSA values.** They compute
  the same element, because nothing has CSE'd them yet. Requiring the same
  *value* refused every real case; the condition that matters is the same base
  unit, which a combined sampler guarantees and a separated pair would fail.

`shading_language_420pack` 310 -> **311 of 329**. The two geometry cases that
still fail want an *integer* sampler in an emulated stage, which Tint's SPIR-V
reader refuses ("sampled image must have float component type") and which the
compute path already answers with per-dispatch variants -- the same treatment
extended to the emulated stages is the next step there.

