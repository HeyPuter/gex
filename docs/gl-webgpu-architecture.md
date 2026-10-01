# Desktop OpenGL on WebGPU

## Decision

The production path uses Mesa's OpenGL state tracker and NIR compiler
middle-end, backed by a WebGPU Gallium driver (gex_wgpu).  It is the only
browser GL backend.

History: gex first drew desktop GL on WebGPU through a fork of MobileGL, an
OpenGL implementation on WebGPU.  It was used as a prototype and briefly as a
differential reference, then disabled, and has since been removed from the
tree.  This decision replaced the plan to add geometry and tessellation by
rewriting glslang SPIR-V inside that backend's renderer.  That plan addressed
one visible gap in one backend, but retained several independent sources of
incorrect behaviour:

- the backend implemented desktop GL object, validation, synchronization, and
  framebuffer semantics itself.
- shader translation is followed by WGSL text rewriting for bindings, clip
  space, interpolation, transform feedback, samplers, and interface packing;
- draw setup reconstructs GL pipeline state directly as WebGPU descriptors;
- unsupported combinations frequently become a dropped draw or a poisoned
  WebGPU command buffer rather than the GL-specified result or error.

Those are system-wide correctness risks.  Fixing the scene observed in one STK
run cannot establish that another state transition, shader, material, map, or
application is correct.

## Reused compiler work

The decision is based on Mesa main at commit
`d0b3b0fc8797b9609cc3a2f23273efd9e6f04d29`, not on an assumed capability.
The relevant implementation is in:

- `src/poly/nir/poly_nir_lower_vs.c`
- `src/poly/nir/poly_nir_lower_tess.c`
- `src/poly/nir/poly_nir_lower_gs.c`
- `src/poly/nir/poly_nir_lower_sysvals.c`
- `src/poly/nir/poly_nir_passthrough_gs.c`
- `src/asahi/libagx/geometry.cl`
- `src/gallium/drivers/asahi/agx_state.c`
- `src/gallium/drivers/zink/nir_to_spirv/`

Mesa's `poly_nir` path already supplies the general pre-raster execution model:

1. lower a vertex shader preceding tessellation or geometry into a compute
   producer writing a global vertex-output arena;
2. lower tessellation-control I/O and barriers to patch storage;
3. generate tessellated coordinates, topology, and an indexed indirect draw;
4. lower geometry emission into count, optional prefix-sum, main, and raster
   variants;
5. run a synthetic raster vertex shader over the expanded output;
6. perform transform feedback before rasterization, including multistream and
   dynamic-output cases.

WebGPU still needs target-specific lowering for Mesa's remaining `poly`
intrinsics and memory ABI.  It does not need a new GLSL/SPIR-V geometry
compiler.

## Data path

```text
stock desktop application
  -> existing gex libGL/GLX/EGL call transport
  -> Mesa st/mesa compiled into the graphics worker
  -> NIR and Mesa poly pre-raster lowering
  -> WebGPU Gallium pipe driver
  -> WebGPU
```

Keeping the standard library ABI at the guest boundary means STK, glxgears,
Mesa applications, and other desktop programs remain unmodified.  This path
does not use virglrenderer and does not require guest Mesa.  The GL frontend is
Mesa in the browser graphics worker; the guest library remains a marshaller.

Initially the worker may use NIR-to-SPIR-V from Mesa's Zink compiler followed
by Tint's SPIR-V-to-WGSL conversion.  That is an output format bridge, not the
place where GL semantics or pre-raster stages are implemented.  A direct NIR
to WGSL backend is only justified later if measurements show this compile path
to be material.

## Driver boundary

The WebGPU Gallium driver owns only Gallium-to-WebGPU work:

- resources, views, samplers, mapping, copies, and hazards;
- framebuffer attachment and format compatibility;
- graphics and compute pipeline creation and caching;
- NIR target legalization and WebGPU binding assignment;
- command encoding, submission, fences, queries, and presentation;
- target support for the buffers, scans, and indirect draws emitted by
  `poly_nir`.

It must not reimplement GL validation, object lifetime rules, pixel-store
semantics, or shader linkage.  Those stay in Mesa's tested frontend/compiler.

## Migration sequence

1. Build a minimal Mesa `pipe_screen`/`pipe_context` WebGPU target in the
   browser worker, with buffers, RGBA8 textures, clear, and synchronized
   readback.
2. Connect st/mesa to that screen and run the existing guest GL transport into
   Mesa.
3. Bring up vertex/fragment draw, indexed draw, textures, FBOs, and queries.
4. Port the Mesa `poly` support ABI and prove VS, GS, TCS/TES, and final-stage
   transform feedback with deterministic pixel and buffer comparisons.
5. Expand Khronos CTS by semantic families.  A feature is supported only when
   its official cases and local memory-canary variants pass in a real browser.
6. Make Mesa/WebGPU the only backend.  (Done: the earlier backend has been
   removed.)
7. Profile STK and optimize real presents only after the relevant rendering
   families pass.  Visual inspection is a smoke test, never the correctness
   gate.

## Required evidence

Every implementation increment needs all applicable evidence below:

- a native Mesa reference result;
- a real-browser WebGPU result with exact pixel or buffer comparison;
- guest-memory canaries around every returned or mapped payload;
- GL error and state-preservation assertions for invalid operations;
- the corresponding Khronos CTS family, with unexpected pass, fail, crash,
  timeout, and trap all distinguished;
- no regression in the previously passing corpus.

Symbol export counts and a successful STK launch are diagnostics only.  They
are not conformance evidence.

`tests/mesa-gallium-sanitize.sh` is a mandatory safety gate for changes to the
WebGPU Gallium driver.  It builds Mesa and the driver in a separate
debug-optimized Emscripten configuration with ASan and UBSan, without full or
ThinLTO, and executes the shared semantic corpus first through softpipe and
then through a real browser WebGPU device.  Release-build success cannot
substitute for this gate.  Application testing and FPS measurement stop on the
first sanitizer finding.

Alignment UBSan alone is disabled for this wasm32 build.  Mesa's intrusive-list
iteration intentionally applies `container_of` to a 4-byte-aligned list
sentinel as a temporary fake enclosing object; some enclosing NIR records also
contain `uint64_t` and therefore have 8-byte type alignment.  The iterator does
not access those fields through the sentinel, but alignment UBSan diagnoses the
intermediate typed pointer.  ASan and all other UBSan categories remain active.

## Correctness ladder

The layers are proved independently and in order.  A failure at one layer is
reduced there; it is not debugged by repeatedly launching an application.

1. **Wire framing:** generated guest and worker schemas agree exactly.  Short,
   oversized, unknown, and trailing payloads are rejected.  Any decoder error
   permanently poisons that graphics connection; its partial reply is dropped
   and the worker context is never resumed.
2. **Memory ownership:** every guest output uses a size derived from the GL
   query, has canaries on both sides, and never exposes a worker/Wasm pointer.
   Mapped buffers have explicit ownership, range, flush, and invalidation
   tests.
3. **GL state semantics:** small programs compare queries, errors, object
   lifetime, framebuffer completeness, synchronization, and exact returned
   bytes against native Mesa.  Each test varies one state family while holding
   the rest fixed.
4. **Shader and raster semantics:** a corpus records native-Mesa shader logs,
   reflection, transformed buffers, and exact/toleranced pixels.  Vertex
   fetch, interpolation, clipping, depth/stencil, blending, sampling, image
   operations, compute, and pre-raster stages are separate families.
5. **Official CTS:** pinned Khronos cases run in a real browser.  Pass, fail,
   crash, trap, timeout, and harness failure are distinct durable results.
6. **Applications:** traces from unmodified desktop programs select missing
   test families.  STK becomes a broad regression smoke test only after those
   families pass; visual inspection never substitutes for them.

Softpipe replay proves the transport and Mesa frontend while the WebGPU
Gallium driver is built.  It advertises only GL 3.3 and cannot prove GL 4.4 or
WebGPU rendering.  The future WebGPU screen must derive its advertised version
from tested Gallium capabilities rather than a hard-coded version string.

## Current WebGPU proof boundary

The bring-up test deliberately proves only the following composition in a real
browser:

The semantic portion is a single source corpus built twice: once against Mesa
softpipe as the reference implementation and once against the WebGPU Gallium
driver.  `tests/mesa-gallium-differential.sh` requires both executions to pass;
only WebGPU-specific validation and malformed-command checks remain outside
the shared portion.  This gate has already caught an uninitialized Gallium
viewport/state assumption, missing NIR driver-location assignment, and an
incorrectly inverted WebGPU `frontFace` translation.  Extending it to uniform
buffers also exposed that the standalone harness had not initialized Mesa's
dynamic GLSL type singleton; the apparent Emscripten stack-cookie failure was
the resulting invalid `glsl_array_type()` access, not a backend draw result.

- single-sample `R8G8B8A8_UNORM` 2D color targets plus `Z32_FLOAT` and
  `Z24_UNORM_S8_UINT` depth/stencil attachments;
- full-surface clear and synchronized texture-to-buffer readback;
- vertex and fragment NIR compiled by Mesa's Zink NIR-to-SPIR-V emitter;
- SPIR-V translated to WGSL by pinned Tint;
- non-indexed and 16-bit indexed triangle-list draws, with tested vertex,
  texture/sampler, blend, depth/stencil, scissor, viewport, and rasterizer
  state variants;
- one smooth `vec4` vertex-to-fragment varying;
- an asymmetric triangle whose exact 16/64-pixel coverage establishes the raw
  WebGPU target's top-left row convention, with exact green foreground and
  blue clear-color comparison at every pixel.
- positive Gallium Y viewport scale lowered to a Y-flipped vertex variant,
  because WebGPU forbids negative viewport height, plus exact 6/64-pixel
  viewport/scissor intersection coverage.
- GL clip-space `z=-0.5` accepted and `z=-1.25` rejected after Mesa NIR
  half-Z lowering, with exact full-target foreground/background comparisons.
- aligned and unaligned-size queue uploads backed by a bounds-checked CPU
  shadow; `float2`, `float3`, and `float4` vertex fetch; an interleaved
  20-byte layout combining `float4` position with normalized `RGBA8` color;
  a nonzero buffer offset; and 16-bit indexed drawing.  Exact asymmetric masks
  and colors are shared with softpipe.  A separate-stream case sources position
  and normalized color from two independently-strided buffers, then drops the
  application's color-buffer reference before drawing to prove binding
  lifetime.  A divisor-one color stream proves per-instance stepping: separate
  one-instance draws select instance zero and nonzero base instance one, then
  a two-instance overlapping draw proves ordered selection of the second
  instance.  Indexed draws scan the validated index shadow to bound every
  attribute fetch, with an out-of-range index regression proving the WebGPU
  context fails closed before issuing a draw;
- up to eight vertex- and fragment-stage constant-buffer slots.  Exact red output
  from fragment offset zero is followed by exact cyan after rebinding a
  256-byte-aligned subrange; a vertex-stage load supplies exact magenta through
  a varying; and a draw with both stages bound proves simultaneous descriptor
  construction.  Application references are dropped before the lifetime
  draws.  A user-pointer constant buffer is synchronously uploaded and a
  two-`vec4` block proves a nonzero internal shader offset.  A vertex shader
  then indexes three position vectors with `vertex_id`, proving dynamically
  indexed 16-byte-aligned loads through the exact asymmetric coverage mask.
  Both backends now receive canonical Gallium `load_ubo` NIR; the WebGPU driver
  lowers supported aligned loads to typed UBO dereferences before
  NIR-to-SPIR-V.  Two independently allocated fragment UBOs add to exact yellow
  after both application references are dropped.  The WebGPU layout is
  assembled from lowered NIR set/binding declarations, rejects collisions, and
  separates texture/samplers, vertex UBOs, and fragment UBOs into bind groups
  zero, one, and two respectively;
- texture, sampler, and fragment UBO descriptors coexisting in one shader and
  pipeline layout.  A nearest green texture sample plus a red uniform produces exact
  yellow output after the uniform-buffer application reference is dropped;
- up to eight slot-paired fragment textures and samplers with collision-checked
  WebGPU bindings.  Two independent one-texel resources are sampled and added
  to exact yellow after both application resource references are dropped.  The
  same shader then changes only slot zero to a BGRA view and produces exact
  cyan.  Variant lowering selects state from each texture instruction's sampler
  dereference, and the cache key includes every referenced slot.  The test
  restores slot zero afterward so subsequent filter cases also prove
  state-history isolation;
- separate NIR texture/sampler descriptors translated through SPIR-V and Tint,
  a 2x2 RGBA8 upload with four distinct texels, nearest/clamp sampling of the
  selected green texel, and linear sampling at the four-texel center producing
  the exact bilinear RGBA8 average over the asymmetric mask.  The nearest and
  linear cases use distinct sampler objects and fragment pipelines to cover
  state rebinding as well as the filter result;
- a one-texel sub-rectangle replacement, repeat and mirrored-repeat addressing
  outside `[0,1]`,
  vertex-to-fragment texture-coordinate transport, and sampling explicit LOD 1
  from a two-level texture/view.  The application resource reference is dropped
  before the latter draws, proving that the bound sampler view retains the
  texture for its required lifetime;
- nearest clamp-to-border with arbitrary finite float border colors, lowered in
  a sampler-keyed Mesa NIR fragment variant because WebGPU has no border-color
  sampler mode.  Rebinding cyan then yellow proves variant invalidation, while
  a constant-outside shader proves an explicit WebGPU pipeline layout remains
  valid even when Tint removes the now-dead texture operation;
- linear clamp-to-border at fixed LOD, using a NIR texture-size query to compute
  the separable in-range bilinear weight and blend the clamped WebGPU sample
  against the GL border color.  The exact half-texel result
  `255,0,128,255` matches the pinned native-Mesa sampling corpus;
- nearest and linear mip selection, including an exact trilinear LOD 0.5 blend,
  plus a view whose base and last level are both resource mip 1; view-relative
  shader LOD 0 returns that underlying level;
- `R8G8B8A8_SRGB` upload and sampling into a linear UNORM target, decoding
  encoded `128,128,128` to exact `55,55,55` as in the native-Mesa corpus;
- sampled `R8_UNORM`, `R8G8_UNORM`, and `B8G8R8A8_UNORM` resources with
  byte-accurate row sizing.  Exact results prove missing-channel defaults
  `(0,0,1)` and BGRA memory order.  These formats remain sampler-only until
  their render-target semantics are separately proven;
- filterable `R16_FLOAT`, `R16G16_FLOAT`, and `R16G16B16A16_FLOAT` sampling
  with exact half decoding and missing-channel defaults.  `RGBA16_FLOAT` is
  additionally proven as a render target: a fullscreen fragment draw writes
  `(0.25,0.5,0.75,1)`, then the result is sampled into RGBA8 and read back as
  exact `64,128,191,255` after dropping the application texture reference;
- layered uploads and views for 2D arrays and cube maps.  A two-layer array
  returns exact green from layer one, and a six-face cube returns exact green
  from the negative-Z face while all other faces are red.  Both draws happen
  after dropping the application's texture reference;
- depth texture views, comparison samplers, and shadow NIR.  A `Z32_FLOAT`
  texture cleared to `0.5` is sampled with `LEQUAL`: reference `0.25` produces
  exact white while `0.75` produces exact black.  The shader metadata selects
  WebGPU `Depth` and `Comparison` binding types, and both draws occur after the
  application resource reference is dropped;
- draw-time descriptor validation checks the bound view's sample type and
  dimensionality against the NIR-derived WebGPU layout.  This prevents a float,
  depth, array, or cube view mismatch from becoming a WebGPU validation error or
  a silently dropped draw;
- texture-only descriptors are distinct from sampled texture/sampler pairs.
  Integer `texelFetch` now maps directly to WebGPU `textureLoad` without a fake
  sampler binding.  Shared softpipe/WebGPU cases prove exact `R8_UINT`,
  `R8G8_UINT`, `R8G8B8A8_UINT`, `R8_SINT`, `R8G8_SINT`, and
  `R8G8B8A8_SINT` values, missing-channel defaults, and view lifetimes;
- unfused NIR `fmad` emitted as separately decorated SPIR-V multiply and add
  operations.  A varying-fed fragment expression exercises this path and
  matches the exact softpipe result; the multiply is explicitly
  `NoContraction`, rather than accidentally changing `fmad` into fused `ffma`
  semantics;
- native WebGPU fragment coordinates at exact half-integer pixel centers.  A
  browser-only boundary case encodes X and Y across every covered pixel and
  checks exact RGBA8 values.  This is intentionally not labeled a softpipe
  differential: softpipe advertises integer native centers, and its Wasm
  NIR-to-TGSI path incorrectly broadcasts the first quad lane's X value in the
  corresponding hand-built test.  An st/mesa-level GL test or another native
  Mesa oracle is still required for the frontend transformation;
- derivative-driven mip selection.  A fullscreen vertex varying is scaled so
  its implicit fragment derivatives select exact LOD 1 from red/green/blue
  mip levels, producing uniform green.  A separate `txd` shader supplies
  explicit `(0.5,0)` and `(0,0.5)` gradients and selects the same green mip.
  Both paths execute after dropping the application texture reference;
- a single-sample `Z32_FLOAT` attachment, full depth clear, `LESS` comparison,
  and depth writes.  After GL half-Z lowering, a farther red fullscreen draw is
  rejected behind green while a nearer red draw replaces it, with exact color
  readback after each transition.  The complete Gallium compare-function map is
  present, with additional exact `GREATER` and disabled-depth-write gates;
- a single-sample packed `Z24_UNORM_S8_UINT` attachment with combined
  depth/stencil clear and dynamic stencil reference.  An initial `0xA0`
  stencil value receives a low-nibble-only `REPLACE` with reference `0x05`;
  full-byte `EQUAL` then accepts reference `0xA5` and rejects `0x05`, proving
  exact reference propagation, write-mask preservation, and compare-mask
  behavior without relying on unavailable stencil readback;
- standard source-alpha/one-minus-source-alpha RGB blending with independent
  alpha factors, producing exact `160,64,96,255` over the UNORM clear color,
  additive `ONE,ONE`, reverse subtract, and dynamic constant-color blending,
  all against native-Mesa expected values, plus a red-only color write mask
  preserving the other destination channels;
- up to eight contiguous `RGBA8_UNORM` color attachments.  A two-output
  fragment shader writes exact red and cyan into locations zero and one, with
  each target read back independently.  A second draw enables independent
  target state: target zero uses a red-only write mask while target one uses an
  additive `ONE,ONE` blend and a green/blue-only mask.  Both exact results match
  softpipe, and rebinding the original one-target framebuffer proves stale MRT
  attachments are released;
- front/back face culling with both clockwise and counterclockwise Gallium
  fronts under positive and negative viewport Y scale.  Exact asymmetric masks
  prove the required inversion between Gallium framebuffer-space winding and
  WebGPU `frontFace`, including the Y-flipped vertex variant;
- BGRA texture-view swizzle lowered by the same sampler/view-keyed NIR variant
  mechanism.  Original Mesa NIR is retained as the variant source; neither
  application GLSL nor generated WGSL is rewritten.  Variants are retained in
  a per-fragment-shader cache keyed by the complete Gallium sampler state and
  view swizzle, so alternating material state does not trigger recompilation;
- checked 64-bit row/layer-size arithmetic before narrowing to Wasm `size_t`.
  A browser regression supplies a two-row upload with `UINT32_MAX` stride and
  only four bytes of source storage, then proves that the context is poisoned
  fail-closed without issuing a WebGPU copy or exposing a readable transfer.
  A separate context submits a two-layer box to a one-layer texture and proves
  the new layered path also poisons fail-closed before a subsequent map.

All Gallium capability values remain zero.  Therefore this proof does not yet
authorize st/mesa to create an OpenGL context on the WebGPU driver, and it says
nothing about STK correctness.  The next proofs must add asymmetric coverage,
varying interpolation, the st/mesa fragment-coordinate transformation,
viewport clipping outside the framebuffer,
viewport swizzles, multiple viewports,
reversed depth ranges and clip-depth boundary cases, the remaining vertex
formats/rates/index modes, and the remaining texture formats, border blending
across implicit/mip-filtered LOD, cube arrays, and shadow
sampling variants before those capabilities are advertised.

Normalized-coordinate integer sampling needs a semantic lowering, not just
WebGPU format table entries, because WebGPU integer textures are load-only.
The first fail-closed subset now lowers a 2D integer `textureLod` operation to
`txs` plus clamped integer `txf`/`textureLoad`: nearest minification and
magnification, no mip filter, clamp-to-edge, an identity-swizzled single-level
view, and explicit or implicit LOD.  For a single-level view, implicit LOD is
semantically independent of derivatives because every LOD clamps to that one
level.  Shared softpipe/WebGPU cases select exact texels
inside 2x2 `RGBA8_UINT` and `RGBA8_SINT` textures, preserve signed component
values through the lowering, clamp a negative coordinate, and prove the exact
`1.0` upper boundary clamps to the final texel.  A single-level view whose base
and last level are both one also returns its exact texel, proving that generated
LOD zero is relative to the bound view.  Repeat wrapping is lowered with
fractional coordinates before texel conversion, while mirrored repeat uses a
two-unit triangle wave.  Negative coordinates chosen to distinguish clamp,
repeat, and mirror produce the exact expected texels in both oracles.  The
typed lowering selects bit-exact signed or unsigned integer border vectors for
out-of-range coordinates; float-encoded border state is rejected for integer
textures.  Explicit LOD on a multi-level view clamps to sampler and view ranges,
rounds nearest-mipmap selection with
`floor(lod + 0.5)`, and uses that dynamic level for all dimensions and texel
load.  Exact cases straddle the 0.5 boundary and independently exercise minimum
and maximum LOD clamps.  Implicit multi-level selection uses the same 2D lambda
approximation as softpipe: the maximum absolute coordinate derivative scaled by
the base-level dimensions, followed by `log2`, range clamps, and nearest-mip
rounding.  A varying-fed fullscreen case selects exact level one in both
oracles.  Typed view swizzles are applied after border selection, including
integer `ZERO` and `ONE`; an exact BGR/ONE case proves that `ONE` remains the
integer value one rather than normalized full scale.  Two-layer 2D arrays use
normalized S/T with rounded, clamped integer layer selection, with exact cases
on both sides of 0.5 and above the final layer.  A two-slice 3D resource proves
allocation, full-volume upload, full-depth view lifetime, and samplerless
integer texel fetch from the second slice.  Normalized integer 3D sampling then
proves R-coordinate clamp-to-edge, repeat, and mirrored-repeat selection against
softpipe and WebGPU.  A two-level 3D view also selects exact level-zero and
level-one texels on opposite sides of the nearest-mipmap 0.5 boundary.  A
varying whose only nonzero spatial derivative is R selects level one in both
oracles, proving that implicit 3D LOD includes depth rather than reusing a 2D
lambda.  Typed integer clamp-to-border in R selects the exact unsigned border
vector for an out-of-range coordinate.  Other state is rejected before pipeline
creation.  Sampler LOD bias is represented in shader variants because WebGPU
sampler descriptors have no corresponding state.  Exact float cases prove
positive and negative bias on implicit LOD, bias addition to explicit LOD,
shader-bias plus sampler-bias composition, alternating bias-state cache rebinds,
and the Gallium rule that explicit gradients ignore sampler bias.  An unsigned
integer case proves bias before nearest-mip rounding in the load-based lowering.
Bias is currently accepted in the finite range [-15, 15]; shadow sampling with
nonzero bias remains fail-closed.  Because WGSL has no filtered sampling
overloads for `texture_1d`, logical Gallium 1D resources are represented as
WebGPU 2D textures with height one.  Compile-time NIR lowering pads coordinates,
offsets, gradients, fetches, and size queries to 2D and rewrites the texture
descriptor declaration to the matching 2D type.  Exact float and unsigned
integer cases prove upload/view lifetime, repeat wrapping, explicit mip
selection, and integer implicit derivative LOD.  Logical 1D arrays use 2D-array
views with physical height one.  Gallium encodes their layer range in upload
`box.y/height`, so the backend translates that range to WebGPU array-layer
origin/extent rather than treating it like a 2D-array `box.z/depth`.  Exact
float and unsigned-integer cases prove layer-one normalized sampling and
resource/view lifetime through this translation.  Rectangle resources are
physical WebGPU 2D textures with one mip level.  Their Gallium sampler must set
`unnormalized_coords`; Mesa's NIR rectangle lowering divides logical pixel
coordinates by an exact texture-size query before normal WebGPU sampling.
Exact 2x2 cases select pixel coordinate (1.5, 0.5) for float and unsigned
integer formats, with the integer path continuing through load-based normalized
sampling.  Float RECT is checked against softpipe.  The wasm softpipe TGSI
interpreter has no valid integer-RECT `exec_tex` dispatch, so that exact integer
case currently runs only against real browser WebGPU and remains an explicitly
recorded oracle gap.  BC texture support is requested only when the adapter
exposes WebGPU `texture-compression-bc`, and the Gallium format query stays
closed otherwise.  The complete S3TC family maps to BC1/BC2/BC3 views,
including linear and sRGB variants.  Because WebGPU has no RGB-only BC1
format, DXT1 RGB views use a sampler-keyed NIR variant that composes the
logical format swizzle and forces alpha to one.  An exact three-color-mode
block catches the otherwise observable transparent-alpha mismatch.  Upload
validation uses format
block width, height, and byte size rather than pretending these formats have a
linear bytes-per-pixel layout; origins must be block aligned, partial blocks
are accepted only at a logical mip edge, and the WebGPU copy extent is rounded
to the physical block footprint.  Mesa's CPU decoder checks every exact S3TC
block, including a nontrivial RGB565 endpoint that distinguishes linear output
`(132,130,132)` from sRGB-decoded output `(59,57,59)`, and the browser WebGPU
path reproduces every result through a complete Gallium draw.  A browser-only
explicit-LOD case uploads a
2x2 logical mip through one complete BC block and samples its exact green
result, while a malformed unaligned update proves fail-closed behavior.  BC2
explicit-alpha decoding and DXT1 RGB opaque-alpha semantics are independently
checked against Mesa's CPU format decoder and through complete browser WebGPU
draws.  Exact BC1 layered cases additionally select layer one of a 2D array,
the negative-Z cube face, and the positive-Z face of a rebased second cube in a
cube-array view.  wasm softpipe's TGSI compressed sampler is nondeterministic
when formats are mixed and compressed explicit-LOD sampling traps in
`exec_tex`, so no compressed draw is falsely labeled a softpipe differential.
The same feature gate now covers RGTC BC4/BC5 and BPTC BC6H/BC7.  Exact BC4 and
BC5 blocks prove one- and two-channel UNORM/SNORM endpoint interpretation,
including a negative signed channel that clamps only at the RGBA8 render
target.  Mesa's BPTC encoder produces uniform signed-float, unsigned-float,
linear-UNORM, and sRGB blocks; its independent decoder supplies the sampled
reference.  Browser draws check BC6H within two RGBA8 codes (after an explicit
scale/bias makes the signed negative channel visible) and BC7 within one code.
The BPTC tests use distinct compiled shader states for transformed and plain
sampling, preventing variant state from leaking between the signed and
unsigned cases.
Integer cube targets are
represented by a second 2D-array view of the same six-layer WebGPU allocation,
because WebGPU forbids integer cube views.  Mesa's integer-cube NIR lowering
was generalized to recognize separate texture and bare-sampler descriptors;
it projects the direction to normalized face coordinates before the driver's
integer normalized-sampling lowering emits array texel loads.  One exact
softpipe/WebGPU case validates all six face indices and an off-axis top-left
texel on every face of a uniquely populated 2x2 cube, catching face and axis
orientation errors while retaining the native WebGPU cube view for float
sampling.  Cube arrays require complete, six-layer-aligned views.  Exact float
and unsigned-integer cases select the +Z top-left texel in the second cube: the
float case uses a native `CubeArray` view rebased at layer six with cube index
zero, while the integer case uses a full `2DArray` alias and cube index one.
Seven-layer resources and misaligned views are rejected at the WebGPU boundary.
Implicit integer cube LOD and cube gather remain fail-closed until their own
differential cases exist.

The shader path also has an explicit WebGPU SPIR-V policy: ordinary fragment
output zero omits Vulkan's optional `Index 0` decoration.  Tint otherwise
correctly interprets the decoration as dual-source blending.  This distinction
is made while emitting SPIR-V; generated WGSL is never patched.  The same
policy surface emits `fsat` as `GLSL.std.450 FClamp` and fused multiply-add as
core `GLSL.std.450 Fma`, avoiding WebGPU-incompatible `SPV_KHR_fma` without
changing Zink's default Vulkan emission.
