# web

The page that runs gex in a browser, and the builds of what it loads:

- the browser shim, the shared kernel module and the seed, from
  `shims/browser` and `shims/kernel` (`build-assets.sh`);
- Mesa for wasm32, from the `third_party/mesa` submodule. It draws through
  Gallium's `gex_wgpu` driver on the browser's WebGPU. A guest's GL calls
  reach it through `guest/gl` and `web/mesa/gexgl-mesa.cpp`
  (`build-mesa-wasm.sh`);
- Xwayland as wasm32, which runs in a Worker of the page and talks to
  `shims/hostwl` (`build-xwayland-wasm.sh`);
- the guest root filesystems come from the debdemo repository, which
  writes them into `web/public/rootfs` (and the desktop demo's launcher
  packages into `web/public/pkgs`).

## The two builds

| build | dist | threads | hosting |
|-------|------|---------|---------|
| multi-process (default) | `web/dist` | each guest thread is a Worker on shared memory | needs COOP/COEP (SharedArrayBuffer) |
| single-thread | `web/dist-sp` | every guest thread is a fiber in one Worker | any static host |

Both builds load the same shim, kernel and seed. Each builds its own Mesa
and Xwayland, with or without `-pthread`. The dists ship stripped wasm.
Unstripped copies stay in `target/web-debug/<mp|sp>/`, and `--debug` ships
them instead.

```sh
git submodule update --init third_party/mesa
web/build-all.sh             # multi-process
web/build-all.sh --sp        # single-thread
web/build-all.sh --both
node web/serve-dist.mjs 8088 --sp-port 8089
```

The first Mesa build also builds Tint (`tint/build-tint.sh`), so it takes
much longer than later ones. Stripping needs `wasm-tools`
(`cargo install wasm-tools`).

## Ejected programs

`build-eject-page.sh` builds the page library that an ejected project ships.
`eject-site.mjs PROJECT OUT` packs a built 32-bit Wine project into a static
site: the library, its runtime and Mesa, zstd-compressed assets, and the
headers the host must send. The site's `index.html` belongs to the project, so the
library can be imported into a page of your own.

## The desktop demo

`build-demo.sh` builds the desktop page into `web/dist-demo`. Its guest side
(the base rootfs and the launcher's packages) comes from the debdemo
repository, built against this checkout first:

```sh
make -C path/to/debdemo native GEXCC_PREFIX=...   # the gexcc .deb pool, once
make -C path/to/debdemo desktop GEX_DIR=$PWD      # web/public/rootfs/base.tar.zst, web/public/pkgs
web/build-demo.sh
```

Without them `build-demo.sh` stops and says so. The other pages (`build-all.sh`)
need nothing from debdemo; their Debian demos say when their rootfs is missing.
