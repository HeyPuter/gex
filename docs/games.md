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

It renders c1a0 at about 9 frames/s on the real GPU (`-nomsaa`).

The black window came from the bundle's hard links. `nt.html` turned each
hard link into a copy, and glibc's loader identifies a loaded library by
device and inode. So `libGL.so.1` and `libEGL.so.1` (one file on the host)
loaded as two libraries, each with its own gex-gl ring and its own GL device.
HL created its framebuffer's texture through one and attached it through the
other, which left the framebuffer incomplete. Hard links are now real links
in the VFS (`gex_add_hardlink`). `tests/games/src/sdl-fbo-blit.c` reproduces
HL's present path on its own, and `info` prints the GL version, the
framebuffer queries and the extension list.

Still open:

1. **MSAA:** without `-nomsaa`, the main framebuffer is incomplete
   (multisample FBO) on gex_wgpu.
2. **V8 out of memory.** One real-GPU run ran the renderer out of heap. Each
   guest thread is a Worker with its own copy of the dispatch tables, and they
   grow to millions of entries (table 4 reached 4.5M). Function-indexed tables
   (merge plan §9) remove these per-Worker tables; that work belongs to §9,
   not to this stream.
3. **SwiftShader:** the WebGPU device is lost ("destroyed") during the run.

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
and HWND_BROADCAST messages work. With MMX lifted (j2k-codec decodes the
JPEG 2000 images), it shows the PopCap logo and reaches the title screen
("Click to play") natively. The startup MessageBox no longer appears. It was
most likely the images/sunglow load failure.

Next steps:

1. Gameplay natively, then in the browser (`browser-noita.mjs` is the model).
2. **GDI text (WIP commit, parked):** `crates/gex-kernel/src/nt/gdi/font.rs`
   parses the bitmap `.fon` files and answers the win32u text calls that
   user32 and comctl32 make for dialogs. It has never been run. Pick it up
   only if Peggle needs dialog text.

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
