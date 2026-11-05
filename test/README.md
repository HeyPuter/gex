# gex's tests

One runner for every test, on two hosts: a headless Chromium page (`browser`)
and a Node process (`node`), both through the runtime API (page/runtime).

```sh
make test                                  # the same as node test/run.mjs, gexc built first
make test ARGS="--group core --host node"  # with options
node test/run.mjs                          # every test, every host it allows, a64
node test/run.mjs --group core --host both --arch a64,x86
node test/run.mjs basic windows/x11        # by name, or group/name
node test/run.mjs --list                   # the jobs, without running them
```

| Option | |
|---|---|
| `--group G,...` | Only these groups (the directories here). |
| `--host browser\|node\|both` | Where to run (default `both`): each test runs on the hosts its `meta.hosts` allows. |
| `--arch a64,x86` | The translated architectures (default `a64`); each test runs on each. |
| `--jobs N` | Tests at once (default 4): pages of one Chromium, Node processes. |
| `--allow-skip` | A skipped test does not fail the run. |
| `--verbose` | Every check line, the guest's console and Chromium's. |
| `--work DIR` | The work directory (default a fresh temporary one, deleted after). Its images are kept and reused while their inputs are unchanged. |
| `--port N` | vite's port (default 5411); Chromium's debugging port is N+1. |
| `--gpu` | Chromium on the machine's GPU (Vulkan), not SwiftShader. |
| `--timed` | Only the tests whose `meta.timed` is true, which no other run includes; refused outside bench.sh (GBENCH_RUN), and implies `--gpu`. |
| `--prebuilt` | Use the images already in `--work` as they are, building none (a bench host, with a work directory synced from where the images were built). An image whose files are not there is missing, and its tests skip (a FAIL without `--allow-skip`). |
| `--prepare` | Make the images the chosen tests need in `--work`, run no test: the half of a `--prebuilt` run done where the images can be built. Exit 1 when one could not be made. |
| `--package DIR` | The runtime as `page/runtime/build.sh DIR` built it, the npm package, not from source: the tests' imports of its entry modules go to DIR's, and `t.baseUrl` is none (the package's own files). Browser host only: the package's Workers are bundled for a page. Also GEX_TEST_PACKAGE. |
| `--artefacts DIR` | Where screenshots go (default `artefacts/` in the work directory): `t.screenshot`'s, and `fail.png` of every browser test that fails, in `DIR/GROUP-NAME-ARCH/`. |

Env: `BUSYBOX` (a wasm32 busybox, for the `busybox` image and page/wasm32-page's shell leg), `GEXCC` (gexcc,
default on PATH; the `gexcc` group's programs run under it, its install's
`gexcc-exec` given this arch's gexc), `GEX_TEST_GLIB` (a wasm32 GLib install, for
gexcc/dl-gobject), `GEX_TEST_INNER` (how many of gexcc/programs' programs run at
once, default 4), `GEX_TEST_SLIM` (a directory with a Debian image,
`debian-slim.tar.zst`, and its apt repository, `repo/`), `CHROME` (default
/usr/bin/chromium), `GEX_TEST_WINE64_DIST` and `GEX_TEST_WINE64_PREFIX` (the `wine64` image), `GEX_TEST_WOW64_DIST`, `GEX_TEST_WOW64_PREFIX` and `GEX_TEST_WOW64_MONO` (the `wow64` group: Native tests), `GEX_TEST_WOW64_BUILD32` and `GEX_TEST_WOW64_SYSROOT32` (`wow64/ptval32`: wine/dist.sh's WINE64_BUILD32 and WINE64_SYSROOT32), `GEX_TEST_GEXCC_WIN`, `GEX_TEST_WINE64_TOOLS`, `GEX_TEST_WINE64_SRC` and `GEX_TEST_WINE64_BUILD` (the `wine64` group: Native tests), `GEX_TEST_WINE64_NATIVE`, `GEX_TEST_WINE64_M64_DIST` and `GEX_TEST_WINE64_M64_PREFIX` (the `wine64-page` group), `GEX_TEST_WINE32_DIST` and `GEX_TEST_WINE32_PREFIX` (32-bit Wine and its prefix: `wine64-page/fault-nest`'s 32-bit cases), `GEX_TEST_GEXC` (the `wine64-units` image), `GEXC_X86` and `GEXC_A64` (the gexc a test's native leg runs, by arch; default `GEXC`, then target/x86/release and target/release), `A64_ROOTFS` (an arm64 Debian root, .tar.zst, for glibc programs on a64), `GEX_WASM_SYSROOT` (page/dense-exec, native/wasm32, page/wasm32-page), `GEX_WASM64_SYSROOT` (native/wasm64, page/wasm64-page), `GEX_TEST_SQLITE_LIB` (gexcc/locks-wal: the wasm32 tree's libsqlite3.so.0 directory), `GEX_TEST_SNAP_SYSROOT` (the `snapshot` group: a snapshot-flavour sysroot), `GEX_TEST_WINE64_SERVER` (snapshot/snapshot's wineserver: wine/dist.sh's WINE64_SERVER, with GEX_TEST_WINE64_DIST), `GEX_TEST_SNAP_WINE64_DIST`, `_PREFIX` and `_NATIVE` (snapshot/snapshot's 64-bit Wine legs: an install built against the wasm64 snapshot sysroot), `GEX_TEST_NODE_ROOT` and `GEX_TEST_NODE_SH` (gexcc/node-child: a Debian arm64 tree with node, and its dash), `GEX_TEST_MESA_BUILD` (the `mesa-cold` image and the `mesa` group's replay: a directory with ports/mesa/build.sh's mesa-mt and mesa-st, default target/web-build; the replay needs mesa-mt's gex-mesa-replay.js, which a page-only build leaves out), `GEX_QEMU` (the `qemu` group's oracle, a qemu-aarch64: default ~/toolchains/qemu-aarch64, then PATH; its work directory must be on disk, as a guest's /dev/shm is its own), `GEX_TEST_NATIVE_LIBS_ROOT` and `GEX_TEST_NATIVE_LIBS_SH` (the `libs` group's native-libs: a Debian arm64 root with libz, libzstd, libsqlite3 and sqlite3, and a wasm32 sh; with toolchain/native-libs' own ZLIB_SRC, ZSTD_SRC, SQLITE_SRC, GEXCC_PREFIX and A64_SYSROOT or MUSL_SRC), `GEX_TEST_WORK`, `GEX_TEST_PORT` and `GEX_TEST_ARTEFACTS` (the
defaults of `--work`, `--port` and `--artefacts`).

The browser host is vite on page/ (cross-origin isolated, the runtime from
source) and one headless Chromium on WebGPU. Each test gets a page in a
browser context of its own. The node host forks a Node process per test (and
per phase).

## Output

One JSON line per test, host and arch on stdout, as each ends:

```json
{"test":"core/basic","host":"browser","arch":"a64","result":"ok","ms":4900,"checks":23,"fail":[],"skip":null,"times":{}}
```

| Field | |
|---|---|
| `test` | `group/name`. |
| `host` | `browser` or `node`. |
| `arch` | `a64` or `x86`. |
| `result` | `ok`, `FAIL` or `skip`. |
| `ms` | Wall time, every phase. |
| `checks` | Checks made. |
| `fail` | What each failed check said; `threw` when the test threw, `load` when it would not load, or the runner's reason (a timeout, an exit). |
| `skip` | Why it was skipped (an image missing, no WebGPU adapter), else null. |
| `times` | `{name: ms}` from `t.time`. |

What a person reads goes to stderr: one line per test, and each FAIL line.
The exit status is 0 only when every test is ok; a skip counts as not ok
unless `--allow-skip` is given.

## A test

`test/GROUP/NAME.test.js`, a module with no HTML:

```js
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 120 };

export default async function (t) {
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new MemoryFilesystem(sys).addTar(t.work('bb.tar.zst'));
  await sys.boot();
  t.eq('echo', (await sys.run(['sh', '-c', 'echo hi'], { env: { PATH: '/bin' } })).stdout, 'hi\n');
  await sys.shutdown();
}
```

`meta` is an object literal (the runner reads it without running the test):

| Field | |
|---|---|
| `hosts` | Where it can run (default `['browser']`). |
| `arches` | The arches it runs on (default both, `['a64', 'x86']`). |
| `images` | The images it needs (fixtures/images.mjs): made in the work directory before any test runs. Without one, the test is skipped. |
| `timeout` | Seconds for the whole test, every phase (default 120). |
| `html` | The page's elements (browser), added to its body before the test runs. |
| `timed` | A timing test: run by `--timed` only (default false). |
| `alone` | Run after every other job, one at a time (default false): a test that reads its renderer's memory from /proc (`t.procStatus`), which the runner can only tell from other pages' when it is the only one running, or one too big to share a renderer (windows/kde: a whole desktop's Workers). |

A page or process runs one test, so each makes its own System.

### `t`

| | |
|---|---|
| `t.check(what, cond, detail)`, `t.eq(what, got, want, more)` | A check: a line, `ok` or `FAIL`. A failed check does not stop the test. |
| `t.near(what, colour, want, tol)` | A check that a colour is within `tol` (default 8) of `want` in every channel it names. |
| `t.say(text)` | A line of output. |
| `t.until(fn, what, ms)`, `t.timeout(promise, ms, what)` | Wait for a truthy `fn()`, or for a promise, else throw. |
| `t.lines(process, stream)` | A stream's text as lines, as they come (a process's stdout by default). |
| `t.time(name, fn)`, `t.record(name, value)` | `fn`'s ms, or any number, in the result's `times`. |
| `t.known(what, cond, detail, why)` | A check known to fail today, `why` saying who has it: a failure is a `KNOWN` line, listed in the result's `known`, and does not fail the test. A pass says the mark can go. |
| `t.skip(why)` | End the test as skipped. |
| `t.host`, `t.arch` | The host, and the translated arch (`aarch64`, `x86`). |
| `t.gpu`, `t.timed` | Whether the run is on a GPU (`--gpu`), and a timed one (`--timed`). |
| `t.baseUrl`, `t.debug(more)` | A System's `baseUrl` and `debug` on this host. |
| `t.work(name)` | A work directory file as a URL (http on the browser host, file on node). |
| `t.reload(state)` | Run the test again in a fresh page or process, as phase `t.phase + 1` with `t.state = state`; the checks so far are kept. |
| `t.phase`, `t.state` | This run's phase (1 first), and what `reload` was given. |
| `t.input(...events)`, `t.click(x, y)` | Real input over CDP: `{ method: 'Input.dispatchMouseEvent', ... }` and the like (browser). |
| `t.key(code, key, modifiers)`, `t.type(text)`, `t.move(x, y, buttons)`, `t.wheel(x, y, dx, dy)` | Real keys, typing, pointer motion and wheel turns, to whatever the page has focused or under the pointer (browser). |
| `t.pixels(clip)`, `t.pixel(x, y)` | The page's pixels as the browser composited them (a WebGPU canvas too): an ImageData of `{ x, y, width, height }`, or one `[r, g, b, a]` (browser). |
| `t.screenshot(name, clip)` | The page, or `clip` of it, saved as `NAME.png` among the artefacts: its path (browser). |
| `t.saveFile(name, text)` | `text` saved as `NAME` among the artefacts, beside the screenshots: its path (browser). |
| `t.cdp(...commands)` | Any CDP commands on the page, `{ method, ...params }`, sent in order: their results (browser). |
| `t.hits(name)` | How many times the page asked for work file `name` (browser). |
| `t.logs(re, { levels })` | The lines the runtime logged this phase that match `re`: the page's and its Workers' console (browser), the process's output (node). With `levels` (browser), each line begins with its console level and a space (`warning ...`). |
| `t.procStatus()` | Memory from /proc in KiB, `{ data, rss, hwm }` (VmData, VmRSS, VmHWM): this phase's process (node), or the page's renderer when it is the only renderer holding over 256 MiB (browser; null otherwise). `t.peakRss()` is its `hwm` (node: the process's maxRSS). |

## Native tests

Node-host tests may run programs natively as well as on the runtime:
lib/gexcc.mjs builds with gexcc and the host's compiler and runs under this
arch's gexc (`gexcc/programs`: fixtures/gexcc/programs, each against the host
natively, under gexc and on the page's shim). A check written as a shell
script is `script(t, NAME)`, fixtures/gexcc/dl/NAME.sh: each `ok`/`FAIL` line
it prints is a check (`gexcc/dl-*`). lib/native.mjs builds Linux programs
with a musl cross gcc (GEX_TOOLCHAIN, an aarch64-linux-musl-cross tree, and
GEX_TOOLCHAIN_X86; default under ~/toolchains), runs them under gexc and,
through lib/guest.mjs, on the page's shim (`native/*`: fixtures/native/NAME,
the sources only; each test builds, runs and compares itself).
lib/qemu.mjs runs AArch64 programs (fixtures/integration) under gexc and
under QEMU, whose stdout and exit status must agree byte for byte (`qemu/*`,
a64 only: QEMU is the oracle, so a bug our decoder and lifter share cannot
pass), and on the page's shim against the same oracle (`qemu/page`).
lib/winerun.mjs runs 32-bit Windows programs (fixtures/wow64, built with
i686-w64-mingw32-gcc) as WoW64 processes of 64-bit Wine, natively under
x86 gexc and on the page's x86 shared shim (`wow64/*`, x86 only). It needs
GEX_TEST_WOW64_DIST and GEX_TEST_WOW64_PREFIX (`WINE64_WOW64=1
wine/dist.sh`'s WINE64_DIST and WINE64_PREFIX, the session prefix) and, for
`wow64/dotnet`, GEX_TEST_WOW64_MONO (Wine Mono, both halves, unpacked); it
prepares a copy of the install and prefix once per work directory. Its
wine64(t) runs 64-bit programs the same way on 64-bit Wine's default
install (`wine64/*`: GEX_TEST_WINE64_DIST and GEX_TEST_WINE64_PREFIX, the
`wine64` image's), the programs from fixtures/wine/src (the `win-progs`
image) and fixtures/wine64: console, DLL, audio, native-core, shell, GUI
(gexc's Wayland compositor, its frames checked) and GL (the host's GL)
cases. `wine64/gexccwin` and `wine64/gl`'s d2dsize need GEX_TEST_GEXCC_WIN
(a gexcc-win install), `wine64/dll` GEX_TEST_WINE64_TOOLS,
GEX_TEST_WINE64_SRC, GEX_TEST_WINE64_BUILD (wine/dist.sh's WINE64_TOOLS,
WINE64_SRC and WINE64_BUILD) and GEX_WASM64_SYSROOT, `wine64/layout`
GEX_TEST_WINE64_SRC.

`wine64-page/*` runs them on the page (run-node.mjs with the x86 shared
shim and seed, wine/page-prefix.sh's prefix): `mp` the default build,
`sp` the single-thread one, `compact` and `pe` with the native PE core
GEX_TEST_WINE64_NATIVE (wine/dist.sh's WINE64_NATIVE) in place of the
install's (the compact space; the native modules' host-data views),
`units` with function-tier units built from a first run, and `mem64` the
memory64 flavour (GEX_TEST_WINE64_M64_DIST and _PREFIX, wine/dist.sh with
WINE64_MEM64=1). wine64/cases.mjs holds the cases both groups share.
`site/eject-page-files` builds an ejected page and checks it carries the
page's runtime and X server.

## Images

fixtures/images.mjs:

- `busybox`: `bb.tar.zst`. This is BUSYBOX and its applets, the probe programs (fixtures/probe/*.c, built with gexcc), and an `/etc/xattr` with an extended attribute. It also holds `lazy.bin` and `lazy-program.bin`, a lazy file's bytes (core/dir-times's and core/runprogram's), and `rt-side.so`, the side module core/dlopen-static's static program loads.
- `slim`: GEX_TEST_SLIM's image and repository, with `slim-repo.json` listing the repository.
- `package`: `pkg/`, the runtime package (page/runtime/build.sh).
- `wine64`: `wine64.tar` and `wine64-lazy.json`. They hold 64-bit Wine's install (GEX_TEST_WINE64_DIST, wine64-dist.sh's WINE64_DIST) at /wine64 and a page prefix made from GEX_TEST_WINE64_PREFIX (wine/page-prefix.sh) at /pfx. Files over 64 KiB are lazy, served from `wine64/` in the work directory (fixtures/wine/pack.py). A link in the install that points out of it (a dist made of links into another) is packed as what it points at.
- `win-progs`: `win-progs.tar`, the Windows test programs in fixtures/wine/src built with MinGW or clang (a source's `gex-nt-cc:` line) into `x64/` and `x86/`, or into one of them (`gex-nt-arch:`), with the libraries a `gex-nt-libs:` line names, as a DLL with `gex-nt-shared`. `win-progs.json` says which did not build, and why.
- `wine64-units`: `wine64-units/`, function-tier units (a unit cache) for the test programs in fixtures/wine/units.mjs and Wine's core DLLs, at the bases a run of each program names (fixtures/wine/unit-bases.mjs), and the NT trampolines. Built with GEX_TEST_GEXC (default target/x86's gexc) in a few minutes.
- `gl-probes`: `gl-probes-x86.tar` and `gl-probes-a64.tar`, the guest GL and the self-checking probes in guest/gl/probes built against it (x86-64 glibc with gcc; aarch64 musl with ~/toolchains/aarch64-linux-musl-cross, when it is there). Also `gl-probes.json`: each probe's last line from a run on the host's own Mesa, which the guest's run must print, and the GL version it needs. Needs the host's EGL and GL and third_party/opengl-registry.
