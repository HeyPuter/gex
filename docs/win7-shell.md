# The Windows 7 shell in the browser

Windows 7's own shell (explorer.exe, its taskbar, Start menu, desktop and
tray, with the Windows 7 Basic theme) running on W3 Wine (Wine as native
wasm32, docs/wine-native.md) in gex's browser runtime, as a static site.
This is the recipe that builds that site from scratch: from a Windows 7
ISO you supply, three source trees, and a Linux build host's toolchains.

Everything here is scripted in `tools/win7-shell/` (the steps) and
`tools/explorer/` (the shell's staging); `tools/win7-shell/build.sh ISO`
runs every step in order.

## Microsoft's files

No Microsoft file is in any of these repositories, and none is fetched.
Every Windows file the site uses is copied by the steps from the image
extracted from your own ISO (`01-image.sh`); the lists of what is copied
are committed (`tools/explorer/native-explorer.txt`, `stage.sh`,
`stage-stock.sh`). The prefix, the bundle and the site then hold those
files: keep them on your own machines, and serve the site privately.

The recording `tools/win7-shell/w3-session.json` holds no Microsoft
content: process command lines, image and file paths, and unit keys.

## Sources

| tree | repository | branch |
|---|---|---|
| gex (this repository) | github.com/MercuryWorkshop/gex | `x/win7-shell` |
| the Wine fork | github.com/MercuryWorkshop/wine-wasm | `explorer/wine` |
| musl for wasm32 | github.com/MercuryWorkshop/musl-wasm | `gex-wasm32-slim` (93c5dfda) |

```sh
git clone -b x/win7-shell https://github.com/MercuryWorkshop/gex
git clone -b explorer/wine https://github.com/MercuryWorkshop/wine-wasm wine
git clone -b gex-wasm32-slim https://github.com/MercuryWorkshop/musl-wasm musl
```

The ISO the site was made from is "Windows 7 Ultimate x86 SP1" (one image
in `sources/install.wim`); any x86 Windows 7 SP1 ISO with an en-US image
should do (`INDEX=N` picks the image of a multi-image WIM).

## Prerequisites

A Debian 12 x86_64 host (others work with the same tools), about 60 GB of
disk and 16 GB of RAM:

- build tools: `build-essential bison flex pkg-config cpio curl git python3 python3-pip`
- LLVM 22 (apt.llvm.org): `clang-22 lld-22 llvm-22 llvm-22-dev`, with the
  unversioned `clang`, `clang++`, `wasm-ld`, `llvm-ar`, `llvm-ranlib`,
  `llvm-nm`, `llvm-objcopy`, `llvm-strip` and `llvm-config` on PATH
- MinGW for i386: `gcc-mingw-w64-i686` (`i686-w64-mingw32-gcc`; Wine's
  i386 modules and the session's winlogon.exe)
- 7-Zip: `7zip` (`7zz`; reads the ISO and its WIM)
- headers and data: `libegl-dev` (EGL headers), `xkb-data`
  (/usr/share/X11/xkb), `tzdata` (/usr/share/zoneinfo)
- Rust through rustup (the checkout's rust-toolchain.toml pins 1.96.0 and
  the wasm32-unknown-unknown target)
- Node.js 24 or later with npm
- for the check only: Chromium with WebGPU (`/usr/bin/chromium`, or `CHROME=`)

Downloaded by the steps, each checked against a sha256 in the script:
compiler-rt 21.1.8's source (`10-sysroot.sh`, unless `GEX_CRT_SRC` names
one), binaryen (`build-sysroot.sh`), FreeType 2.13.3, Wayland 1.23.1,
libxml2 2.12.9 and libxkbcommon 1.7.0 (`12-deps.sh`); with pip, meson 1.5.2
(`12-deps.sh`) and python-registry 1.3.1 with enum-compat 0.0.3 and
unicodecsv 0.14.1 (`31-stage.sh`, which reads the image's registry hives):

```sh
python3 -m pip install --no-deps --target $W7_OUT/pylib python-registry==1.3.1 enum-compat==0.0.3 unicodecsv==0.14.1
```

## Running it

```sh
export W7_OUT=$PWD/win7-out          # every step writes here
export WINE_SRC=$PWD/wine MUSL_SRC=$PWD/musl
export JOBS=16                       # default: nproc
gex/tools/win7-shell/build.sh "/path/to/Windows 7 Ultimate x86 - SP1.iso"
```

Each step is a script of its own and can be run alone, in order, with the
same environment; `FROM=NN build.sh ISO` resumes at step NN. Logs go to
`$W7_OUT/logs`. Optional: `W7_USER` (default `claude`), the Windows user the
prefix is made for and the site runs as; `GEX_CRT_SRC`.

| step | what it does | output (in `$W7_OUT`) |
|---|---|---|
| `01-image.sh ISO` | `7zz e` install.wim from the ISO, `7zz x` its image | `img/` (Windows/, Program Files/, ...; 4.9 GB) |
| `10-sysroot.sh` | the wasm32 sysroot: musl, libgex, compiler-rt, the ICALL plugin, binaryen (`tools/wine-wasm/build-sysroot.sh`) | `sysroot/sysroot` |
| `11-wine-tools.sh` | Wine's host build tools from the fork (`make __tooldeps__`) | `wine-tools/` |
| `12-deps.sh` | FreeType, Wayland, libxml2, libxkbcommon for wasm32; the host wayland-scanner; gex-gl's EGL; the OSS header | `deps/` |
| `13-wine-wasm.sh` | the fork's wasm32 tree (`make -k`; the modules W3 needs are checked) | `wine-wasm/` |
| `14-link-loader.sh` | the W3 loader: Wine's loader and ntdll Unix side, the in-process wineserver, the Unix libraries of the PE modules W3 uses | `dist/lib/wine/i386-unix/wine` |
| `15-wine-i386.sh` | the fork's i386 PE build, installed | `wine-i386-install/usr/local` |
| `16-dist.sh` | the dist: the native wasm32 module (`winepe.wasm`) and its skeletons for `pe-native.txt` (`tools/wine-wasm/stage-native.sh`), every other i386 module, wine.inf, nls, fonts, winmd | `dist/` (the bundle's `/w3`) |
| `20-runtime.sh` | gexc for x86 guests, the browser shim, kernel and seed, `npm ci` in web/ | in the gex checkout: `target/x86/release/gexc`, `web/public/gex/*.x86*.wasm` |
| `30-prefix.sh` | Wine's install (`wineboot -u`) under gexc on the dist, a `wineboot -r` that carries out its pending renames, then `tools/wine-wasm/page-prefix.sh` (the W3 session's registry, Wayland driver) | `base-prefix/` |
| `31-stage.sh` | Windows 7's shell from the image into a copy of the base prefix (`tools/explorer/stage.sh`, `SHELL_KIND=windows`, the images in `native-explorer.txt`); winlogon.exe (`tools/explorer/build-winlogon.sh`) | `prefix/` |
| `32-bundle.sh` | the bundle: `pfx/` (the prefix with `dll-bases`), `w3/` (the dist), `usr/` (xkb, zoneinfo) | `shell.tar`, `bundle/`, `units.txt` |
| `33-units.sh` | the function-tier units (`gexc cgen-image` at each pinned base) and the session (the recording with this build's unit keys) | `units/`, `rec/w3-session.json` |
| `34-site.sh` | `gexc eject` (compositor off), `user/eject.json`, the project's build, `web/eject-site.mjs --strip --prune --dedupe` | `project/`, `site/` |

Times of a run from fresh clones on a 24-thread host with `JOBS=8`
(1 h 51 min in all): the image 1 minute; the sysroot, Wine's tools and the
dependencies 4 minutes; the wasm32 tree 26 minutes; the i386 build 33
minutes; the loader 5 minutes; the runtime 7 minutes; the install 4
minutes; staging and the bundle 1 minute; the units 27 minutes; the site
5 minutes.

## The result

`$W7_OUT/site` is static files. Serve it over HTTP(S) with
`Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp` (SharedArrayBuffer), and
byte ranges (the bundle's lazy files), and open `index.html` in a browser
with WebGPU. It loads, boots Wine, and shows Windows 7's desktop: the
Basic taskbar with the Start button, the pinned Internet Explorer, Windows
Explorer and Windows Media Player, the clock, the Recycle Bin, and the
Start menu with its stock programs.

The check (correctness only):

```sh
node gex/tools/win7-shell/page-check.mjs $W7_OUT/site $W7_OUT/check
```

serves the site, opens it in headless Chromium at 1280x720 and checks from
screenshots that the taskbar comes up, the wallpaper (with its Windows
logo) and the Recycle Bin show, the three pins and the clock are drawn,
and a click on Start opens the Start menu
(`check-desktop.png`, `check-start.png`, `check.log`; exit status 0 on
PASS).

## Notes

- The dist's PE modules: the eleven in `pe-native.txt` (ntdll, kernel32,
  user32, gdi32, ... and the session's programs) are native wasm32 code
  in `winepe.wasm`; every other module is the fork's i386 build, run by
  gex's x86 guest. MinGW GCC 12 has an internal compiler error on
  oleaut32's vartype.c with Wine's `-msse2`; `15-wine-i386.sh` compiles
  each object that fails so without SSE2.
- The recording: what the shell's run started, loaded and read, made with
  `tools/eject/record-w3.mjs` from the page at the time of site-7h. The
  site's bundle split keeps the files it read in the bundle and serves the
  rest lazily, and `dll-bases.py` pins every DLL it names. Its unit keys are
  replaced by `33-units.sh`; a new recording is only needed when what the
  shell loads changes.
- `tools/explorer/shell-leg.sh` runs the same prefix natively (gexc
  `--wayland-gpu`), with `DIST`, `PFX`, `WINLOGON` and `OUT` set, and
  `USERINIT=1` to start the shell through userinit.exe as the site does.
- Known difference from site-7h: in about half of the page's runs the
  desktop stays unpainted (Windows' plain blue background, no wallpaper,
  no Recycle Bin; the taskbar, pins, clock and Start menu are unaffected),
  and page-check then fails its wallpaper and Recycle Bin checks. Wine's
  desktop process (`explorer.exe /desktop`) deadlocks on the loader lock:
  one of its threads loads Windows 7's uxtheme.dll during the theme logon
  and does not release the lock (16 of 28 runs of the recipe's sites;
  site-7h: 0 of 8 runs).
