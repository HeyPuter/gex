# Games on gex: Half-Life and Peggle Deluxe

Stream G of the pewasm to gex merge. The goal is to run both games in gex's JIT
tier, first natively (`gexc`) and then in the browser (`web/nt.html`). Nothing
here is specific to one game: every fix lives in gex as general x86, Linux, NT,
or GL behaviour, and each one has its own test under `tests/`.

State as of 2026-10-03:

| | native (`gexc`) | browser |
|---|---|---|
| Half-Life (i386 Linux, GoldSrc) | plays c1a0, about 29 fps | loads c1a0 (needs `-nomsaa`) at 8-9 presents/s on a real GPU, but **the frames are black** |
| Peggle Deluxe (i386 Windows, PopCap) | gets to its first MessageBox; text is still in progress (WIP commit) | not tried yet |

## Half-Life

### Native

You need:

- A Steam install of Half-Life. Use a copy (for example
  `work/games/halflife`), because the game writes into its own directory.
- gex's guest GL for i386: `GEX_CC="gcc -m32" guest/gl/build.sh` (gives you
  libGL.so.1, libEGL, and libGLESv2 on gex-gl).
- gex-steam for i386: `guest/steam/build.sh` (libsteam_api with no Steam
  client, which provides an auth ticket and the private vtable slots that
  hw.so calls).
- CEF's GTK2 dependencies from the Steam runtime
  (`~/.local/share/Steam/ubuntu12_32/steam-runtime/usr/lib/i386-linux-gnu`),
  linked into one directory (`GEX_HL_FALLBACK`). chromehtml.so (CEF) is loaded
  every time and needs an X display. Natively, the host's X display provides
  it.

```
cd work/games/halflife
gexc run --no-cgen --gl --wayland $XDG_RUNTIME_DIR/hlwl --wayland-size 800x600 \
  --env LD_LIBRARY_PATH=$STEAM_I386:$GL_I386:$PWD:$FALLBACK \
  --env SDL_VIDEODRIVER=wayland --env SDL_AUDIODRIVER=dummy --env HOME=$HLHOME \
  ./hl_linux -novid +map c1a0
```

Run games on cores 5 and 11 (`taskset -c 5,11`). Add `--wayland-out DIR` to
write every presented frame as a PPM. Screenshots:
`work/games/screens/halflife-{menu,c1a0}-native.png`.

### Browser

```
TMPDIR=/tmp/gexg HL_DIR=... GEX_GL_I386=... GEX_STEAM_I386=... GEX_HL_FALLBACK=... \
  node tests/games/browser-halflife.mjs --shot DIR --hl-args -nomsaa [--map c1a0]
```

`TMPDIR` has to be a short path. Otherwise Chromium's socket path is too long.
`GEX_WEBGPU_SOFTWARE=0` selects the real GPU. The script bundles the game,
gex's libraries, the GTK2 fallback, and the host files that Half-Life opens
(`tests/games/halflife-system-files.txt`, collected with a native strace) into
`web/public/gex/nt/halflife.tar`. Delete the tar to rebuild it. The script
opens `nt.html` with `exe=/hl/hl_linux&gl=1&glbackend=wgpu&hostx=1` and waits
for the map, then reports presents/s.

What made it this far:

- `hostx=1` gives the page an X server (Xwayland as wasm,
  `web/build-xwayland-wasm.sh`). CEF/GTK get their X display from it, so no
  CEF stubs are needed.
- `/dev/shm` is one filesystem shared by all processes in the browser shim
  (CEF's processes use shm_open across process boundaries).
- Stale translations are now retired after each syscall in the browser shim
  (dlclose followed by dlopen at the same address made libudev jump into the
  middle of an instruction).
- `nt.html` resolves the bundle's hard links and accepts Linux ELF programs.
- EGL's default context version is now 1.0 with the compatibility profile.
  Before, it was 4.4 core, and HL could not build its main framebuffer.
- `?mesaenv=K=V,...` passes environment variables to Mesa in the GL worker
  (for example `MESA_DEBUG=incomplete_fbo`).

Blockers, in order:

1. **Black frames in the page.** The map loads and the compositor counts
   presents, but the canvas stays black. **This is not the driver.** I
   replayed a native capture of the same stream (with `-nomsaa`, the first 600
   frames) through `tests/gl-replay.mjs` on softpipe, then through
   `web/public/mesa-wgpu-replay.html` (`allowerrors=1`) on gex_wgpu with a real
   GPU. Both produced the c1a0 corridor with the same mean (32.6,37.2,38.2 and
   coverage 45.7%). The cause is therefore in the live path:
   - how the window's GL surface gets presented, or
   - something the browser run sends that the native capture does not.

   HL ends each frame by blitting a texture-rectangle FBO to framebuffer 0
   with `glBlitFramebuffer`. A standalone version of that blit does work in
   the page.

   Next step: run a small i386 SDL2 program that does the same thing (FBO,
   blit to 0, swap) under `nt.html`. If it is also black, the problem is the
   Linux-guest presentation path. If it renders, capture HL's live stream in
   the browser and diff it against the native capture.
2. **MSAA:** without `-nomsaa`, the main framebuffer is incomplete
   (multisample FBO) on gex_wgpu.
3. **SwiftShader:** the WebGPU device is lost ("destroyed") during the run.
   One real-GPU run also ran out of V8 heap.

## Peggle Deluxe

The game directory is `Peggle Deluxe/`. Make a run copy (`work/games/peggle`)
and extract the real executable from the DRM wrapper with
`tests/games/popcap-unwrap.py Peggle.exe popcapgame1.exe`. Stage the Wine prefix
with `tools/nt-stage-wine.sh`, which now also stages Wine's bitmap fonts into
`drive_c/windows/Fonts`. Then run:

```
cd work/games/peggle
GEX_NT_STRICT=1 gexc run --no-cgen --wine-prefix $PREFIX --gl --wayland-out DIR popcapgame1.exe
```

What works: it loads, bass.dll's self-check passes (tier-1 SMC guard fix),
HWND_BROADCAST messages work, and the game reaches its first MessageBox.

Next steps:

1. **GDI text (WIP commit):** `crates/gex-kernel/src/nt/gdi/font.rs`.
   - It parses the bitmap `.fon` files (NE resources, FNT v2/v3).
   - It answers the win32u calls that user32 and comctl32 make for dialog
     text: GetTextExtentExW, GetTextMetricsW, GetTextFaceW, GetCharWidthW,
     GetCharABCWidthsW, GetGlyphIndicesW, and ExtTextOutW (alignment, opaque
     and clipped).
   - It builds but has not been run against Peggle yet. Test it on the
     MessageBox first, then on `tests/nt` with a test of its own.
2. **images/sunglow fails to load.** The JPEG 2000 path is j2k-codec, which
   uses MMX. **MMX has to be implemented in the lifter**, because removing it
   broke i386 glibc, whose ISA level requires MMX
   (`tests/x86-programs/glibc/run.sh` guards that now). After that, find out
   why sunglow fails.
3. Gameplay natively, then in the browser (`browser-noita.mjs` is the model).

## Measurement scripts (not written yet)

The plan is for `tests/games/*.sh` to cover native and browser variants of
each game: fps, time to gameplay, and scripted input. `browser-halflife.mjs`
already reports time to map and presents/s.

## Tools that came out of this

- `GEX_TRACE_INSNS` + `tools/x86-difftrace`: differential instruction tracing
  between gex and native (gdb), which found the bass.dll SMC bug.
- `GEXGL_TRACE_GENERATED`: logs generated compat-GL commands.
- `tests/gl-replay.mjs` (softpipe) and `web/public/mesa-wgpu-replay.html`
  (gex_wgpu) replay the same capture. Comparing the two separates a driver bug
  from a transport or presentation bug.
