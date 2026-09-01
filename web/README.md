# Browser demo

The SuperTuxKart demo uses the direct compatibility graphics path:

```text
stock application -> guest libGL/libEGL marshaller -> MobileGL -> WebGPU
```

It does not run virglrenderer or guest Mesa. Those remain available for the
separate legacy virgl demos; the direct path intercepts the standard library
ABI, so STK and other desktop applications require no source modification.

MobileGL is pinned as `third_party/mobilegl-webgpu` and patched at build time.
The WebGPU fork is unfinished and, candidly, much of this backend is not written
well: important GL invariants are missing from cache keys, unsupported calls
silently poison whole WebGPU command buffers, and several formats, framebuffer
operations, and texture types remain unimplemented despite advertising desktop
OpenGL 3.3. Treat it as an experimental compatibility layer, not a conformant GL
implementation.
Keeping the changes in `web/mobilegl-*.patch` makes those local fixes explicit
and leaves the upstream checkout reproducible.

Build the direct WebGPU renderer, then publish the assets:

```sh
git submodule update --init --recursive third_party/mobilegl-webgpu
web/build-mobilegl-wasm.sh
(cd web && npm run assets)
```

The first WebGPU build compiles the fork's pinned Tint and glslang trees and is
substantially slower than subsequent incremental builds. SuperTuxKart selects
`gexmobilegl-webgpu.mjs`; the virgl demos continue to use their own renderer.

## OpenGL 4.4 validation

The complete 543-command core-4.4 loader surface and the currently transported
and WebGPU-backed subsets have a structural gate:

```sh
tests/gl44-layer.sh
```

That is deliberately not a conformance claim. Official semantic evidence comes
from the pinned Khronos GL CTS (`opengl-cts-4.6.8.1`):

```sh
export GEX_CTS_TOOLCHAIN=/path/to/aarch64-linux-musl-cross
tests/gl44-cts.sh
tests/gl44-cts-browser.sh
```

The second command runs a real `KHR-GL44` case through the persistent browser
and direct MobileGL/WebGPU path. `GEX_CTS_CASE=KHR-GL44.…` selects another case;
the growing pass list, rather than symbol coverage or an advertised version
string, is the compliance measure.

## SuperTuxKart real-present benchmark

With the persistent browser CDP server on port 9335 and the demo server on
port 8091, run:

```sh
tests/stk-webgpu-fps.sh
```

The gate profiles a race and separately measures browser animation ticks and
distinct WebGPU canvas frames over the final ten seconds. It requires at least
59.8 real presents per second, 99.5% fresh-frame coverage, and no more than one
missed animation tick. The game can produce frames faster than the browser can
display them, so its own `Average FPS` is diagnostic rather than the pass/fail
metric. A four-snapshot MobileGL presentation queue decouples guest production
from browser animation timing; this costs about 4.7 MiB at 640x480 and can add
roughly 40 ms of queueing latency when the guest runs near 95 FPS.
