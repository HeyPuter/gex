# web

gex in a browser: the runtime API (`runtime/`, the `@gexemu/runtime`
package, `runtime/README.md`, built by `make runtime PKG=DIR`), gex's test page (`index.html`,
`src/harness.js`), the library an ejected program ships, and the builds of
what they load:

- the browser shim, the shared kernel module and the seed, from
  `hosts/browser` and `hosts/kernel` (`build-assets.sh`);
- Mesa for wasm32, from the `third_party/mesa` submodule. It draws through
  Gallium's `gex_wgpu` driver on the browser's WebGPU. A guest's GL calls
  reach it through `guest/gl` and `ports/mesa/src/gexgl-mesa.cpp`
  (`build-mesa-wasm.sh`);
- Xwayland as wasm32, which runs in a Worker of the page and talks to
  `hosts/hostwl` (`build-xwayland-wasm.sh`);
- the guest root filesystems come from the debdemo repository, which
  writes them into `page/public/rootfs`.

## The two builds

| build | dist | threads | hosting |
|-------|------|---------|---------|
| multi-process (default) | `page/dist` | each guest thread is a Worker on shared memory | needs COOP/COEP (SharedArrayBuffer) |
| single-thread | `page/dist-sp` | every guest thread is a fiber in one Worker | any static host |

Both builds load the same shim, kernel and seed. Each builds its own Mesa
and Xwayland, with or without `-pthread`. The dists ship stripped wasm.
Unstripped copies stay in `target/web-debug/<mp|sp>/`, and `--debug` ships
them instead.

```console
$ make page                  # multi-process: page/dist
$ make page-sp               # single-thread: page/dist-sp
$ make serve                 # page/dist at http://localhost:8088/, page/dist-sp at :8089
```

`make page` runs `page/build.sh --mp`. Its header lists the steps and
the options for building one part alone; `make mesa`, `make xwayland` and
`make assets` are three of them. Mesa comes from the `third_party/mesa`
submodule, which `make mesa` checks out.

The first Mesa build also builds Tint (`tint/build-tint.sh`), so it takes
much longer than later ones. Stripping needs `wasm-tools`
(`cargo install wasm-tools`).

## Ejected programs

`make eject-page DIR=...` (`build-eject-page.sh`) builds the page library that
an ejected project ships. `make eject-site PROJECT=... SITE=...`
(`eject-site.mjs`) packs a built 32-bit Wine project into a static
site: the library, its runtime and Mesa, zstd-compressed assets, and the
headers the host must send. The site's `index.html` belongs to the project, so the
library can be imported into a page of your own.
Its API, `start()`, is in `src/README.md`. It is a preset over the runtime
API (`src/run-program.js`).
