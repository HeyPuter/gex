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
  -> wined3d with a WebGPU adapter (new: adapter_wgpu, beside adapter_gl/vk)
       shaders: vkd3d-shader -> WGSL (new target)
  -> webgpu.dll (new Wine DLL, shaped like winevulkan): webgpu.h exports
       Unix side: a dawn_wire client writing into a gex ring
  -> /dev/gex-gpu (new device, shaped like /dev/gex-gl)
  -> page: dawn_wire server on emdawnwebgpu, on the compositor's GPUDevice
     native host: dawn_wire server on native Dawn (Metal, Vulkan, D3D12)
```

Decisions and why:

- **WebGPU's own API as the wire, not a D3D-level protocol.** webgpu.h is a
  standard C API; anything (wined3d, a d3d12 layer, Wine's own winewebgpu
  for Windows programs that use Dawn, a future GL layer) can sit on it, and
  the page side stays a thin, generic replayer. dawn_wire is the
  serialisation Chrome itself uses between renderer and GPU process, so the
  wire is Google's, tested, and versioned with the headers.
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

### 2.2 Next

1. The rest of the WGSL target (2.1's list), then SM2/3 (d3dbc) on the
   runner, which d3d9 needs.
2. dawn_wire client for the guest (wasm32, W3's sysroot) and server for the
   page (emscripten, emdawnwebgpu) and the native host; `/dev/gex-gpu`.
3. Wine `dlls/webgpu` (PE exports + Unix side), then wined3d's adapter.
4. Presentation: a swap chain's texture is a compositor surface on the same
   device (the compositor today assumes one GL frame:
   web/src/compositor-worker.js, "one canvas and one frame").

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
