# Browser demo

The SuperTuxKart demo uses the direct graphics path:

```text
stock application -> guest libGL/libEGL marshaller -> gex-gl wire
    -> web/mesa/gexgl-mesa.cpp -> Mesa st/mesa -> Gallium driver
```

It does not run guest Mesa. The direct path intercepts the standard library
ABI, so STK and other desktop applications require no source modification; the
guest's GL calls cross to the page and run on Mesa compiled to wasm. Which
Gallium driver Mesa runs on is chosen with `?glbackend=`:

- `virgl` (the default for desktop applications): Mesa's virgl encoder, decoded
  by virglrenderer on the page's WebGL 2.
- `wgpu`: the gex_wgpu driver, straight onto the browser's WebGPU. This is the
  driver under development; `?gl=mesa` on the gears demo and the gl44 gates
  select it.

The separate legacy virgl demos run guest Mesa with virglrenderer on WebGL and
do not use this path.

Build the Mesa modules, then publish the assets:

```sh
git submodule update --init third_party/mesa
web/build-mesa-wasm.sh          # gex-wgpu-replay (needs emdawnwebgpu and Tint)
web/build-mesa-virgl-wasm.sh    # gex-virgl-replay
(cd web && npm run assets)
```

The first `build-mesa-wasm.sh` run builds Tint (`web/tint/build-tint.sh`) and is
substantially slower than subsequent incremental builds.

## OpenGL 4.4 validation

The complete 543-command core-4.4 command surface and the guest library's
coverage of it have a structural gate:

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
and the Mesa/gex_wgpu path. `GEX_CTS_CASE=KHR-GL44.…` selects another case;
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
metric.
