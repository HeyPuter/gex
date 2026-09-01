# The OpenGL ceiling on WebGPU

**Question.** Given WebGPU's API surface as Chrome exposes it, plus the hardware
in this machine, how far up the OpenGL version ladder can `gex_wgpu` be pushed
*in principle* -- not "what works today", but where the wall actually is.

**Method.** Mesa decides the reported GL version in one place,
`src/mesa/main/version.c:254-400`. That function is the authoritative
checklist: a conjunction of extension flags and numeric constants per rung. So
the analysis walks that ladder, and for each requirement asks which of three
categories it falls into:

| | meaning |
|---|---|
| **direct** | a WebGPU/WGSL primitive maps to it |
| **lowerable** | no primitive, but the driver generates every shader and can express it in shader code or by restructuring submissions |
| **blocked** | no expression exists at any cost -- a WebGPU validation rule or a hard numeric limit stands in the way |

Only **blocked** sets a ceiling. "Lowerable" may be absurdly expensive and still
counts, because the question is theoretical reach.

Numbers below are measured on this machine, not quoted from a spec.

---

## Baseline: what this machine actually offers

Chromium 151.0.7922.108, AMD Radeon RX 6500 XT (RADV NAVI24, Vulkan 1.4.354),
reached with `--use-angle=vulkan --enable-features=Vulkan`. (The default
headless configuration gets SwiftShader instead; see
`docs/gl-webgpu-handoff.md` and the note at the end of this file.)

### Adapter features

```
bgra8unorm-storage          float32-blendable        subgroups
clip-distances              float32-filterable       texture-component-swizzle
core-features-and-limits    indirect-first-instance  texture-compression-bc
depth-clip-control          primitive-index          texture-compression-bc-sliced-3d
depth32float-stencil8       rg11b10ufloat-renderable texture-formats-tier1
dual-source-blending        shader-f16               texture-formats-tier2
                                                     timestamp-query
```

### Adapter limits

```
maxBindGroups                              4     maxStorageBufferBindingSize        4294967292
maxBindGroupsPlusVertexBuffers            24     maxStorageBuffersInFragmentStage           16
maxBindingsPerBindGroup                 1000     maxStorageBuffersInVertexStage             16
maxBufferSize                     4294967292     maxStorageBuffersPerShaderStage            16
maxColorAttachmentBytesPerSample         128     maxStorageTexturesInFragmentStage           8
maxColorAttachments                        8     maxStorageTexturesInVertexStage             8
maxComputeInvocationsPerWorkgroup       1024     maxStorageTexturesPerShaderStage            8
maxComputeWorkgroupSizeX                1024     maxTextureArrayLayers                    2048
maxComputeWorkgroupSizeY                1024     maxTextureDimension1D                   16384
maxComputeWorkgroupSizeZ                  64     maxTextureDimension2D                   16384
maxComputeWorkgroupStorageSize         65536     maxTextureDimension3D                    2048
maxComputeWorkgroupsPerDimension       65535     maxUniformBufferBindingSize             65536
maxDynamicStorageBuffersPerPipeline        8     maxUniformBuffersPerShaderStage            12
maxDynamicUniformBuffersPerPipeline       10     maxVertexAttributes                        30
maxImmediateSize                          64     maxVertexBufferArrayStride               2048
maxInterStageShaderVariables              28     maxVertexBuffers                            8
maxSampledTexturesPerShaderStage          48     minStorageBufferOffsetAlignment           256
maxSamplersPerShaderStage                 16     minUniformBufferOffsetAlignment           256
```

**A device only gets what it asks for.** Anything absent from `requiredLimits`
comes back at WebGPU's *default*, not the adapter's. Measured with the repo's
current request list on this adapter:

```
maxTextureDimension2D             adapter 16384 -> device 8192
maxTextureArrayLayers             adapter  2048 -> device  256
maxVertexAttributes               adapter    30 -> device   16
maxSampledTexturesPerShaderStage  adapter    48 -> device   16
```

Every limit quoted in this document is the *adapter* number, i.e. the ceiling
available if asked for.

---

### The WGSL surface, enumerated

Tint reports its own vocabulary when handed a bogus name, so these lists are
exhaustive for this browser rather than recalled from a spec.

**`enable` extensions available:** `atomic_vec2u_min_max`, `clip_distances`,
`dual_source_blending`, `f16`, `primitive_index`, `subgroup_size_control`,
`subgroups`.

**`@builtin` values accepted:**

```
barycentric_coord*  frag_depth        instance_index          num_subgroups
clip_distances      front_facing      local_invocation_id     num_workgroups
position            global_invocation_id                      primitive_index
sample_index        global_invocation_index                   sample_mask
subgroup_id         local_invocation_index                    subgroup_size
vertex_index        workgroup_id      workgroup_index
```

`*` `barycentric_coord` parses but its extension
(`chromium_experimental_barycentric_coord`) "is not allowed in the current
environment" -- it is not reachable without a Chrome flag.

**Conspicuously absent:** `layer`, `viewport_index`, `cull_distance`,
`point_size`, `draw_index` / `base_vertex` / `base_instance`,
`helper_invocation`.

**`@interpolate` sampling values:** `center`, `centroid`, `either`, `first`,
`sample`. **There is no `last`.**

### Directly measured WGSL/API facts

| probe | result |
|---|---|
| `@builtin(primitive_index)` (with `enable`) | **OK** |
| `@builtin(sample_mask)`, `@interpolate(..., sample)` | OK |
| `dpdxFine` / `dpdxCoarse` / `dpdyFine` / `dpdyCoarse` | OK |
| const texel offset `-8` and `+7` | OK (exactly GL's required range) |
| `textureGather` with offset | OK |
| `subgroupAny` / `subgroupAll` / `subgroupBallot` | OK |
| read-write storage texture, `r32uint` and `rgba8unorm` | OK |
| `f16` | OK |
| sampler `maxAnisotropy: 16` | OK |
| sampler `mirror-clamp-to-edge` | **rejected** |
| sampler `clamp-to-border` | **rejected** |
| `f64` | **no such type** |
| `textureQueryLod` | **no such function** |
| texture atomics (`texture_storage_2d<r32uint, atomic>`) | **not a thing** |
| `@builtin(layer)` / `@builtin(viewport_index)` | **not builtins** |
| `@interpolate(flat, last)` | **not a sampling mode** |

### Inter-stage capacity, measured

The binding constraint for fragment inputs. Built pipelines with N `vec4`
inter-stage variables and found the largest N that validates:

| configuration | max `vec4` slots | components |
|---|---|---|
| plain | 28 | **112** |
| + `@builtin(front_facing)` | 27 | 108 |
| + `@builtin(sample_index)` | 27 | 108 |
| + `clip_distances[4]` | 27 | 108 |
| + `clip_distances[8]` | 26 | 104 |
| + `front_facing` + `sample_index` + `clip_distances[8]` | 26 | 104 |

The CTS requires `GL_MAX_FRAGMENT_INPUT_COMPONENTS >= 128`
(`external/openglcts/modules/gl/gl4cLimitsTests.cpp:157`). **112 < 128**, and
clip distances or a `front_facing` read push it to 104.

---

## The verdict

**GL 4.6 is theoretically reachable. Nothing on the ladder is provably
impossible.** But that sentence hides all the work, so the useful answer is
tiered by the machinery each rung demands.

The organising fact: **WebGPU has no programmable geometry stage, and its
vertex stage cannot write memory.** Measured -- a `var<storage, read_write>`
"cannot be used by vertex pipeline stage". So every GL feature that needs to
observe or reshape primitives between vertex and fragment must be re-run as a
*compute* pass over the vertex work. That single piece of machinery is what
transform feedback (GL 3.0), geometry shaders (GL 3.2), tessellation (GL 4.0),
`gl_Layer` / `gl_ViewportIndex` (GL 4.1 / 4.3), cull distance (GL 4.5) and the
provoking-vertex convention (GL 3.2) all reduce to. Build it once and six
requirements fall together. Build none of it and the ceiling is **GL 2.1** --
because the very first rung above 2.1 already requires it.

| tier | machinery | ceiling | state in this tree |
|---|---|---|---|
| **0** | direct mapping only | **GL 2.1** | -- |
| **1** | + shader lowering, index rewriting, submission restructuring | **GL 2.1** (3.0 is gated on transform feedback) | -- |
| **2** | + compute pass re-running vertex work into storage buffers (**transform feedback**) | **GL 3.1** | **built** |
| **3** | + **geometry shaders** emulated in compute | **GL 3.3** | **built** (GS); 3.3 gated on features, not machinery |
| **4** | + a **tessellator written in WGSL**, Mesa's software fp64, LOD computed by hand | **GL 4.0** | not started |
| **5** | + per-viewport and per-layer primitive binning, cull distance, vertex pulling | **GL 4.3** | not started |
| **6** | + texture aliasing by buffer round-trip, image atomics via storage buffers, manual attribute interpolation | **GL 4.6** | not started |

Tier 1 buying nothing over tier 0 is not a rounding error -- it is the finding.
`version.c` puts `EXT_transform_feedback` in the GL 3.0 conjunct, and with the
vertex stage unable to write memory there is no way to capture varyings without
a second, compute-shaped execution of the vertex work.

This is not speculative for this codebase: the driver **already** implements
transform feedback as a compute dispatch (`gex_wgpu_screen.c:7326-7341`, the
stream-output bindings are `WGPUShaderStage_Compute`) and **already** emulates
geometry shaders the same way -- `gex_wgpu_screen.c:9211` says so outright:
*"the geometry stage is emulated by running the shader as compute ...
Tessellation still has no emulation and so is still absent."* Tiers 2 and 3
exist. Tier 4 is the next real build.

## Rung by rung

Each row is a literal conjunct of `version.c`. `direct` / `lowerable` /
`blocked` as defined above.

### GL 3.0 -- reachable

Everything is direct except one item. Formats all present on this adapter:
`rgba32float` sampleable+blendable (`float32-filterable`, `float32-blendable`),
`depth32float`, `rg11b10ufloat` + `rg11b10ufloat-renderable`, `rgb9e5ufloat`,
BC4/BC5 via `texture-compression-bc`, RG formats, integer textures, sRGB
framebuffers, 2D arrays. `MaxColorAttachments` 8 >= 4, `MaxSamples` 4 >= 4.

| requirement | verdict |
|---|---|
| `EXT_transform_feedback` | **lowerable, and it must be a compute pass.** No WebGPU XFB, and -- measured -- a `var<storage, read_write>` "cannot be used by vertex pipeline stage", so you cannot simply bolt a capture write onto the real vertex shader. The vertex work has to be *re-run* in compute, writing varyings to a storage buffer. (Vertex-stage storage *reads* are fine, which is what makes vertex pulling viable later.) `PRIMITIVES_GENERATED` / `TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN` from atomics; fragment-stage storage writes and atomics both work. |
| `NV_conditional_render` | direct (occlusion query set; already implemented) |
| everything else | direct |

### GL 3.1 -- reachable

| requirement | verdict |
|---|---|
| `ARB_uniform_buffer_object` | direct-ish: `maxUniformBuffersPerShaderStage` 12, GL wants 12 here and 14 at 4.3; overflow already goes to storage buffers |
| `EXT_texture_snorm` | direct **only on hardware** -- 16-bit snorm needs `texture-formats-tier1` |
| `NV_primitive_restart` | lowerable -- WebGPU restart is fixed-index and strip-only; rewrite the index buffer |
| `NV_texture_rectangle` | lowerable -- unnormalized coords in shader (already done) |
| vertex `MaxTextureImageUnits >= 16` | direct (`maxSampledTexturesPerShaderStage` 48) |

**This is the ceiling without a compute geometry pipeline.**

### GL 3.2 -- needs geometry shaders

| requirement | verdict |
|---|---|
| GLSL 150 => **geometry shaders** | **lowerable, already done here** -- no WebGPU GS at any tier; this driver runs the GS as compute, emits to a storage buffer and draws from it (`gex_wgpu_screen.c:9211`). |
| `EXT_provoking_vertex` | **lowerable** -- WGSL `@interpolate` sampling offers `first` and `either`; **there is no `last`**, and `last` is GL's default. Fix by cyclically rotating each primitive's indices (rotation preserves winding). |
| `ARB_seamless_cube_map` | lowerable -- WebGPU cube sampling is *always* seamless; GL's default is *off*. Non-seamless needs manual face projection + per-face clamp (the driver already has that path for integer cubes). |
| `ARB_depth_clamp` | direct (`depth-clip-control`) |
| `ARB_texture_multisample` | direct (sampleCount 4; measured: only 1 and 4 exist, which is conformant since GL may round up) |
| `ARB_fragment_coord_conventions`, `EXT_vertex_array_bgra` | lowerable in shader |
| `ARB_sync`, `ARB_draw_elements_base_vertex` | direct |

### GL 3.3 -- reachable

| requirement | verdict |
|---|---|
| `ARB_blend_func_extended` | **direct** -- `dual-source-blending` is present and a real `@blend_src(0)/@blend_src(1)` pipeline with `src1`/`one-minus-src1` compiles clean on this adapter |
| `ARB_timer_query` | direct (`timestamp-query`) |
| `ARB_instanced_arrays` | lowerable -- WebGPU `stepMode: instance` advances once per instance; arbitrary divisors need `arrayStride: 0` plus one draw per divisor group, or vertex pulling |
| `ARB_vertex_type_2_10_10_10_rev` | direct (`unorm10-10-10-2`) / shader unpack |
| `EXT_texture_swizzle` | direct (`texture-component-swizzle`) |
| rest | direct |

### GL 4.0 -- the heaviest rung

| requirement | verdict |
|---|---|
| `ARB_tessellation_shader` | **lowerable; the single largest unbuilt item.** WebGPU has no tessellator *and no hardware one to drive*, so unlike GS this is not "run the user's shader as compute" -- you must also write the fixed-function tessellator itself in WGSL. `MAX_TESS_GEN_LEVEL` 64, `MAX_PATCH_VERTICES` 32. The driver's own note calls it out: *"Tessellation still has no emulation and so is still absent."* |
| `ARB_gpu_shader_fp64` | **lowerable, and already written** -- WGSL has no `f64` (confirmed: "unresolved type 'f64'"), but Mesa ships `src/compiler/glsl/float64.glsl` plus `nir_lower_doubles(shader, softfp64, ...)`, used in production by Intel's compiler. Set `caps.doubles` and run the pass. |
| `ARB_texture_query_lod` | **lowerable** -- confirmed no `textureQueryLod` in WGSL. The LOD formula is fully specified; compute it from `dpdx`/`dpdy` in shader. |
| `ARB_gpu_shader5` | mixed. `fma` direct; `textureGatherOffsets` (4 offsets) = 4 gathers; `gl_SampleID`/`gl_SampleMask` direct; dynamically-uniform sampler indexing = a switch over N bindings. **`interpolateAtOffset`/`interpolateAtSample` have no WGSL equivalent** -- needs manual interpolation (see below). |
| `ARB_sample_shading` | direct -- `@builtin(sample_index)` forces per-sample invocation; `glMinSampleShading` fractions may be rounded up, which GL permits |
| `ARB_transform_feedback2/3` | lowerable, on the XFB emulation; `MAX_VERTEX_STREAMS >= 4` |
| `ARB_draw_buffers_blend`, `ARB_draw_indirect`, `ARB_texture_cube_map_array` | direct |
| `ARB_texture_buffer_object_rgb32` | lowerable -- storage buffer; `MAX_TEXTURE_BUFFER_SIZE >= 65536` easily met |

### GL 4.1 -- every limit met exactly, and only on hardware

`version.c` demands five numeric constants here. This adapter meets all five;
SwiftShader misses three, which is why GL 4.1+ is *structurally* impossible on
the software path no matter how much driver work happens.

| constant | required | adapter | SwiftShader |
|---|---|---|---|
| `MaxTextureSize` | 16384 | **16384** | 8192 |
| `MaxRenderbufferSize` | 16384 | **16384** | 8192 |
| `MaxCubeTextureLevels` | 15 (=16384) | **16384** | 8192 |
| `Max3DTextureLevels` | 12 (=2048) | **2048** | 2048 |
| `MaxArrayTextureLayers` | 2048 | **2048** | 256 |

Note how many of these are met *exactly*. There is no headroom.

| requirement | verdict |
|---|---|
| `ARB_viewport_array` | **lowerable** -- `MAX_VIEWPORTS >= 16`, and there is no `@builtin(viewport_index)` (confirmed absent from Tint's builtin list). Bin primitives by viewport in the emulated geometry stage, one render pass per viewport. |
| `ARB_vertex_attrib_64bit` | lowerable on softfp64 + manual attribute assembly |
| `ARB_shader_precision` | a conformance claim about ULP bounds; RADV should satisfy it |
| `ARB_ES2_compatibility` | direct |

### GL 4.2 -- reachable

| requirement | verdict |
|---|---|
| `ARB_shader_image_load_store` | mixed. Storage textures direct, and **read-write storage textures work for both `r32uint` and `rgba8unorm`** (measured). But **WGSL has no texture atomics** (`texture_storage_2d<r32uint, atomic>` does not parse), so `imageAtomic*` must route through a storage buffer with manual address arithmetic, synced to the texture at `glMemoryBarrier` points. `MAX_IMAGE_UNITS >= 8` vs `maxStorageTexturesPerShaderStage` 8 -- exact. |
| `ARB_shader_atomic_counters` | direct (storage-buffer atomics) |
| `ARB_conservative_depth` | direct -- it is a hint; ignoring the optimisation is conformant |
| `ARB_texture_compression_bptc` | direct (BC6H/BC7) |
| `ARB_base_instance` | direct (`firstInstance`, `indirect-first-instance`) |
| rest | direct |

### GL 4.3 -- the ugliest rung

| requirement | verdict |
|---|---|
| `ARB_ES3_compatibility` | **direct only on hardware.** Mesa's gate (`st_extensions.c:1668`) needs `R16_UNORM`, `RG16_UNORM`, `R16_SNORM`, `RG16_SNORM` sampleable. Measured: all four **unavailable on SwiftShader**, all four **available on hardware under `texture-formats-tier1`**. The same gate's comment reads "Requirements for ETC2 *emulation*" -- no native ETC2 format is ever needed. |
| `ARB_texture_view` | **lowerable, expensively.** WebGPU `viewFormats` permits only sRGB reinterpretation. Measured rejections: `rgba8unorm`->`rgba8uint`, `rgba8unorm`->`r32uint`, `rgba32float`->`rgba32uint`, `r32float`->`r32uint`. GL view classes need real bit-aliasing. Escape: `copyTextureToBuffer` then `copyBufferToTexture` preserves the byte pattern across formats, so a view can be materialised as a separate texture and re-synced at bind transitions. Simultaneous read-through-one/write-through-other is undefined in GL anyway. |
| `ARB_fragment_layer_viewport` | **lowerable** -- no `@builtin(layer)` either. Rendering to a specific array layer works (`baseArrayLayer` view, measured OK), so layered rendering is one pass per layer with primitives binned in the emulated geometry stage. |
| `MAX_VERTEX_ATTRIB_BINDINGS >= 16` | **lowerable** -- `maxVertexBuffers` is **8**. Programmable vertex pulling from storage buffers sidesteps the vertex-buffer limit entirely (`maxVertexAttributes` is 30, comfortably over GL's 16). |
| vertex `MaxUniformBlocks >= 14` | lowerable -- 12 native + storage-buffer overflow (already the driver's approach) |
| `ARB_framebuffer_no_attachments` | lowerable -- measured: a render pass with **zero** attachments is rejected, but a **depth-only pass with no colour attachments is accepted**, so the throwaway can be a depth texture at the declared default size |
| `ARB_copy_image` | lowerable -- same buffer round-trip for cross-format copies |
| `ARB_robust_buffer_access_behavior` | direct -- WebGPU is robust by construction |
| `ARB_compute_shader`, `ARB_shader_storage_buffer_object`, `ARB_stencil_texturing`, `ARB_texture_buffer_range`, `ARB_texture_query_levels`, `ARB_shader_image_size`, `ARB_arrays_of_arrays`, `ARB_explicit_uniform_location`, `ARB_internalformat_query2` | direct |

Compute limits all clear GL's minimums: invocations/workgroup 1024 = 1024,
shared memory 65536 >= 32768, workgroups/dimension 65535 = 65535.

### GL 4.4 -- reachable

| requirement | verdict |
|---|---|
| `ARB_texture_mirror_clamp_to_edge` | **lowerable** -- measured: the sampler address mode is rejected (as is `clamp-to-border`). The driver generates every shader, so the wrap can be folded into coordinate math; take derivatives before wrapping to keep implicit LOD correct. |
| `ARB_buffer_storage` | **lowerable** -- GL persistent+coherent mapping has no WebGPU analogue (a mapped buffer is unusable by the GPU). Keep the "mapped" pointer in wasm memory and upload the shadow before each submit. Correct, and slow. |
| `ARB_query_buffer_object` | direct (`resolveQuerySet` writes into a GPU buffer) |
| `ARB_texture_stencil8` | direct (measured: standalone `stencil8` texture OK) |
| `ARB_vertex_type_10f_11f_11f_rev` | lowerable -- uint attribute + shader unpack |
| `MaxVertexAttribStride >= 2048` | direct -- `maxVertexBufferArrayStride` is **exactly** 2048 |
| `ARB_enhanced_layouts` | lowerable, on XFB emulation + component packing |

### GL 4.5 -- reachable

| requirement | verdict |
|---|---|
| `ARB_cull_distance` | **lowerable** -- WGSL has `clip_distances` capped at exactly 8 (measured: `array<f32, 9>` is rejected, "N <= 8") and no cull distance at all. GL wants `MAX_CLIP_DISTANCES >= 8`, `MAX_CULL_DISTANCES >= 8`, `MAX_COMBINED_CLIP_AND_CULL_DISTANCES >= 8`. That works out only because cull is emulated *outside* the clip slots: "drop the primitive if all its vertices are negative" is a one-line test in the emulated geometry stage, consuming none of the 8. |
| `ARB_clip_control` | lowerable -- WebGPU is fixed at upper-left origin and [0,1] depth; both conventions are a vertex-shader transform |
| `NV_texture_barrier` | lowerable -- end the render pass and begin another; that *is* the barrier |
| `ARB_derivative_control` | direct (`dpdxFine`/`dpdxCoarse`/`dpdyFine`/`dpdyCoarse` all compile) |
| `ARB_shader_texture_image_samples` | direct (a uniform) |
| `ARB_ES3_1_compatibility`, `ARB_conditional_render_inverted` | direct |

### GL 4.6 -- reachable

| requirement | verdict |
|---|---|
| `ARB_shader_group_vote` | **direct** -- `subgroups` is present and `subgroupAny`/`subgroupAll`/`subgroupBallot` all compile |
| `ARB_texture_filter_anisotropic` | direct (`maxAnisotropy: 16` accepted) |
| `ARB_polygon_offset_clamp` | direct (`depthBiasClamp`) |
| `ARB_gl_spirv` + `ARB_spirv_extensions` | lowerable -- ingest with Mesa's own `spirv_to_nir`; the driver already runs NIR -> SPIR-V -> Tint outbound |
| `ARB_indirect_parameters` | lowerable -- no indirect *count*. A compute pass writes N indirect structs and zeroes `instanceCount` past the real count, then issue N `drawIndirect`s. |
| `ARB_shader_draw_parameters` | lowerable -- no `draw_index`/`base_vertex`/`base_instance` builtins; pass them as per-draw immediate data (`maxImmediateSize` is 64 bytes, i.e. push constants are available) rather than rebinding a uniform, since the driver already loops over draws |
| `ARB_shader_atomic_counter_ops` | direct |
| `ARB_transform_feedback_overflow_query` | lowerable, on the XFB emulation's atomics |

---

## The one genuine numeric shortfall

`GL_MAX_FRAGMENT_INPUT_COMPONENTS` must be **>= 128**
(`gl4cLimitsTests.cpp:157`). This adapter's `maxInterStageShaderVariables` is
**28**, i.e. **112 components**, dropping to **104** once a shader reads
`front_facing` or declares `clip_distances[8]`. Measured, not inferred.

This is the only requirement on the whole ladder with no comfortable answer.
It is still not *blocked*, because of one specific feature this adapter has:

**`@builtin(primitive_index)` works** (confirmed: compiles with `enable
primitive_index;`, and `primitive-index` is in the adapter's feature list).
With it, a fragment shader can read the primitive's vertices out of a storage
buffer, derive barycentrics from `@builtin(position)`, and interpolate any
number of attributes by hand -- perspective-correct, since the formula is
specified. The post-transform vertex data is already sitting in a storage
buffer if the compute geometry pipeline exists, so this composes with tier 3
rather than adding new machinery.

The same trick is the answer for `interpolateAtOffset` / `interpolateAtSample`
in `ARB_gpu_shader5`, which likewise have no WGSL form.

Chrome does have a real barycentric builtin -- Tint knows
`@builtin(barycentric_coord)` -- but its extension
`chromium_experimental_barycentric_coord` reports "not allowed in the current
environment", so it is not reachable here without a flag. If it were exposed,
the manual-interpolation path would get dramatically cheaper.

---

## Things that are genuinely lost, outside the version ladder

Not gated by `version.c` and not tested by the GL CTS, but real:

- **`glLogicOp` / `GL_COLOR_LOGIC_OP`** is core GL state
  (`src/mesa/main/enable.c:710`) and WebGPU has no logic-op blend stage, no
  framebuffer fetch, and no rasteriser-ordered views. Emulating it through a
  read-write storage texture gives no ordering guarantee between overlapping
  primitives, so it is wrong in exactly the case people use XOR drawing for.
  The GL CTS under `external/openglcts/modules/gl` contains no logic-op tests,
  so this does not block a version claim -- it just does not work.
- **Only sample counts 1 and 4 exist** (2, 8, 16 all rejected). Conformant, since
  GL permits rounding up, but `glRenderbufferStorageMultisample(..., 2, ...)`
  silently becomes 4.

---

## Practical note: none of this is reachable in the default test setup

Every measurement above required
`--use-angle=vulkan --enable-features=Vulkan`. Both flags, together: each alone
falls back to SwiftShader. The repo's harness (`web/test/drive.mjs:273`) passes
`--use-angle=swiftshader-webgl` in headless, which pins Dawn to SwiftShader,
and `tests/gl44-cts-browser.sh` and `tests/mesa-wgpu-replay.sh` both route
through it.

And a device only receives limits it explicitly requests. With the repo's
current `requiredLimits` (`web/src/mesa-gl.js:85-90`), even on hardware the
device comes back with `maxTextureDimension2D` 8192 and `maxTextureArrayLayers`
256 -- below the GL 4.1 floor -- and without `dual-source-blending` or
`texture-formats-tier1` in `requiredFeatures`, GL 3.3 and GL 4.3 stay blocked
on a device that could serve both.

So the ordered list of what actually gates progress today:

1. Chrome flags -> a hardware adapter at all.
2. `requiredLimits` / `requiredFeatures` -> let the adapter's capability reach the device.
3. Driver reads those limits instead of hardcoding WebGPU's defaults
   (`gex_wgpu_screen.c:8851-8855`), and `max_dual_source_render_targets`
   stops being 0 (`:8859`).
4. Everything above, in ladder order.
