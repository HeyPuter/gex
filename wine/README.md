# wine

Wine for gex. Wine's own Unix side is built as wasm32 with gex's compilers:
the loader, ntdll's Unix half, the wineserver and the drivers. On the
Windows side, Wine's builtin DLLs are native wasm modules; WoW64's i386 ones
are still translated until their native builds pass. A program's own x86
code is translated by gex like any other guest code. Everything is built
from the pins in `wine64.pins` (Wine and musl) and `wine32.pins` (the Unix
libraries).

## Build it

```console
$ make wine
...
WINE64_DIST=~/.cache/gex/wine64/dist64-...
WINE64_PREFIX=~/.cache/gex/wine64/pfs64-...
WINE64_NATIVE=...
```

This is 64-bit Wine with WoW64. One process runs x86-64 and i386 Windows
programs, and one prefix holds both (`system32` and `syswow64`).
`make wine-x64` builds it without WoW64. The output lines are also saved in
`out/wine.env` (or `out/wine-x64.env`), ready for `.`. Each step is cached in
`~/.cache/gex/wine64` by what it is built from, so a rerun only checks. The
first build takes about 40 minutes on 16 threads.

What it fetches and builds, in order:
- 64-bit Wine at `WINE64_COMMIT`. `WINE_DIR=` builds a Wine worktree of
  your own instead.
- The LP64 sysroot, from musl at `MUSL64_COMMIT` (`make sysroot64`).
- A configured 32-bit Wine build tree, which the wineserver is linked from
  (`make -C wine build`).
- Wine's tools, natively, and its PE side with MinGW.
- The Unix side as wasm. The native PE modules are linked as one core plus
  a module for each remaining DLL.
- The prefix (`wineboot -i`, natively, as user `gex`), with the session in
  `session.reg` applied.

It needs the host's MinGW-w64 GCC 12: `x86_64-w64-mingw32-gcc`, plus
`i686-w64-mingw32-gcc` for WoW64. It is a documented prerequisite, not a
pin; another GCC version builds different PE files.

## Run a program

```console
$ . out/wine.env
$ wine/run.sh --page TREE --dist $WINE64_DIST --prefix $WINE64_PREFIX app.exe
```

That runs `app.exe` in the page on a bench box (`BENCH_SH`; `run.sh -h`).
A 32-bit program runs in the 64-bit process through WoW64.

## Known gaps

The 64-bit install has no:

- **GStreamer**: winegstreamer is left out (`WINE_GSTREAMER=1` keeps it, but
  it cannot start), so media that Wine decodes through GStreamer does not play.
- **Gecko**: wine-gecko, the engine behind mshtml, is not shipped. Pages
  run with `mshtml=` in `WINEDLLOVERRIDES`, and HTML views (mshtml, the
  WebBrowser control, hh.exe) do not render.
- **Mono**: wine-mono is not shipped, and pages run with `mscoree=`, so .NET
  Framework programs do not start.

## Legacy: 32-bit Wine

`make wine32` (this directory's `Makefile`, `wine32.pins`' Wine commit)
builds the older 32-bit-only Wine. Its PE side is staged from the host's
Wine 11.15 (`wine/legacy32/nt-stage-wine.sh`). It stays only until WoW64 replaces it,
and no new work goes into it. `wine/run.sh app.exe` runs a program
on it natively, and the `Makefile` header lists its settings.
