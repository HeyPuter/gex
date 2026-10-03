# Games on gex: Half-Life and Peggle Deluxe

Stream G of the pewasm to gex merge. The goal is to run both games in gex's JIT
tier, first natively (`gexc`) and then in the browser (`web/nt.html`). Nothing
here is specific to one game: every fix lives in gex as general x86, Linux, NT,
or GL behaviour, and each one has its own test under `tests/`.

State as of 2026-10-03 (evening):

| | native (`gexc`) | browser (`nt.html`, real GPU) |
|---|---|---|
| Half-Life (i386 Linux, GoldSrc) | plays c1a0, 16-29 fps | renders c1a0 (needs `-nomsaa`), 7-9 fps |
| Peggle Deluxe (i386 Windows, PopCap) | plays Stage One: name entry, shots, scoring; 7-12 fps | plays Stage One with the same scripted input; 3-5 fps |

Screenshots: `work/games/screens/` (`halflife-c1a0-{native,browser}.png`,
`peggle-stage1-{native,browser}.png`).

## Measurement scripts

Both games are workloads for the other streams. Each script prints `ok` lines
(time to the first frame or the map, time to gameplay, frames/s over
`--measure` seconds) or `FAIL`:

```
tests/games/peggle.sh    [--browser] [--measure S] [--shot DIR] [-- GEXC-FLAGS]
tests/games/halflife.sh  [--browser] [--map M] [--measure S] [--shot DIR] [-- GEXC-FLAGS]
```

- Native runs use `GEXC` (default `target/x86/release/gexc`) in the JIT tier
  (`--no-cgen`); anything after `--` replaces those flags, so the same run
  measures `--cgen` or `--fjit`. Games run on `CORES` (default 5,11).
- `--browser` runs `browser-peggle.mjs` / `browser-halflife.mjs`.
  `GEX_WEBGPU_SOFTWARE=0` picks the real GPU; SwiftShader is the default.
  They need `web/public/gex` built (`GEX_ARCH=x86 shims/browser/build.sh`,
  `web/build-mesa-wasm.sh`, and for Half-Life `web/build-xwayland-wasm.sh`).
- Peggle's input is `tests/games/peggle-input.txt`: `GEX_NT_INPUT`, timed by
  GL presents, so the run is the same natively and in the page, from a
  profile-less start (click to play, the name "GEX", Stage One, its
  instructions, then shots). Gameplay is measured from its `# gameplay N`
  frame. The native script runs on a linked copy of the prefix with an empty
  `ProgramData` (where Peggle keeps its profile).
- Half-Life's workload is `+map c1a0`, the tram ride, which plays the same way
  with no input. Its frames are counted from `--wayland-out` file names
  (`GEX_PPM_EVERY=10` writes every 10th frame, named by its real index).
- Defaults for Half-Life's libraries are `work/games/deps/{gl-i386,steam-i386,
  hl-fallback}` and the game copy `work/games/halflife`; Peggle's is
  `work/games/peggle`.

Measured on this machine with the scripts (JIT tier, shared cores):

| | time to first frame | time to gameplay / map | gameplay fps |
|---|---|---|---|
| Peggle native | 173 s | 332 s | 7.1 (120 s) |
| Peggle browser, real GPU | 43 s | 487 s | 3.4 (150 s) |
| Half-Life native | | 429 s | 16.0 (60 s) |
| Half-Life browser, real GPU (`-nomsaa`) | | 88 s | 7.2 (60 s) |

Native numbers include writing every 10th frame and the GL readback for it,
and the cores were shared with other streams' builds; compare runs made the
same way. Half-Life's native load (429 s) is far slower than the page's
(88 s) and has not been looked at.

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
with `tools/nt-stage-wine.sh`. Then run:

```
cd work/games/peggle
GEX_NT_STRICT=1 GEX_NT_INPUT=... gexc run --no-cgen --wine-prefix $PREFIX --gl --wayland-out DIR popcapgame1.exe
```

or `tests/games/peggle.sh` (above). The window is 1920x1080 with Peggle's
800x600 scaled to 1440x1080 in the middle; `GEX_NT_INPUT` coordinates are
client coordinates of that window.

What made it work:

- bass.dll's self-check (tier-1 SMC guard fix), HWND_BROADCAST messages.
- MMX lifted: j2k-codec decodes the JPEG 2000 images (logo, title screen).
- SetWindowPos on another thread's window runs on the owning thread (Wine's
  WM_WINE_SETWINDOWPOS). In the page, the compositor's Configure is handled
  by whichever thread peeks first, which for Peggle is a helper thread; before,
  that sent WM_WINDOWPOSCHANGING across threads and stopped the process.
- gex's `GEX_*` settings reach the kernel in a page (`nt::Config::settings`,
  `Process::setting`), so `GEX_NT_INPUT` scripts work in the browser, and
  `nt.html`'s `env=` keeps commas inside a value.

Not needed so far: the GDI text WIP (`crates/gex-kernel/src/nt/gdi/font.rs`,
parked commit). Peggle draws all its text itself.

Open:

1. Frame rate. Natively 7-12 fps in the JIT tier, in the page 3-5 fps, where
   Peggle wants 100. Not profiled yet; the main thread is guest time
   (`GEX_NT_STATS`' time split in the page).
2. The page's window has a caption, so a 1920x1080 client area does not fit
   a 1920x1080 page (scrollbars). Input is injected at client coordinates, so
   the script is not affected.

## Tools that came out of this

- `GEX_TRACE_INSNS` + `tools/x86-difftrace`: differential instruction tracing
  between gex and native (gdb), which found the bass.dll SMC bug.
- `GEXGL_TRACE_GENERATED`: logs generated compat-GL commands.
- `tests/gl-replay.mjs` (softpipe) and `web/public/mesa-wgpu-replay.html`
  (gex_wgpu) replay the same capture. Comparing the two separates a driver bug
  from a transport or presentation bug.
