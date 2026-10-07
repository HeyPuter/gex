# Games on gex: Half-Life, Peggle Deluxe and Unity games

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

## Unity games (stream U)

Muck (Linux x86-64, Unity 2019.4.19f1), Hollow Knight (Windows x64,
Unity 6000.0.61f1) and Rain World (Windows x64, Unity 2020.3.45f1). All
three are Mono (MonoBleedingEdge), not IL2CPP. Run them in place from the
Steam install, read-only; write prefixes, homes and logs elsewhere.

State as of 2026-10-04: Muck reaches its main menu natively (`--fjit
--mem64`, status below); Rain World and Hollow Knight have not been run
since. Every one of them needs more address space than a wasm32 guest has
(below), which `--mem64` provides.

### Running

All three need a 64-bit guest memory: `gexc run --mem64` (docs/mem64.md), or
nothing at all after the first run, which records the image when it runs out
of address space.

- **Muck** (natively): guest GL for x86-64 (`GEX_CC=gcc guest/gl/build.sh
  DIR`) and an X display: Xwayland run *inside the guest*, a client of gex's
  own compositor (no Xvfb, no host socket bridge). A small script started
  under gexc runs `Xwayland :N -geometry 800x600 -shm -ac -nolisten tcp`
  with no `LD_LIBRARY_PATH` (gex-gl's libGL is not Xwayland's), waits for
  `/tmp/.X11-unix/XN`, then `DISPLAY=:N exec`s the game (`-ac`: the socket
  exists only in the guest's namespace):

  ```
  GEX_FJIT_CACHE=1 gexc run --fjit --mem64 --gl --wayland $XDG_RUNTIME_DIR/x \
    --wayland-size 800x600 --wayland-out DIR --env LD_LIBRARY_PATH=GLDIR \
    --env HOME=... --env MONO_DEBUG=explicit-null-checks \
    /bin/sh inguest.sh 71 XWLOG ./Muck.x86_64 -logFile LOG \
    -screen-width 800 -screen-height 600 -screen-fullscreen 0
  ```

  Xwayland's lock and socket node land in the host's /tmp; remove only your
  display's afterwards. (A copy of the script: work/unity-resume/inguest.sh.)
  Unity 2019.4's SDL is X11-only (GLX through gex-gl's libGL). Its window's
  frames are presented by gex-gl on the host GPU and written to DIR
  (`GEX_PPM_EVERY=N`, `GEX_FPS=1`).
- **Hollow Knight / Rain World:** a staged prefix (`tools/nt-stage-wine.sh`),
  `gexc run --fjit --mem64 --wine-prefix P --gl --wayland-out DIR game.exe`.
  Hollow Knight's build has no OpenGL renderer (`-force-glcore` fails with
  "OpenGL Core was not built"), so it needs D3D11 on wined3d. Rain World:
  `--env WINEDLLOVERRIDES=winhttp=b` keeps BepInEx's doorstop proxy
  (`winhttp.dll` in the game directory) from loading, without touching the
  install.
- **Mono's null checks.** Mono turns a fault on a null object reference into
  NullReferenceException, and gex has no page protection in the low region,
  so the fault does not happen. The run scripts set
  `MONO_DEBUG=explicit-null-checks` (Mono configuration, not a game change).
  The general fix is low-page protection in gex: a fault on an access below
  the guest's floor, which other runtimes that rely on null faults (JVMs,
  .NET) need as well.
- **Start-up time.** Under `--fjit` the first run spends almost all of its
  time translating code it has not seen (Cranelift about 60%, recovery about
  15%): Muck took over 15 minutes of wall time to reach Mono's assembly load,
  which takes 0.1 s natively. `GEX_FJIT_CACHE=1` keeps compiled
  translations on disk (about 150 KB each, about 1 GB for Muck's start-up)
  so later runs load them; AOT units for UnityPlayer.so and libmonobdwgc are
  the larger lever.
- **What Unity needed from the kernel:** `sched_getparam` and friends (Mono
  aborts when `pthread_getschedparam` fails), `/proc/stat`, and
  priority-inheritance futexes (`FUTEX_LOCK_PI`/`UNLOCK_PI`; without them a
  `PTHREAD_PRIO_INHERIT` mutex never woke its waiter and the player
  deadlocked with every thread asleep). On Windows: crypt32's Unix side,
  rsaenh's registration, `NtNotifyChangeKey`, `NtRenameKey`, in-process named
  pipes (`CreatePipe`), `SystemSupportedProcessorArchitectures`.
- **Steam.** Without a Steam client, Muck's Host Lobby does nothing (its
  game is a Steam lobby; there is no offline mode), so Muck's gameplay needs
  an x86-64 gex-steam with lobby and loopback P2P. Hollow Knight stops on its
  title screen natively without Steam; Rain World gets through EOS errors.
- **Status (2026-10-04, later):** Muck reaches its main menu natively
  under `--fjit --mem64` (Host Lobby, Join Lobby, Settings, Achievements,
  About, Quit over the 3D island; work/games/screens/muck-menu-gex-fjit-mem64.png)
  at 17-23 fps at 800x600 on two cores (cores 1,7; a GL capture was being
  written, and there is no pewasm Unity run to pair it with). From a cold
  translation cache the menu took about 100 minutes of wall time; most of
  it is translation. What it took, each with a test: tier-1's self-modification
  guard read code bytes at an i32 address in an M64 module; the function
  JIT translated a JIT's writable-and-executable code without guards (it now
  leaves it to tier-1); tier-1 failed a miss whose page another thread's
  guard retired mid-translation (it now translates again); a worker
  thread's `exit_group` or fatal error left the process hanging; and gex-gl's
  `glMapBufferRange` shadow uploaded zeros over bytes not written or not
  flushed. Muck logs a NullReferenceException after "Initializing packets."
  (no Steam) and goes on.
- **Slow menu frames look tiled.** While a frame takes seconds, Muck's
  menu draws its scene tiled (8x11, later 23x30 copies). The GL capture shows
  that this is Muck's own data: its full-screen quad's texcoords are
  0.5 +/- 11.5 (and 15.3 vertically, 4:3), sampled with REPEAT, and `_MainTex_ST`
  is (1,1,0,0). It looks like a per-frame zoom that is unstable at
  Unity's 0.333 s maximum deltaTime, and the menu draws correctly once frames
  are fast.
- **Mono's code under the function JIT** now runs on tier-1, each block
  checking its bytes on entry (`Block::guard`). This is correct, but managed
  code gets no function-tier speed. What would recover it: a function-level
  byte check for code in writable-and-executable pages, with direct
  transfers into code not yet seen leaving through dispatch, or a store-side
  check of a code-page bitmap.
- **Rain World (2026-10-04, paused for a reboot):** run natively as
  `rw.sh` in work/unity-resume/rw/ does (`--fjit --mem64 --wine-prefix P
  --gl`, `--env WINEDLLOVERRIDES=winhttp=b`, 800x600, a copy of the staged
  prefix and a hard-linked copy of F's scratchpad/fjit/cgcache-peg units).
  Its build has **no OpenGL Core shaders** ("Forced GfxDevice 'OpenGL Core'
  was not built from editor"), so it runs D3D11 on wined3d, as Hollow Knight
  must: "Direct3D 11.0 [level 11.1]", Mono's reload completes (430-500 s
  cold), Rewired's input initializes, the first scene loads ("UnloadTime")
  and the window presents its first (black) frame. Fixed on the way, each
  with a test in tests/nt: NtSuspendThread/NtGetContextThread for Boehm's
  stop-the-world (it aborted with a message box), WINEDLLOVERRIDES (BepInEx's
  winhttp.dll doorstop loaded regardless and MonoMod died), the display
  configuration queries (Unity allocated 0xffffffff * 72 bytes), raw input,
  display device names, per-monitor DPI; and a regression of mine (a
  Windows worker thread's ExitThread ended the process).
  **Where it stops:** run r7 died about 10 minutes after the scene load of
  an unhandled illegal instruction in Mono's JIT code (0xa3ea00a, no
  module). gexc now logs the bytes (`gex-nt: illegal instruction at`); run
  r8 was stopped by the reboot before reaching that point (presents=1, 36
  min in). **Resume:** rerun `rw.sh r9 7200 --fjit --mem64` (with
  `RENDER=` and `CGDIR=.../cgpeg`; about 40 minutes cold), read the
  illegal-instruction line, decode the bytes, fix the decoder or the
  trap's handling. Still unimplemented and seen: NtCreateUserProcess (the
  crash handler), NtSetInformationThread class 1000, NtUserSetWindowsHookEx,
  secur32's Unix side.
- Knobs: `GEX_FPS=1` (presents per second), `GEX_PPM_EVERY=N`,
  `--wayland-out null`, `GEX_STRACE_STACK=N` (stack words at each syscall
  under `--strace`), `GEX_NT_TRACE` (logs each NtAllocateVirtualMemory
  request), `GEX_FJIT_CACHE=1`, `GEX_DUMP_BAD_WASM=DIR` (keep a translation the
  engine rejects as DIR/PC.wasm).

### Address space: why none fits in wasm32

Unity's memory manager (`MemoryManager::VirtualAllocator`, named from Muck's
`UnityPlayer_s.debug`) gives every allocator block its own 256 MiB-aligned
slot and looks blocks up by `address >> 28`, so two blocks never share a
slot whatever their size. Baselib reserves each one by mapping 512 MiB - 4 KiB
`PROT_NONE` and trimming. A gex x86-64 guest has 2 GiB for mappings
(`MMAP_TOP`, the dispatch directory's reach), and the NT allocator 2 GiB.

Slots used, measured natively (Muck on Linux with an `mmap` backtrace shim,
the Windows games under host Wine 11.15 with `WINEDEBUG=+virtual`):

| game | slots at startup | what they are |
|---|---|---|
| Muck | 19 at the menu | 4 job temp allocators (ThreadsafeLinearAllocator), 1 bucket allocator, 1 main-thread temp (TLSAllocator), 9 dynamic-heap pools (main, per-thread, gfx, typetree, file system, ...), 4 large allocations (gfx command buffer, async upload queue, a texture, FMOD samples) |
| Hollow Knight | 17 at the title screen | (no symbols; same Baselib pattern) |
| Rain World | 16 after engine init | (no symbols; same pattern) |

Every allocation larger than a heap block takes a slot of its own, so the
count grows with what a level loads. Launch knobs tried:

- Muck: `-job-worker-count 0/1`, `-force-gfx-st`, both together, even
  `-nographics`: 19 every time. Unity 2019.4 has no `memorysetup-*` keys.
- Hollow Knight (Unity 6 reads `-memorysetup-KEY VALUE` from the command
  line; the `KEY=VALUE` form is ignored): every heap block size at 256 MiB
  takes it from 17 to 13 at the first screen; 1 MiB blocks leave it at 17;
  `-job-worker-count 1` leaves it at 17.
- Rain World (2020.3): only `-job-worker-count`, no memorysetup keys.

Raising `MMAP_TOP` does not get there: up to 3 GiB costs a larger directory
(it must end below the non-PIE base 0x400000, which allows exactly 3 GiB) and,
for Windows guests, moving the host-call slots and trampolines at
0x30_0000-0x40_0000 that a larger directory runs into; past 3 GiB the
directory has to leave the low region, and in the browser the NT shim's heap
above 0x8000_0000 has to move. 3.75 GiB gives at most 14 slots, fewer once
images, stacks and the 512 MiB transient reservation are placed. So all
three need a 64-bit guest memory (docs/mem64.md).

## Games matrix (stream G, 2026-10-05): untested non-.NET games

`tests/games/matrix.sh` (games listed in `tests/games/matrix.txt`) runs each
game natively under `gexc --no-cgen --gl` with a time limit, from a copy of
its directory (hard links, small files copied, so the originals are never
written) on a hard-linked copy of the staged prefix, and prints
`slug arch stage frames secs end reason`, keeping first.png/last.png.
Correctness only: no timing on gooner.

```
. ~/gex-sandbox/scratch/env.sh
guest/steam/build-win.sh ~/gex-sandbox/scratch/G/steam-win      # gex-steam (below)
GEX_CC=gcc guest/gl/build.sh ~/gex-sandbox/scratch/G/gl-x64      # Linux x86-64 games
TMPDIR=~/gex-sandbox/scratch/G/tmp GEX_STEAM_WIN=~/gex-sandbox/scratch/G/steam-win \
  GEX_GL_X64=~/gex-sandbox/scratch/G/gl-x64 tests/games/matrix.sh --time 300 --only undertale,katana-zero --out DIR
```

Run a *copy* of matrix.sh for long batches (editing the script under a
running bash breaks it at the end of its loop). Never run a game in place:
the first triage did, and wrote steam_appid.txt, logs and configs into
games/ (removed again).

### gex-steam for Windows (guest/steam/build-win.sh)

Without a Steam client the game's steam_api.dll fails, and GameMaker games
then quit or wait ("Steam must be running to play this game"). gex-steam's
Windows build replaces steam_api.dll/steam_api64.dll (matrix.sh swaps it in
the copy; the original is kept as `.orig`): clang for the MSVC ABI
(thiscall, callee-popped arguments, sret for aggregates, MSVC overload
order), every interface version (242, generated by `gen_versions.py` from
per-version headers: `~/src/gbe_fork/sdk/steam`, a sparse clone of
github.com/Detanup01/gbe_fork, plus `~/src/Steamworks.NET/CodeGen/steam/steam_api.json`;
neither is in the tree). Pre-1.37 accessors (SteamUser() ...) use the
versions found in the `.orig` DLL. Undertale and Katana ZERO get through
Steam init with it. `GEX_STEAM_TRACE=1` (GAME_ENV) is meant to log
lookups but writes to the GUI process's stderr handle, which shows
nothing yet; use OutputDebugString next time.

### Scope (user, 2026-10-05)

gex's NT layer is legacy: stream W's native Wine replaces it, and no more
NT-layer work is done. Stream G covers i386 Windows games on W's native
Wine (`wt/wine-slim`'s gexc with a copy of `scratch/W/pfx-gui-slim`), Linux
builds, and browser runs of what reaches gameplay. x64 Windows games wait
for 64-bit native Wine. Isaac: Rebirth is SteamStub-wrapped (it relaunches
through steam://run): parked for the user. Antichamber's UDK.exe has a
managed entry point: it needs a CLR.

### Where the games got on the NT layer (2026-10-05)

With gex-steam and the input scripts in tests/games/*-input.txt, natively,
on master's translated-Wine path plus NT fixes that are now parked (below):

| game | arch | reached | notes |
|---|---|---|---|
| Undertale | i386 | **gameplay**: name entry, walking in the Ruins | menu frame 650, gameplay 1500 |
| Retrocycles | i386 | **gameplay**: a light-cycle match | menu 200, gameplay 1000; also on W's native Wine |
| Katana ZERO | i386 | **menu** | GameMaker's menu ignores clicks: keyboard script |
| N++ | i386 | first screen, garbled background | glClearBufferfv on the default framebuffer did nothing on gooner's llvmpipe pbuffer |
| Momodora | i386 | data loaded, no frame in 900 s | |
| 5D Chess | x64 | gameplay (a move) | waits for 64-bit native Wine |
| Geometry Dash | x64 | menu | waits for 64-bit native Wine |
| DELTARUNE, Will You Snail | x64 | not mapped | fixed base 0x140000000, no relocations: needs code above 4 GiB (docs/mem64.md) |

Screens: work/games/screens/ (undertale-ruins, retrocycles-gameplay,
katana-zero-menu, 5d-chess-board, geometry-dash-menu; `-native.png`).

### What native Wine must provide (from these games)

Each of these stopped a game on the NT layer. The NT-layer versions, with
tests in tests/nt, are parked on merge/games2 (not for master); native Wine
needs the same behaviour:

- **Window hooks** (`SetWindowsHookEx`, `CallNextHookEx`, `CallMsgFilter`):
  Katana ZERO, Spelunky 2, Hyper Light Drifter set a low-level keyboard
  hook. W's native-Wine path also stops here today (Katana ZERO,
  `NtUserSetWindowsHookEx`). Test: tests/nt/src/hooks.c.
- **GL uploads larger than 8 MiB** (one ring command): Undertale's
  2048x2048 texture pages (16 MiB), Retrocycles (8 MiB). W's path still hits
  "a GL command of 8388664 bytes" with Retrocycles. Test: glbig.c.
- **COM classes registered** (what wineboot's DllRegisterServer does):
  DirectInput8 for SDL_Init (N++, OneShot), DirectSound/MMDevice for
  SDL_mixer (5D Chess), WIC. W's path: Retrocycles' dsound gets
  REGDB_E_CLASSNOTREG. Test: com.c; a generator from the DLLs' WINE_REGISTRY
  resources is tools/gen_nt_classes.py on merge/games2.
- **No audio device and no schannel without stopping**: Wine's audio
  drivers (winepulse.drv ...) and secur32's Unix side absent, the program
  falls back to silence or no TLS.
- **OpenGL 3.3 core-profile contexts** (wglCreateContextAttribsARB with the
  core bit): 5D Chess (SDL2), Cruelty Squad (Godot 3). Test: glcore.c.
- **I/O completion ports** (Spelunky 2). Test: iocp.c.
- **`\??\unix\...` paths** (Haxe: /etc/.haxepath; Dicey Dungeons).
  Test: unixpath.c.
- **GetKeyNameText** (Spelunky 2), **SetThreadExecutionState**,
  SPI_GET/SETFOREGROUNDLOCKTIMEOUT and MOUSETRAILS (OneShot),
  **VirtualLock** (Spelunky 2): tests keyname.c, sysmisc.c.
- **A synchronous RegNotifyChangeKeyValue** that waits (Geometry Dash's
  watcher thread).
- **NtAreMappedFilesTheSame** (Geometry Dash), **NtSetContextThread on
  another thread** (Mosa Lina's HashLink GC), **NtCreateUserProcess** for
  crash handlers (Cruelty Squad): seen, not done.

### On native Wine (master c5620656 + this branch, prefix a copy of scratch/W/pfx-gui-slim), 2026-10-06

| game | reached | stops on (repros for W in scratch/G/wrepro) |
|---|---|---|
| Undertale | **gameplay** (the Ruins), but the intro picture and the player's sprite are missing | 16 MiB texture uploads fail (glbig) |
| Retrocycles | **gameplay** | 8 MiB uploads (glbig), dsound's MMDevice class (com) |
| Momodora | 1300 black frames | uploads (glbig), wined3d draw-quad GL_INVALID_OPERATION, glGetShaderSource/glGetAttachedShaders |
| Katana ZERO | no frame | NtUserSetWindowsHookEx (hooks) |
| N++ | no frame | SDL_Init's DirectInput8 class (com) |

Linux builds (no Wine): Retrocycles reaches its language menu on Xwayland
inside the guest (SDL 1.2 and libpng12 from Debian packages in
LINUX_LIBS); Baba Is You gets past Steam (gex-steam Linux) and D-Bus
autolaunch, creates its GL window, no frame yet. Input for Linux games
needs scripted Wayland input (GEX_WL_INPUT, on stream U's branch).

### Linux builds: scripted input, Steam, X11 (2026-10-06)

- Input: `GEX_WL_INPUT` (master), timed in seconds from the Nth frame;
  matrix.sh passes a Linux game's input file (`*.wl`) as it.
  tests/games/retrocycles-linux.wl takes Retrocycles through its language
  menu and First Setup to the welcome screen. With `frames 1` the clock
  started on Xwayland's first frame, long before the game's menu.
- Steam: gex-steam for Linux (`guest/steam/build.sh OUT clang++ [-m32]`;
  GCC 12 lacks `-nostdlib++`), swapped in by matrix.sh with
  GEX_STEAM_LINUX64. Smoke test: tests/games/steam-smoke.sh (i386 and
  x86-64, on the host).
- X11: Baba Is You's static SDL2 has no Wayland, and SDL 1.2 has none, so
  both run as `linux-x11` on Xwayland inside the guest. Xwayland runs
  xkbcomp (translated) to compile its keymap, which takes minutes on a
  loaded gooner; X clients wait for it.
- Baba Is You (Chowdren) needed: gex-steam with an older stats interface
  (STEAMUSERSTATS 012), no D-Bus autolaunch (libdbus ran dbus-launch, which
  waited on the display), `--lazy` (eager translation of the 9 MB
  executable is 216 MB of wasm). It creates its GL window and stops after
  its renderer string, waiting on an X reply.
- Retrocycles (Linux): **gameplay natively** (language menu, First Setup,
  welcome, matches; tests/games/retrocycles-linux.wl, menu 650, gameplay
  6000). gexc had died of SIGSEGV in the host's Mesa when the first match
  started: the native GL host passed a glDrawElements' client-side index
  pointer (guest memory) to the host's GL as an offset. Fixed in gex-gl's
  native host (b7556e59, with a test).
- In the page (tests/games/browser-linux.mjs, hostx=1): Retrocycles starts,
  its X windows appear and Mesa draws (hundreds of thousands of draws), but
  the canvas stays black: a GLX window's frames on an X11 window do not
  reach the page's compositor yet (Half-Life's GL is on a Wayland surface;
  only its CEF uses X). Next: how the page shows a GL surface made for an X
  window (gexgl eglCreateWindowSurface with an X window id under hostx).
- Baba Is You: from its game directory (`@bin64/Chowdren`) it finds its
  assets. It requires six GL extensions by name and showed "OpenGL
  extension 'GL_ARB_shader_objects' not supported." in an SDL message box
  (a forked child whose X font request failed, so it only hung; the line
  `Renderer: R - V - ` is meant to end empty). The guest GL withheld the
  extension for one missing command, glDeleteObjectARB; with it Baba
  renders. Run fullscreen (its default), the frames are
  cropped: SDL believes the window is the screen's size while the GL
  drawable keeps the window's 854x480 (no X window manager in the guest to
  carry the fullscreen request). It keeps its settings in
  ~/.local/share/Baba_Is_You from getpwuid(), not $HOME.
- Xwayland in the guest: `gexc run --bind` puts tests/games/xkbcomp-cache.sh
  over /usr/bin/xkbcomp (matrix.sh does it for linux-x11 games), so the
  keymap is compiled once (/tmp/gex-xkm-cache-UID) and later runs copy it.
- GLX: contexts are what glXCreateContextAttribsARB asked for
  (tests/games/glx-context.sh: 3.2 core, 2.1 compatibility, ES 2.0 after
  SDL's throwaway context).
- The page, resolved (2026-10-06): Retrocycles renders in the page,
  windowed and fullscreen (work/games/screens/retrocycles-linux-language-browser.png).
  The black canvas was SDL 1.2's fullscreen: it maps an override-redirect
  window over the screen and reparents its GL window into it, so the
  unpainted window covered the one the frames were attached to. The guest
  GL now attaches to the top-level window and follows reparenting
  (tests/games/src/glx-quad.c shows it in the page). A correctness run on
  edger's GPU (one run, no timing) showed the compositor repainting
  normally, so there was no starvation; on gooner's SwiftShader the GL
  bridge still times out after a screen change (a batch over 30 s).
- Baba Is You: **gameplay natively** (2026-10-06; tests/games/baba.wl,
  gameplay 4000): through its credits screen into the first level, where
  Baba walks right, pushes the rocks and wins; the world map follows.
  matrix.txt's `seed=seed/baba@~/.local/share/Baba_Is_You` mounts a fresh
  copy of tests/games/seed/baba (fullscreen=0) there for each run; nothing
  reaches the real home. What it took:
  - The "intro screen" is the credits screen (IGF, Nordic Game Jam, MP2),
    and it waits for a click; then a fresh save loads ~3,900 sprite PNGs on
    the main thread and goes straight into the first level (no title menu).
  - Chowdren reads the mouse and keys as state once per game frame, so the
    script's `click` and `tap` (press and release 0.1 s apart) fell inside
    one slow frame and were missed. baba.wl holds every press for a second
    or more (`button 272 1` / `key right 1` ... `0`).
  - No key worked at first because the seed said `controlinit=1`, which
    tells Baba its key bindings already exist; with none in the file it had
    no bindings at all. The seed now leaves controlinit out, so Baba writes
    its defaults.
  - X focus is not the problem for Baba (SDL focuses its own window), but
    matrix.sh now starts tests/games/xfocus.c (stream U's) for every
    linux-x11 game, since some need it (NOFOCUS=1 leaves it out).
  - Runs that "stuck" before any frame were Xwayland waiting on xkbcomp:
    every process started under gex is translated afresh (a short one is
    seconds of Cranelift; on a loaded gooner, minutes), and the xkbcomp
    cache's cache hit was six processes (id, mkdir, cat, cmp, rm, cp). It
    now uses shell builtins and one cp (tests/games/xkbcomp-cache-test.sh).
  - A gexc thread panic, "overflow when subtracting durations", in a timed
    futex wait on a loaded machine (two clock readings around the deadline):
    fixed in gex-native's perform_block (82480885, test time_left_tests).
  - `--fjit` with GEX_FJIT_CACHE=1 caches Cranelift's output only; the
    analysis and wasm emission still run in every process. No tier-share
    line: Baba never exits on its own, and the counts print at exit.
- Retrocycles in the page on edger's GPU (2026-10-06, one correctness run,
  no timing): first frame, the input script, past present 6000 (gameplay)
  and on to the script's arrow keys. The compositor's `lit` readback of
  its own output changed with each scene (3,127 to 104,775 sampled pixels),
  so the page drew the game; the harness's screenshots stayed black on the
  real GPU (Chrome's screenshot misses the WebGPU canvas there; on gooner's
  SwiftShader the same screenshots show the game). A readback-based
  screenshot in browser-linux.mjs would fix the pictures.
- Process start under native gexc: the module cache already covers runtime
  translations (a second run of `sh -c cat` adds no entries), so wasmtime's
  own cache would add nothing. Within one run, though, each new process's
  translations come out as different wasm and miss: about 60 identical
  short processes (scratch/G/hang/loop20.sh) left 1,008 new entries.
- Earlier, the page (browser-linux.mjs; own ports 5291/9431): Retrocycles' GL
  frames were attached to SDL 1.2's child window, which no compositor
  window has; they now name its top-level window. The run still shows
  black: Mesa keeps drawing (about 25 draws a second under SwiftShader on
  a loaded gooner, immediate-mode draws, each with its own buffer map),
  the compositor worker that hosts Mesa repaints only a handful of frames
  (its animation loop does not get a turn while the GL ring is served), and
  after a screen change one batch takes over the bridge's 30 s limit. Next
  steps: a real GPU (edger) to separate slowness from the compositor
  starving, and letting the compositor repaint between GL batches.

### Resume (stream G)

Gate branch merge/games2-gate (PE loader fixes, gex-steam for Windows,
matrix and scripts, this doc). The NT-layer work stays on merge/games2.
Scratch ~/gex-sandbox/scratch/G: `launch.sh NAME TIME SLUGS [args]` runs a
private copy of matrix.sh (env: GEXC, GEX_WINE_PREFIX), `sheet.py` makes
contact sheets. Native Wine: `GEXC=~/gex-sandbox/wt/wine-slim/target/x86/release/gexc`,
prefix a copy of `scratch/W/pfx-gui-slim` (never W's own). Repros for
missing pieces on that path go to W through the coordinator.

## Tools that came out of this

- `GEX_TRACE_INSNS` + `tools/x86-difftrace`: differential instruction tracing
  between gex and native (gdb), which found the bass.dll SMC bug.
- `GEXGL_TRACE_GENERATED`: logs generated compat-GL commands.
- `tests/gl-replay.mjs` (softpipe) and `web/public/mesa-wgpu-replay.html`
  (gex_wgpu) replay the same capture. Comparing the two separates a driver bug
  from a transport or presentation bug.
