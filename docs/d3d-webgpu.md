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
         native Wine: on Dawn (Metal, Vulkan, D3D12), in-process
         in a gex guest (W3): forwarded through /dev/gex-gpu
  -> /dev/gex-gpu: gexc's on Dawn (crates/gex-webgpu), or the page's
     server on the browser's WebGPU (emdawnwebgpu, gpu-bridge.mjs)
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

### 2.2a /dev/gex-gpu (working: gexc, browser runtime in Chrome and Node)

A guest reaches a host WebGPU through `/dev/gex-gpu`
(`guest/gpu/transport-dev.h`): one ioctl per wire message, carrying the
message's and the reply buffer's addresses in the guest's memory; a reply
too big for the buffer is held for `GEX_GPU_FETCH`. Every open is a
connection with a server of its own (ids are the client's). The guest's
transport is `guest/gpu/transport-dev.c`; webgpu.dll's Unix side opens the
device when there is one, and without Dawn (no `DAWN_LIBS`) that is its
only server.

- **gexc** (`--features webgpu`, `GEX_DAWN_PREFIX=<Dawn release>`):
  `shims/native/src/gpu.rs`, the servers on Dawn from `crates/gex-webgpu`,
  all on one thread.
- **Browser runtime**: `shims/browser/src/gpu.rs` (the submit deferred
  past the kernel's lock, like the GL device's), `gpu-bridge.mjs` (a
  SharedArrayBuffer channel: the Worker blocks in `Atomics.wait`, the
  server answers), and the server, `web/build-gexgpu-server.sh`:
  wire-server.c on emdawnwebgpu with JSPI, so a WAIT suspends the server's
  promise rather than the page. A page serves it with `?webgpu=1`
  (`runProgram`'s `webgpu`); `run-node.mjs` with `GEX_GPU=1` over Dawn's
  Node bindings (the `webgpu` npm package) and `--experimental-wasm-jspi`.

With a compositor, the server runs in its worker on its own device
(`Compositor.hostGpu`, `gexgpu_server_set_shared`: every client is given
that adapter and device and cannot destroy it), so a guest's textures are
on the device that draws the screen; `web/test/gpu-compositor.html` runs
gpu-smoke so. Presenting a guest's swap chain texture as a window's
pixels, without the GDI read-back, is the next step on that.

`guest/gpu/test/gpu-smoke.c` (static aarch64, `build-guest.sh`: Alpine in
Docker) runs a compute shader and checks the result: "gpu-smoke: ok" under
`gexc run`, under `run-node.mjs`, and in Chrome (a page serving the
server, a Worker running the single-Worker `Gex` with a `GpuProxy`). The emdawnwebgpu package must match
`third_party/dawn-api` (the Dawn release `v20261009.190412` for both).

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
| d3d11 (FL 9_3) | 3925 checks, 3 failing, 190 skipped |
| d3d10_1 (FL 9_3 / 10_1) | 97 / 318 checks, none failing |
| d3d10core (FL 10_1) | runs to the end: 194656 checks, 480 failing (depth bias 205: Metal's depth24plus is float underneath; stream output ~100; layered rendering needs geometry shaders) |
| d3d9 visual | 332 failing checks to test_depth_stencil (the 20-minute limit), 120 in test_signed_formats; shader fog fixed |

Known gaps: D3D9 sRGB write control, stream output (WebGPU's vertex
shaders cannot write storage), occlusion and timestamp queries (answered,
not measured), UAV clears, triangle fans, instance divisors above 1,
wireframe, depth bias for lines, comparison sampling of non-depth
textures, geometry and tessellation shaders, and presentation (GDI
read-back for now). Done by other means than the GL and Vulkan renderers:
typed buffer views (storage buffers decoded in the shader, the format part
of the variant), sampler LOD bias (in the shader), depth24plus read-back
(drawn into r32float), mipmap generation and partial clears (draws).

### 2.4 Next

1. W3 end to end: a D3D program under W3 in the page with `?webgpu=1`
   and `renderer=webgpu` (waits on the W3 page bring-up being reproducible
   here).
2. Presentation: a swap chain's texture is a compositor surface on the same
   device (the compositor today assumes one GL frame:
   web/src/compositor-worker.js, "one canvas and one frame").
3. Section 3's compositor.

## 3. Aero: route (b), a glass compositor of our own

D3D alone does not give glass: Windows 7 draws it in DWM. An analysis of the
SP1 binaries (dwm.exe, dwmcore, uDWM, dwmredir, uxsms, with Microsoft's
public symbols, against the Wine fork's spec files, 2026-10-14) settled the
choice.

**What real DWM needs.** dwmcore's device is `D3D10CreateDevice1(HARDWARE,
flags 0x99)` trying 10_1, 10_0, 9_3, 9_2, 9_1 (no WARP, no D3D9 path), so
the WebGPU wined3d at 9_3 is enough for the device. Everything else is
win32k's: uxsms starts dwm.exe and hands it `\UxSmsApiPort`; dwm.exe makes a
waitable LPC port and `RegisterSessionPort`s it, then
`DwmStartRedirection(1)`; win32k then tells it about windows over kernel-only
LPC messages (sprites, child notifications, logical surfaces: about 25
undocumented layouts, dispatched by dwmredir's
`DwmRedirectionManagerDispatchMessage`). Pixels come from win32k logical
surfaces (gdi32 ordinals 1000-1011: `DwmGetSurfaceData`,
`DwmHLSurfGetDirtyRgn`, ...) as DX shared allocations or sections, and
recomposition is driven by `D3DKMTGetPresentQueueEvent` /
`D3DKMTGetPresentHistory` tokens. Wine has no LPC (all `NtCreatePort`-family
calls are stubs), none of the HLSURF or Sfm calls, and a stub present
history. The glass frame itself is uDWM building dwmcore (MIL) drawing
instructions from aero.msstyles' `DWMWindow` atlas: not reusable without
dwmcore. Estimate for (a): 10k+ lines, mostly reverse-engineering
undocumented kernel behaviour.

**Chosen: (b).** gex already has DWM's redirection in effect: each top-level
window is a surface with damage in the gex compositor (docs/nt-layer.md
§6a). What is needed:

1. dwmapi: composition on for Windows 7 versions; keep each window's
   frame extension, blur-behind region, attributes, colorization and
   thumbnail registrations and send them to the compositor; the ordinals
   explorer and shell32 import (#113 live preview, #114/#124 thumbnails,
   #127/#131 colorization, #106/#107).
2. win32u/user32: no frame drawn for composited top-level windows (the
   frame is client area, as on Windows 7), caption buttons hit-tested
   through `DwmDefWindowProc`, the client's alpha kept in extended glass.
3. The compositor, on WebGPU: backdrop blur under frames and blur-behind
   regions, the colorization tint (ColorizationColor, Balance, Afterglow,
   BlurBalance), the `DWMWindow` atlas parts as nine-grids (uxtheme's
   `GetThemeStream` for TMT 213 in msstyles.c, or the atlas extracted
   offline), reflection and caption glow, live thumbnails for the taskbar
   and Alt-Tab.

Revisit (a) only if exact Flip3D or animations become the goal.

### 3.1 State (2026-10-14)

- **Protocol**: `crates/gex-wayland/protocols/gex-glass-v1.xml`: a
  surface's glass region and DWM's colorization (colour, balance, blur
  balance), taking effect at once. gex-wayland keeps it in `SurfaceInfo`
  (`Glass`), and the surface record (56 words now) carries up to four
  rectangles.
- **Compositor** (`web/src/compositor-worker.js`, `Painter.glass`): before
  a window with glass, the scene's pass ends, the box beneath the glass is
  copied out and blurred (separable Gaussian, two passes each way, wider
  the second time), and drawn back, tinted, under the window. The window's
  alpha counts only inside its glass, as under DWM, so a GDI window (alpha
  byte 0 throughout) is opaque but where it is glass. `web/test/glass.html`
  shows it with hand-made records.
- **Wine** (fork `gex-webgpu`): dwmapi reports composition on when the
  display driver draws glass, and `DwmExtendFrameIntoClientArea`,
  `DwmEnableBlurBehindWindow` and `DwmGetColorizationColor` work, through
  `NtUserSetWindowGlass` (a private `NtUserCallHwndParam`) to win32u, which
  keeps each window's glass, turns it into rectangles relative to the
  visible rect (frame bands, extended margins, blur-behind) whenever the
  window's rects change, and hands them to the new user-driver entry
  `pSetWindowGlass`. winewayland.drv implements it with `gex_glass_v1`.
- **Frames**: `tools/aero/extract-dwm-frame.py` extracts DWM's frame atlas
  and its parts (rectangles, nine-grid margins, states) from a Windows 7
  aero.msstyles; the page serves them from the user's own copy at
  `/gex/aero/` (Microsoft's artwork: gex ships only the tool).
  `gex_glass_v1.set_frame` carries a frame's flags (frame, active,
  maximized, tool) and the caption buttons' rectangles and states; the
  compositor draws the frame pieces, the title in black on its glow, and
  the buttons and glyphs. In Wine, win32u makes every captioned top-level
  window's frame glass when the driver draws glass (the glass surrounds
  the box inside borders and caption; a menu bar stays opaque) and passes
  the buttons where its own hit testing has them, pressed state included.
- **Not yet**: hover glow on caption buttons (win32u does not track
  hover), the reflection streaks, thumbnails and Flip3D, the Windows 7
  colorization formula beyond colour-over-blur, and an end-to-end run:
  W3 (winewayland in the page) is not reproducible from the repo yet.

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
