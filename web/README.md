# Browser demo

The SuperTuxKart demo uses the direct graphics path:

```text
stock application -> guest libGL/libEGL marshaller -> gex-gl wire
    -> web/mesa/gexgl-mesa.cpp -> Mesa st/mesa -> Gallium driver
```

It does not run guest Mesa. The direct path intercepts the standard library
ABI, so STK and other desktop applications require no source modification; the
guest's GL calls cross to the page and run on Mesa compiled to wasm, on the
gex_wgpu Gallium driver, straight onto the browser's WebGPU (built from
mesa-wink's `wink` branch, the `third_party/mesa` submodule).

Build the Mesa modules, then publish the assets:

```sh
git submodule update --init third_party/mesa
web/build-mesa-wasm.sh          # gex-wgpu-replay (needs emdawnwebgpu and Tint)
(cd web && npm run assets)
```

The first `build-mesa-wasm.sh` run builds Tint (`web/tint/build-tint.sh`) and is
substantially slower than subsequent incremental builds.

## The two builds: multi-process (default) and single-thread

The page has two builds:

- **MP**, the multi-process build, is the default. Every guest thread is its
  own Worker on a shared memory. It needs SharedArrayBuffer, so it needs
  COOP/COEP.
- **SP**, the single-thread build (`docs/single-thread.md`), runs on any
  static host. Every guest thread is a JSPI fiber in one Worker.

Both run the same shim, kernel and seed, and the same `web/dist`. A page picks
SP when it has no SharedArrayBuffer, or when asked with `?single=1`. Only Mesa
and Xwayland differ: they are compiled with or without `-pthread`, and each
flavour has its own build and published directories. One command builds
either flavour or both, incrementally:

```sh
web/build-all.sh            # MP (the default)
web/build-all.sh --sp       # SP
web/build-all.sh --both     # both, side by side
```

| | MP | SP |
|---|---|---|
| shim, kernel, seeds, rootfs | `web/public/gex/`, `web/public/rootfs/` (shared) | same files |
| Mesa | `target/web-build/mesa-mt` → `web/public/gex/` | `target/web-build/mesa-st` → `web/public/gex/st/` |
| Xwayland | `target/web-build/xwl-mt` → `web/public/gex/xwayland/` | `target/web-build/xwl-st` → `web/public/gex/xwayland-st/` |
| bundle | `web/dist` (shared) | same |

`--no-assets`, `--no-gl` and `--no-x` skip the shared runtime, Mesa and
Xwayland. `GEX_WEB_BUILD` moves `target/web-build`. To serve both at once,
MP with COOP/COEP on one port and SP without on another:

```sh
node web/serve-dist.mjs 8088 --sp-port 8089
```

Tests: the default page smoke test is `node web/test/site.mjs`. The SP leg is
`tests/single/run.sh`, which runs `tests/single-callout` and then the same
demos without COOP/COEP.

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
