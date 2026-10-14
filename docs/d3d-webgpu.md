# Direct3D on WebGPU: plan and state (stream D, started 2026-10-14)

The aim is Windows 7's Aero, glass and all, in the page, through a Direct3D
implementation on WebGPU that serves any D3D program, not only the shell.
Today a D3D program under W3 runs wined3d as translated i386 code, onto
OpenGL, through gex-gl, onto Mesa compiled to wasm, onto the page's WebGPU
(docs/gl-path.md). Each layer costs: wined3d's GL query round trips, Mesa's
size and start-up, GL semantics WebGPU lacks. This replaces the middle of
that chain with WebGPU end to end.

## 1. Layers

```text
D3D8/9/10/11, DXGI (Wine's front ends, unchanged)
  -> wined3d with a WebGPU renderer (adapter_wgpu, context_wgpu,
     texture_wgpu, shader_wgsl, beside the GL and Vulkan ones)
       shaders: vkd3d-shader -> WGSL (new target)
  -> webgpu.dll (new Wine DLL): webgpu.h exports, the gex wire's client
       Unix side: the wire's server
         native host: on Dawn (Metal, Vulkan, D3D12), in-process
         W3: forwarded through /dev/gex-gpu (to do)
  -> page: the wire's server on the browser's WebGPU (to do)
```

Decisions and why:

- **WebGPU's own API as the wire, not a D3D-level protocol.** webgpu.h is a
  standard C API; anything (wined3d, a d3d12 layer, Windows programs that
  use Dawn, a future GL layer) can sit on it, and the page side stays a
  thin, generic replayer. The wire is gex's own (guest/gpu/), generated
  from Dawn's dawn.json: plain C for both ends (a Wine PE DLL of any
  architecture, wasm, native), objects as client-chosen ids so creating one
  needs no round trip, batched until a call needs a reply.
- **Shaders translated in the guest.** vkd3d-shader is already in Wine and
  already lowers D3D9's fixed-function bits (alpha test, fog, clip planes,
  point size) in its IR; a WGSL target gives every D3D shader model a path
  with no Tint and no SPIR-V in the page. The page compiles WGSL, which
  the browser does anyway.
- **wined3d, not a new D3D.** wined3d's front ends and state tracking are
  mature; its Vulkan backend (adapter_vk, context_vk, texture_vk,
  shader_spirv: about 10k lines) is the model, and WebGPU is close to a
  subset of Vulkan.

## 2. State

### 2.1 vkd3d-shader WGSL target (working, tested)

`libs/vkd3d-shader/wgsl.c` on vsir, modelled on `msl.c`; SM1-5 (d3dbc and
DXBC TPF) behind `VKD3D_SHADER_UNSUPPORTED_WGSL`, like MSL. Developed on
upstream vkd3d (2.1+git, branch `wgsl` in the working clone), to be carried
into the Wine fork's `libs/vkd3d` (2.0).

The shape of the output, and why (also in the file's header):
registers are `vec4<u32>` read through `bitcast<>` (WGSL has no unions);
masked writes are per component (no swizzle assignment); non-finite
immediates are made at run time (WGSL rejects them in const-expressions);
switch labels with no code between them share a clause (no fallthrough);
the derivative uniformity diagnostic is off (valid D3D breaks it). Without
interface info, descriptors take D3D11's model: group 0 CBVs, 1 SRVs,
2 samplers, 3 UAVs, the register as the binding; a texture compared against
has a depth view 128 bindings past its own.

**Tests.** `tests/shader_runner_webgpu.c` runs vkd3d's HLSL shader tests on
any webgpu.h (configure with `WEBGPU_CFLAGS`/`WEBGPU_LIBS`; Dawn's
prebuilt release on macOS, which runs on Metal). First full run, SM4.0-5.1:

| | |
|---|---|
| test files clean | 247 of 283 |
| checks executed | 6474 |
| checks failed | 220 (96.6% pass) |
| checks skipped | 752 (476 missing caps, 232 buffer views) |

What fails is what is not yet written: raw, structured and typed buffers,
UAVs, groupshared memory and atomics, GetDimensions/resinfo, attribute
evaluation, clip and cull distances.

Since: the rest of the target (raw, structured and typed-less buffers,
UAVs and storage textures, groupshared memory and atomics, resinfo and
bufinfo), SM1-3 (d3dbc) on the runner, and what wined3d needs: binding
reflection (`vkd3d_shader_wgsl_binding_info`, the module's bindings as a
bind group layout needs them), buffer shader parameters (clip planes,
point size, alpha reference, fog, bump mapping, from a uniform buffer),
`cnd`/`lit`/`sgn`, clip distances, arrayed I/O, integer attributes read as
float (D3D9's UBYTE4), every varying the pixel shader reads declared by
the vertex shader (WebGPU requires it), and 1D textures as 2D. Latest run,
SM2.0-5.1: 250 of 283 files clean, 11064 checks, 131 failing (98.8%),
identical through the wire. What fails is mostly WebGPU's: attribute
evaluation, point size, typed buffer views, cull distances, stencil export.

### 2.2 The wire and webgpu.dll (working)

`guest/gpu/`: `tools/gen_webgpu_wire.py` generates the encoders, decoders
and dispatch from `third_party/dawn-api/dawn.json`; `wire-client.c` (ids,
batching, futures and events, mapped-range shadows) and `wire-server.c`
(on any webgpu.h). In the Wine fork, `dlls/webgpu`: the PE side is the
client, the Unix side the server on Dawn, every call made from one thread
(Dawn keeps error scopes per thread). Its tests pass natively (x86_64,
Metal).

### 2.3 wined3d's WebGPU renderer (working natively)

`WINE_D3D_CONFIG=renderer=webgpu`. Modelled on the Vulkan renderer, but
WebGPU tracks lifetimes and hazards itself: no barriers, layouts or
retirement. A buffer object is a WGPUBuffer with a CPU copy that maps
return; an unmap writes the ranges with `wgpuQueueWriteBuffer()`, which is
ordered before all unsubmitted commands, so a NOOVERWRITE write needs
nothing, a DISCARD one renames a busy buffer, and others submit first.
Textures upload through staging copies (256-byte rows) and read back
through mapped buffers. One bind group of sequential bindings per draw,
its layout from the WGSL target's reflection, adjusted for bound formats
that cannot filter. Render pipelines are cached by state. Clears are made
by render pass load operations (per aspect for depth/stencil). Pipelines,
shader modules and textures are made in error scopes, and draws checked
against the bound vertex and index buffers, because one validation error
discards every command in its encoder.

Feature level 9_3 by default: WebGPU has vertex, pixel and compute shaders,
no geometry or tessellation ones. `webgpu_feature_level=10_1` (or 11_0)
claims more, for programs that do not use those stages; Windows 7's DWM
works at 9_x.

Native tests (macOS, Apple M4 Pro, Rosetta x86_64 Wine):

| suite | result |
|---|---|
| webgpu.dll | 40 of 40 |
| d3d11 (FL 9_3) | 3919 checks, 9 failing (16-bit packed formats), 190 skipped |
| d3d9 visual | 735 failing checks (378 in shader fog; the run stops in test_updatetexture) |

Known gaps: 16-bit packed formats (B5G6R5 and the like: no WebGPU
format, conversion not written), D3D9 sRGB write control, partial colour
clears (CPU), stream output, occlusion and timestamp queries (answered,
not measured), UAV clears, mipmap generation, triangle fans, instance
divisors above 1, wireframe, depth bias for lines, comparison sampling of
non-depth textures, and presentation (GDI read-back for now).

### 2.4 Next

1. W3: webgpu.dll's Unix side forwards the wire through `/dev/gex-gpu`;
   the page runs the server on the browser's WebGPU.
2. Presentation: a swap chain's texture is a compositor surface on the same
   device (the compositor today assumes one GL frame:
   web/src/compositor-worker.js, "one canvas and one frame").
3. Section 3.

## 3. Aero: open decision

D3D alone does not give glass. Windows 7 draws it in DWM: `dwm.exe`, whose
`dwmcore.dll` renders with `D3D10CreateDevice1` and `CreateDXGIFactory`, and
which takes each window's pixels from win32k's redirection surfaces
(`dwmcore`: `D3DKMTGetPresentHistory`, `D3DKMTOpenAdapterFromHdc`;
`dwmredir`: `GetWindowCompositionInfo`/`Attribute`). Wine has none of that,
and its dwmapi reports composition off below Windows 8. Two routes:

- **(a) Win7's own dwm.exe**, on 2.x's D3D10.1, with the redirection
  plumbing faked in win32u and gex-kernel's D3DKMT.
- **(b) a glass compositor of our own** in the page (or a guest process)
  on the same WebGPU device: blur-behind for the regions dwmapi is asked
  for (`DwmExtendFrameIntoClientArea`, `DwmEnableBlurBehindWindow`), frames
  from aero.msstyles, thumbnails.

To decide once D3D10.1 renders: (a) is exact if it can be made to run, (b)
is in our hands.

## 4. Reproducing (macOS, Apple silicon)

- Wine host tools from the fork (for widl): configure with Homebrew `llvm`
  and `lld` on PATH (the `lld@20`/`lld@21` kegs lack their libLLVM), no
  `--enable-archs=`; `make __tooldeps__`.
- vkd3d: `./autogen.sh`, then `configure WIDL=<tools>/widl/widl
  CPPFLAGS="-DVKD3D_SHADER_UNSUPPORTED_WGSL" WEBGPU_CFLAGS=-I<dawn>/include
  WEBGPU_LIBS="<dawn>/lib/libwebgpu_dawn.a -lc++ -framework Metal
  -framework QuartzCore -framework IOSurface -framework IOKit -framework
  Foundation -framework CoreFoundation -framework CoreGraphics"` (Perl's
  `JSON` module is needed: `cpanm JSON`). Paths with spaces break
  autotools.
- `tests/shader_runner --filter-executor WebGPU tests/hlsl/X.shader_test`;
  `vkd3d-compiler -x hlsl -p ps_5_0 -b wgsl` for one shader; Dawn's `tint`
  validates the output.
