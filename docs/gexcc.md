# gexcc: building Unix programs for gex

Stream C (2026-10-07). Goal (merge-plan §10, §10.1): source compatibility with
any Linux codebase. Ordinary C and C++ projects cross-build to gex's native
wasm32 target with their own build systems, and their own test suites pass
under gexc. Later, glibc itself.

The target is stream W's (docs/wine-native.md §3.10–§3.12):
- clang `--target=wasm32-unknown-linux-musl`;
- musl with gex's `arch/wasm32` (syscalls on the i386 ABI through
  `gex.syscall`) plus libgex;
- wasm-ld PIE modules that import gex's memory and function table, in an ELF32
  container that gexc runs as an ordinary process.

gexcc packages that target as a cross toolchain.

## 1. Using it

### Install

```sh
BUSYBOX_TAR=.../busybox-1.37.0.tar.bz2 LLVM_SRC=.../llvm-project-22.1.8.src \
  tools/gexcc/install.sh PREFIX [MUSL_SRC]   # MUSL_SRC: musl-wasm, gex-wasm32-slim or later
export PATH=PREFIX/bin:$PATH
```

Install options (environment variables):

| variable | effect |
|---|---|
| `GEXC` | the gexc programs run under (default: this tree's `target/release/gexc`) |
| `GEX_BINARYEN` | an existing binaryen (otherwise downloaded) |
| `BUSYBOX_TAR` | builds busybox (defconfig, 400 applets) as the guest's native `/bin/sh` and coreutils (§1.4) |
| `LLVM_SRC` | an LLVM tree of clang's version (22.1.8); builds libc++, libc++abi and libunwind (`build-cxx.sh`). It needs `runtimes cmake llvm/cmake llvm/utils libcxx libcxxabi libunwind libc third-party` |
| `JOBS` | parallel jobs |

What it installs:

| path | what |
|---|---|
| `bin/gexcc`, `bin/gexc++` | the compiler driver (gcc/clang interface) |
| `bin/wasm32-unknown-linux-musl-{cc,gcc,clang,c++,g++,clang++}` | the driver under the names autotools looks for |
| `bin/wasm32-unknown-linux-musl-{ar,ranlib,nm,objdump,ld,...}`, `gexcc-{ar,ranlib,nm}` | LLVM's tools; `ld` is wasm-ld |
| `bin/…-strip`, `gexcc-strip` | does nothing (a program is a container around a wasm module whose names gex binds by) |
| `bin/…-pkg-config`, `gexcc-pkg-config` | pkg-config over the sysroot only, `--static` |
| `bin/gexcc-exec` | the runner programs' launchers exec: gexc with gexcc's defaults (busybox) |
| `bin/gexcc-run PROG ARGS` | `gexcc-exec run --host-env PROG ARGS` |
| `lib/gexcc/` | flags.sh, wasm-elf.py, forkreach.py, hoststub.bin, GexPoll.so, main3.o, empty.o, busybox |
| `sysroot/` | musl (include, libc.a, crt1.o), libgex.a, libgexcc.a, compiler-rt builtins, libc++ (include/c++/v1, lib), Linux UAPI headers (i386), binaryen |
| `sysroot/usr/` | where cross-built libraries install: `--prefix=/usr`, `DESTDIR=$(gexcc --print-sysroot)` |
| `share/gexcc/toolchain.cmake`, `meson-cross.ini`, `config.sub`, `config.guess` | build-system files |

### Build systems

```sh
# autotools
./configure --host=wasm32-unknown-linux-musl --prefix=/usr && make && make check
make install DESTDIR=$(gexcc --print-sysroot)        # a library others use

# CMake (try_run and ctest work)
cmake -DCMAKE_TOOLCHAIN_FILE=PREFIX/share/gexcc/toolchain.cmake ...

# Meson (cc.run() and meson test work)
meson setup build --cross-file PREFIX/share/gexcc/meson-cross.ini

# plain Makefiles
make CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib

# a build system that configures as native (perl's Configure, Python's
# configure without --host): CC=gexcc and pkg-config/library paths into the
# sysroot (see tests/gexcc/corpus/recipes.sh)
```

### Running programs

A gexcc program runs like any binary:
- **Under gex:** `gexc run ./prog`. Inside gex, an exec of another gexcc
  program stays inside gex.
- **On the build machine:** `./prog`, which needs an x86 Linux host with
  IA-32 emulation. The container's entry holds a small i386 launcher
  (`tools/gexcc/hoststub.c`) that re-execs the file as `RUNNER run --host-env
  --argv0 ARGV0 FILE ARGS`.
  - The runner is `$GEXCC_RUNNER` if set, else the one baked in at install
    (`PREFIX/bin/gexcc-exec`), else `gexc` on `$PATH`.
  - gex never runs the launcher: it binds the module at the entry.
  - Because of the launcher, configure sees a native build ("cross
    compiling… no"), and `AC_RUN_IFELSE`, `make check`, `try_run` and
    `ctest` all run the real target.

The guest sees:
- the host filesystem, the working directory and the environment
  (`--host-env`);
- standard streams redirected from files, sharing the host's offset;
- busybox's applets as `/bin/*` and `/usr/bin/*` (gexcc-exec passes
  `--busybox` with the whole applet list).

### Inside gex: busybox

A gexcc program that runs `system()`, `popen` or `sh`, or execs `sleep`,
`kill`, `cat`, …, would otherwise exec the host's x86 binaries, translated:
seconds per command on a cold cache. gexcc-exec presents a native busybox
in their place.
- `GEXCC_BUSYBOX=` (empty) turns it off; `GEXCC_BUSYBOX=path` picks another.
- `GEXCC_GEXC` picks another gexc.

### Libraries

Everything links statically. A project that builds a shared library still
works: `-shared` writes an **archive** under the `.so` name. Its objects go
in, and a `gexcc.deps.o` member records the `-l`/`-L` it was linked with.
Linking `-lfoo` or `libfoo.so` takes the archive and adds its recorded
dependencies. libtool, CMake `SHARED` and Meson `library()` need no options.
`dlopen` of real side modules is a gap (§3).

### Driver options and environment

| | |
|---|---|
| `GEXCC_POLL=0`, `-fno-gex-poll` | no loop safepoints (§4); signals then reach only code that makes system calls |
| `GEXCC_VERBOSE=1` | print every command |
| `GEXCC_KEEP_WASM=1` | keep `OUT.wasm` beside a program |
| `GEXCC_SYSROOT` | another sysroot |
| `--print-sysroot`, `--print-prefix`, `-dumpmachine`, `-print-prog-name=ld`, `-print-file-name=` | queries build systems use |

C++: exceptions (wasm EH), RTTI, iostreams and threads work
(`tests/gexcc/cxx.cpp`).

## 2. Status

Natively, on gooner: gexc built from this branch, the corpus built and its
own test suites run with `tests/gexcc/corpus/run.sh`. "pass" counts are the
suite's own units:
- automake tests (including gnulib's unit tests);
- the suite's own cases (gawk, make, tar's autotest);
- one unit for a single all-in-one script (lua all.lua, the sqlite and dash
  sessions).

Skips are the suite's own: locales, root, missing tools.

| program | version | build system | build | tests (pass/total) | notes |
|---|---|---|---|---|---|
| zlib | 1.3.1 | own configure | ok | 3/3 | static, "shared" and 64-bit examples |
| xz | 5.6.3 | autotools+libtool | ok | 19/19 | |
| bzip2 | 1.0.8 | Makefile | ok | 6/6 | |
| lua | 5.4.7 | Makefile | ok | 1/1 (all.lua, `_port`) | locale probe patched: musl accepts any locale name |
| sqlite | 3.46.1 | autotools | ok | 1/1 | autoconf tarball has no suite: a session diffed against the host's sqlite3 |
| jq | 1.7.1 | autotools | ok | 29/29 | bundled oniguruma |
| pcre2 | 10.44 | autotools | ok | 1/3 | RunTest test 8: compiled sizes are 64-bit expectations (ILP32); RunGrepTest 150: `setlocale("badlocale")` succeeds on musl |
| expat | 2.6.4 | autotools | ok | 2/2 | |
| libpng | 1.6.44 | autotools | ok | 32/32 | |
| mbedtls | 3.6.2 | CMake (ctest) | ok | 129/130 | psa_crypto passes alone (1958/1958); failed once under `ctest -j3` |
| gawk | 5.3.1 | autotools | ok | 654/654 ("ALL TESTS PASSED") | |
| tar | 1.35 | autotools (autotest) | ok | 204/204, 40 skip | |
| grep | 3.11 | autotools+gnulib | ok | 568/606, 100 skip | remaining: signal timing (exec latency), SIGSEGV catching, multibyte locales |
| sed | 4.9 | autotools+gnulib | ok | 206/214, 46 skip | |
| diffutils | 3.10 | autotools+gnulib | ok | 386/410, 70 skip | before the fixes; rerun pending |
| findutils | 4.10.0 | autotools | ok | 2/2 | the find/xargs suites need DejaGnu (not on gooner) |
| dash | 0.5.12 | autotools | ok | 1/1 | a script session against the host's sh |
| less | 668 | autotools | ok | 1/1 | as a filter (no terminal) |
| ncurses | 6.5 | autotools | ok | – | installed for less, bash, vim, tmux |
| make | 4.4.1 | autotools | ok | rerun pending | first run used the translated host sh inside gex (60 s timeouts) |
| bash | 5.2.37 | autotools | ok | 10/83 before fpcast; rerun pending | needs `-fgex-fpcast-emu` (function pointer casts in unwind-protect); sigsetjmp fixed |
| fmt | 11.0.2 | CMake (C++) | rerun pending | | libc++ 22 strictness: `-include algorithm -Wno-invalid-specialization` |
| curl | 8.10.1 | autotools + mbedtls | ok | rerun pending | |
| perl | 5.40.0 | Configure | rerun pending | | needs `-D_GNU_SOURCE` (musl hides eaccess) |
| coreutils | 9.5 | autotools+gnulib | pending | | |

Recurring gnulib failures not fixed yet:
- `test-select`/`test-pselect`: a write to a loopback TCP socket whose peer
  closed is EPIPE at once (Linux takes the first write), and the SIGPIPE then
  leaves the process hanging.
- `test-sigsegv-*`, `test-c-stack`: a wasm trap is not a SIGSEGV a handler
  can catch.
- Locale tests (musl).

Toolchain checks:
- `tests/gexcc/run.sh` (host against gex): alarmloop, cxx, emptypath,
  fdsemantics, fifononblock, mmapreuse, pathcorners, stdinseek,
  timestamps.
- `tests/wasm32/run.sh` (W's): dlopen, fork, signals, sjlj, threads.
- A CMake project (`find_package(ZLIB)`, `check_c_source_runs`, a `SHARED`
  library, ctest) and a Meson one (`dependency('zlib')`, `cc.run`,
  `library()`, `meson test`) both build and pass.

**Browser** (shims/browser under Node, `tests/gexcc/run-browser.sh`,
`GEX_ARCH=aarch64` for the AArch64 build's shim):
- 2026-10-08, both builds alike: 11 of 15 pass; longargs, pathcorners,
  tcphalfclose and timestamps fail in the browser runtime only (they were
  written after the last browser check; the browser filesystem and network
  lack the fixes made natively). To fix.
- hello, cxx (exceptions, threads), alarmloop (polls deliver SIGALRM, the
  default action kills the child), and three-argument main all match their
  native runs.
- The browser arena's ceiling is 2 GiB (0x80000000): mapping and freeing
  1 GiB in turn works, but a 1 GiB `realloc` beside it fails, where
  natively it succeeds.

Sources and their sha256: `tests/gexcc/corpus/sources.txt`.

### Test runs and the host (2026-10-07, after gooner's crash)

Every gexcc program started on the host is a whole gexc (wasmtime, large
reservations, about 27 threads). Configure try-runs and a host-side
`make check` start thousands, and several suites in parallel took gooner
down (the system log showed about 45,000 processes a minute). Now:

- **Suites run inside one gexc** (`tests/gexcc/corpus/recipes.sh`):
  - configure runs in cross mode (`--build=x86_64-pc-linux-gnu`), so it runs
    nothing it builds;
  - the host builds the suite (`make check TESTS=`) without running it;
  - `guest make -k check SHELL=/bin/sh` runs the suite in the guest, with the
    gexcc-built GNU make (`$GEXCC_WORK/guest-bin`) and busybox's sh and tools.
    Every test process is a guest process.
- **Measured (xz 5.6.3, 19/19, in the guest):**
  - 6 host execs: gexcc-exec's shell, its helpers and the one gexc;
  - 92,037 host clones from that gexc's tree, about 120 a second over 13
    minutes. These are threads, one per guest process: a guest fork plus
    exec of `/bin/true` costs one host thread (50 execs: 77 clones against
    27 for none).
  - zlib's `make test`: 6 host execs.
  - /proc/stat's machine-wide counter also counts those threads and other
    users' processes (about 46 a second on an idle gooner).
- **In-guest results so far** (corpus3, one suite at a time; "host execs"
  counts the run's host programs, "host clones" the gexc tree's threads):

  | suite | tests | host execs | host clones |
  |---|---|---|---|
  | zlib | 3/3 | 6 | — |
  | xz | 19/19 | 6 | 92,037 |
  | bzip2 | 6/6 | 6 | 43 |
  | jq | 28/29 (shtest: busybox `script` has no `-e`) | 6 | 8,203 |
  | pcre2 | 1/3 (as before: ILP32 sizes, musl locales) | 6 | 11,027 |
  | expat | 2/2 | 6 | 22,438 |
  | libpng | 32/32 | 6 | 4,539 |
  | lua, sqlite | 1/1 each | one host gexc each | — |
  | gawk | 654/654 ("ALL TESTS PASSED") | 6 | 8,090 |
  | sed | 206/214 (gnulib counted right; first in-guest run, before the path and select fixes) | 6 | 176,577 |

  After that run (strace counting is now opt-in, `COUNT=1`, because it
  slowed tar's suite):
  - grep and diffutils stopped compiling in cross mode (gnulib's sigsegv
    guessed a sigaltstack): fixed by `share/gexcc/config.site`, the answers
    the same checks gave natively on gex.
  - The gnulib failures in sed's guest run were gex bugs: trailing slashes
    and `.`/`..` in creating calls (test-mkdir, test-rename, test-link,
    test-symlink, test-rmdir, test-fopen), select's EBADF/EINVAL and a pipe
    write end reported readable (test-select, test-pselect). Fixed, each
    with a test (pathcorners, selectcorners); also O_NOFOLLOW and chown on
    missing files (fscorners).
  - findutils: its suites need DejaGnu (`runtest`), absent on gooner.

- **Slots** (`gexcc-exec`): at most `GEXCC_SLOTS` (4) host-started gexcc
  programs run at once per user. The rest wait on an flock. Guest execs take
  none. Tested by `tests/gexcc/slots/run.sh`: at most 2 at once with 2
  slots, all 6 overlap without.
- **Every run** is wrapped in `systemd-run --user --scope -p TasksMax=256
  -p MemoryMax=32G nice -n 19`, one suite at a time, `-j2`.
- **Converted:** automake suites, gawk, tar (autotest), bash's run-* loop,
  CTest suites (`ctest-guest.py` writes the suite as one guest script).
  **Not yet** (refused without `HOSTRUN=1`): make and curl (perl test
  runners: need a gexcc perl in the guest), perl, python3.

## 3. Gaps found, and what fixed them

| gap | where it showed | fix | test |
|---|---|---|---|
| stdin read through std's 8 KiB buffer: input the guest never asked for was consumed | pcre2grep -m tests 132/133 | gex-kernel `StdStream` reads the host fd unbuffered | stdinseek |
| a stream redirected from a host file was a char device: no lseek, no size, no shared offset | pcre2grep -m | the host descriptor's offset, lseek and fstat (`FileHandle::seek`) | stdinseek |
| top-down mmap never reused a hole above its cursor: a 1 GiB buffer freed and asked for again got ENOMEM | expat test_buffer_can_grow_to_max, libpng pngvalid/pngimage | one more pass from the top when the cursor runs out | mmapreuse, gex-kernel unit test |
| `""` resolved to the working directory | Lua `package.searchpath` (`fopen("")` succeeded) | ENOENT for every empty path | emptypath |
| signals reached only code making system calls | findutils configure hung (alarm-guarded check); any busy loop | GexPoll: loop safepoints (§4) | alarmloop (native and browser) |
| no C++ runtime; with one, every `catch` was dropped | — | libc++ over wasm EH; `-mllvm -wasm-enable-eh` (clang enables it only for emscripten) | cxx |
| `main(argc, argv, envp)` did not link | GNU make | rt/main3.c adapter, linked when needed | make |
| prlimit answered 8 MiB/∞ for everything; no RLIMIT_NOFILE | gnulib test-dup2, test-getdtablesize, test-fcntl | per-process limits, NOFILE enforced by the fd table | fdsemantics |
| O_CREAT/O_TRUNC with read-only access failed (std refuses them); a directory opened for writing; *at() on a non-directory fd; fcntl/setsockopt on a bad fd | gnulib test-open, test-fdopendir, test-fopen, test-openat-safer, test-setsockopt | gex-kernel open/at_path/fcntl | fdsemantics |
| host FIFO opened O_NONBLOCK blocked; reads on it seeked (ESPIPE) | grep -D skip | O_NONBLOCK to the host; non-regular host files are streams | fifononblock |
| `file/` resolved; rename to `dir/.` | gnulib test-canonicalize-lgpl, test-rename | ENOTDIR for a trailing slash on a non-directory; EINVAL for . / .. | pathcorners |
| whole-second mtime only; futimens did nothing | gnulib test-stat-time; touch | atime, ctime and nanoseconds from the host; utimensat/futimens set them | timestamps |
| every exec of a wasm32 program recompiled it (2.2–2.7 s for busybox) | background `sleep 1; kill` arrived too late (test-sigprocmask) | compiled modules cached by content (`~/.cache/gex/modules/wasm32`): 0.17 s | — |
| a gexcc program's `sh`/`sleep`/`kill` ran the translated host tools | test-sigprocmask, dash script | busybox through gexcc-exec | — |
| `-Wl,-rpath -Wl,DIR`; Meson's `-Wl,--version` | libtool relinks; Meson | driver | xz, Meson demo |
| sigsetjmp/_setjmp not lowered (only `setjmp` is) | bash error recovery | include/setjmp.h wrapper + rt/sigsetjmp.c | sigjmp |
| calls through mismatched function pointer types trap | bash unwind-protect | `-fgex-fpcast-emu` (binaryen fpcast-emu, entries restored by elemfix.py) | fpcast |
| TCP shutdown did nothing; EPIPE on the open direction | gnulib test-select | Transport::shutdown, send_closed | tcphalfclose |
| "link/" with lstat, dangling/looping links with a slash | gnulib test-lstat | at_path | pathcorners |
| libtool made no shared library (ld -v not GNU-like) | xz | ld wrapper | xz |
| execve arguments capped at 4096 bytes | automake's test-suite.log rule run in the guest | Linux's limits (128 KiB a string, 2 MiB in all, E2BIG) | longargs |
| musl setjmp on try_table broke asyncify | coreutils timeout | GEX_WASM_LEGACY_EH libc | coreutils |

Expected gaps not reached yet (merge-plan §10):
- dlopen of real side modules (W's registry is link-time);
- `sigsetjmp`/`siglongjmp` across handlers;
- SIGSEGV from wasm traps;
- locale and iconv (musl's);
- syscall cost.

Platform differences, not gex bugs, recorded so they are not chased:
- musl accepts any locale name (Lua, pcre2grep test 150);
- ILP32 (wasm32) structure sizes differ from 64-bit expectations (pcre2
  test 8);
- clang 22 rejects implicit function declarations, as gcc 14 does (old code
  such as bash's termcap).

## 4. How it works (for changing it)

**The driver** (`tools/gexcc/gexcc`):
- **Compile steps** go to clang with W's flags (`tools/wine-wasm/flags.sh`;
  setjmp lowered onto the legacy EH instructions, which asyncify can
  instrument), plus:
  - `-mllvm -wasm-enable-eh` (all languages) and `-fwasm-exceptions` (C++);
  - `-fpass-plugin=GexPoll.so`;
  - `-isystem` for `include/c++/v1`, `usr/local/include` and `usr/include`.
- **Links:**
  - wasm-ld with `--experimental-pic -pie --shared-memory --import-memory`,
    then crt1, the objects, main3.o when needed, the libraries,
    libc++/libc++abi/libunwind for C++, libgexcc, libgex, libc and the
    builtins;
  - binaryen: asyncify if the program can fork, and always
    `--translate-to-exnref`;
  - `wasm-elf.py --host-stub hoststub.bin --runner PREFIX/bin/gexcc-exec`
    writes the container.
- **Linker options** with no meaning for a static wasm module (rpath,
  soname, version scripts, `-z`, as-needed, …) are dropped, including
  arguments split across `-Wl`.
- **`-l` lookup:** the driver resolves `-lNAME` itself (a `libNAME.so`
  archive, then `libNAME.a`) so that it can follow recorded dependencies.
  libc's own names (`-lm -lpthread -ldl -lrt …`) are no-ops.

**The container's host launcher:**
- `wasm-elf.py --host-stub` puts a `jmp` at the entry and the stub at
  entry+0x20.
- It leaves out `PT_GNU_STACK`, so an x86-64 host maps the read-only header
  page executable (IA-32 `READ_IMPLIES_EXEC`).
- gex goes by `PF_X` alone, binds `__gex_start` at the entry and never runs
  it.
- Without `--host-stub`, the container is byte-for-byte what W's wcc writes.

**Loop safepoints** (`tools/gexcc/poll/GexPoll.cpp`, `rt/poll.{c,s}`):
- gex delivers a signal to native code at a system call's return. The pass
  puts `if (--__gex_poll_counter < 0) __gex_poll();` at every loop header,
  at the end of the optimisation pipeline.
- The counter is a wasm global, so it is per thread.
- Every 65536 back-edges, `__gex_poll` re-arms it and makes one `getpid`,
  where pending signals are delivered and interval timers checked.
- This works natively and in the browser. Its cost on the browser has not
  been measured yet (edger).

**GexPoll's cost** (edger, browser runtime under node, lua 5.4.7 with and
without polls, ABBA, 6 runs each): median total 1.91 s with polls against
1.84 s without, about 4% slower, inside the run-to-run spread (1.52–2.25 s).

**C++** (`build-cxx.sh`):
- libc++/libc++abi/libunwind 22.1.8, static, with
  `-D_LIBUNWIND_HIDE_SYMBOLS` (wasm is not ELF, so libunwind would otherwise
  use `__declspec`).
- The `__cpp_exception` tag comes from `cpp_exception.s`, added to the
  three archives.

**gex-kernel and gexc changes:**
- **stdio:** `StdStream` duplicates host fds 0–2. A regular file reads,
  writes, seeks and stats through the duplicate. stdin is read unbuffered.
  The browser (`target_family = "wasm"`) is unchanged.
- **mmap:** `MemoryMap::mmap` makes a second pass from `mmap_ceiling`.
- **Paths:** `at_path` rejects empty paths, a trailing slash on a
  non-directory, and a dirfd that is not a directory. rename refuses `.`
  and `..`.
- **Limits:** `Kernel::rlimits` holds the limits; `FdTable::limit` holds
  NOFILE's soft limit.
- **Times:** `Metadata::times` and `Stat`'s atime, ctime and nanoseconds.
  `FileSystem::set_times` and `FileHandle::set_times` serve utimensat and
  futimens.
- **HostFs:** O_CREAT, O_EXCL, O_TRUNC and O_NONBLOCK go to the host as
  flags. Non-regular files are streams.
- **gexc:** `run --host-env`. `gex_cache::compile_cached` caches compiled
  wasm32 programs.

## 5. Resume (stream C)

- Worktree `~/gex-sandbox/wt/gexcc`, branch `merge/gexcc` (off master
  5a066f67). Scratch: `~/gex-sandbox/scratch/CC`.
  - `musl/`: clone of the musl fork, branch `gex-wasm32-cc` (= mw
    gex-wasm32-slim; no changes yet).
  - `tc/`: the installed toolchain.
  - `src/`: tarballs (busybox, llvm-project-22.1.8 too).
  - `llvm/`: the extracted LLVM runtimes tree.
  - `bb/`: busybox experiments.
  - `corpus/`: build trees, `logs/`, `results.txt`.
  - `tmp/`: anything else.
- Reinstall the toolchain (about 15 s without busybox and libc++):
  ```sh
  GEX_BINARYEN=~/gex-sandbox/scratch/W/sr-slim/sysroot/binaryen \
    BUSYBOX_TAR=~/gex-sandbox/scratch/CC/src/busybox-1.37.0.tar.bz2 \
    LLVM_SRC=~/gex-sandbox/scratch/CC/llvm \
    tools/gexcc/install.sh ~/gex-sandbox/scratch/CC/tc ~/gex-sandbox/scratch/CC/musl
  ```
  After editing only the driver: `cp tools/gexcc/gexcc ~/gex-sandbox/scratch/CC/tc/bin/`.
- Build gexc: `cargo build --release -p gexc --features gexc/x86`, giving
  `target/release/gexc`, which gexcc-exec runs.
- Corpus:
  ```sh
  export GEXCC_PREFIX=~/gex-sandbox/scratch/CC/tc GEXCC_SRC=~/gex-sandbox/scratch/CC/src GEXCC_WORK=~/gex-sandbox/scratch/CC/corpus
  tests/gexcc/corpus/fetch.sh            # pins sha256 on first fetch
  tests/gexcc/corpus/run.sh NAME...      # TESTONLY=1: rerun the suite only
  ```
  - Don't edit `run.sh` while a run is going: bash reads scripts as it goes.
    Long runs use a frozen copy (`scratch/CC/run-frozen.sh`).
  - Programs built before a runner change keep the old runner baked in;
    rebuild to pick up gexcc-exec.
- Checks before a gate:
  - `cargo test -p gex-kernel`;
  - `tests/wasm32/run.sh $PWD/target/release/gexc` (the gexc path must be
    absolute; `GEX_WASM_SYSROOT=~/gex-sandbox/scratch/CC/sr/sysroot`);
  - `tests/gexcc/run.sh`;
  - shims build (`GEX_ARCH=x86 shims/kernel/build.sh`, then
    `GEX_SHARED=1 GEX_ARCH=x86 shims/browser/build.sh`) and
    `tests/gexcc/run-browser.sh`;
  - the corpus.
- Ownership:
  - The musl fork, Wine and the `$W` sysroots are stream W's. musl changes
    go on `gex-wasm32-cc` in our clone; ask the coordinator before pushing
    to mw.
  - Don't edit crates/gex-cgen without asking.
- No game runs on gooner; the corpus is fine here. No timing on gooner.
- **Demo (§6), state 2026-10-09:**
  - Design: §6.1a (arm64 native from Debian, `musl-linux-wasm32` ours),
    approved. Item 1 done (gated as integ/2026-10-08f). Item 2 (aarch64
    ELF providers) on this branch, `merge/gexcc-a64prov`, no
    x86 re-key; the coordinator schedules its landing with U. Item 3
    (LP64-layout libraries lowered to wasm32, no memory64) not started:
    measure the lowering's cost in V8 on edger first.
  - AArch64-only gexc for testing: `cargo build --release -p gexc
    --target-dir target/a64`. shims/browser/build.sh rebuilds
    `target/release/gexc` without x86; rebuild it with `--features
    gexc/x86` afterwards (gexcc-exec runs it).
  - Debian builder: `tools/gexcc/debian/` (§6.3). Toolchain
    `~/gex-sandbox/scratch/CC/dtc`; work area `scratch/CC/debian`
    (`src/`, `stage/`, `logs/`, `built.txt`, `pool/`); Debian downloads
    cached in `scratch/CC/deb`. Whole thing: `web/build-native-debian.sh`
    with `GEXCC_PREFIX=~/gex-sandbox/scratch/CC/dtc`, in the build scope.
  - Rootfs (tiers 1, 2 and pm): 22 Debian arm64 packages + 44 of ours,
    175.6 MiB unpacked, 39.3 MB as tar.zst (`native-debian.tar.zst`);
    `native-debian-cache.tar.zst` (49.7 MB) adds Debian's lists and jq's
    .debs for `apt-get install --no-download jq` without the network.
    Rebuild: `mkrootfs.py --pool $GEXCC_DEB/pool --out TREE --tar TAR`
    (`--exclude jq` for the cache image), zstd -19 into `web/public/rootfs/`.
  - dpkg, apt (one `apt-multi` program) and gpgv are ours, labelled arm64
    (§6.1a). Natively: tampered InRelease rejected, `apt-get update` from
    deb.debian.org 10.1 MB in 12 s, install/remove, `dpkg --audit` clean.
  - In the page (edger, bench.sh, `site.mjs native-debian-run`, real Wisp
    to anura.pro): `apt-get update` 9.33 s (10.1 MB at 1633 kB/s),
    `apt-get install -y hello` 5.79 s, `hello` runs. The Wisp connection
    is the page's, served to Workers over shared memory
    (`shims/browser/js/net-bridge.mjs`): a WebSocket in a wasm32 program's
    own Worker never delivers, since the program never returns its event
    loop.
  - `apt-get install --no-download jq` in the page, 26 processes: 92.9 s
    with Debian's translated apt/dpkg/debconf, 8.6 s with ours (8.65 and
    8.56 s). Before, by process: apt-get 92.65 s, dpkg --unpack 39.5 s,
    dpkg-preconfigure 17.4 s (perl; ld.so 7.3 s, translation 2.5 s), each
    dpkg-deb 4.6-5.5 s (ld.so ~0.7 s, translation ~0.4 s, compile
    ~0.37 s), apt-extracttemplates 5.1 s; summed over processes ld.so
    19.0 s, translation 14.1 s (452k blocks), compile 9.4 s, Worker
    start 3.7 s. After: apt-get 8.4 s, dpkg --unpack 2.8 s, each dpkg-deb
    ~0.3 s.
  - Page startup (browser launch to the prompt): 5441 and 5407 ms.
    `native-debian-cmd` (each includes two `date` forks, ~0.5 s), before
    then after: `dpkg -l` 2658/2684 then 1084/1030 ms; `apt-cache policy
    coreutils` 11504/7415 (first), 5.7/4.9 s (later) then 1330/1167 ms;
    `ls /` 954/1028 ms; `perl -e 1` 3297/1855 ms (Debian's perl,
    translated).
  - GUI tier (2026-10-09), `?gui=1&rootfs=/rootfs/native-debian-gui.tar.zst`
    (61.6 MB; 27 Debian packages + 48 of ours, 262 MiB unpacked; rebuild
    with `scratch/CC/mkgui.sh`): a compositor canvas above the terminal,
    `WAYLAND_DISPLAY=wayland-0` and the page's X server (`DISPLAY=:0`).
    - foot (ours, with wayland, xkbcommon, pixman, freetype, fontconfig,
      fcft) runs bash in its window, draws colour and bold, and takes
      typed keys (`drive.mjs --wltype`). It used to stop drawing after
      its first frames: musl's time64 `timerfd_settime` issues the 32-bit
      call (325) when the times fit, which the i386/wasm32 personality
      did not map, so foot's delayed-render timers never fired.
    - Chocolate Doom (ours, on SDL2's X11 driver, SDL2_mixer, SDL2_net,
      libX11/libxcb, all static) on Debian's freedoom plays its demo loop
      in the page through the page's Xwayland. `-timedemo demo1` stalls
      after ST_Init with the process alive and its window gone (open).
      No sound (SDL's dummy driver only).
    - xkbcommon's Compose tables come from Debian's libx11-data.
    - edger scripts: `scratch/CC/pipe/gui.sh` (foot), `doom.sh`,
      `doom2.sh` (timedemo); drive.mjs with `GEX_WEBGPU_SOFTWARE=1`.
  - python3: `_ctypes` built on the wasm32 libffi
    (`tools/gexcc/debian/libffi-wasm32`: generated trampolines, no
    closures); `import ctypes`, types, structures and buffers work;
    `CDLL(None)` raises OSError (no dlopen), patched so the import
    survives it.
  - Known: fork inside a C++ catch block traps (asyncify).
  - perl 5.40 ours builds (`build_perl`): Debian's options, Configured
    natively, every extension static; make_ext's make and compiler run on
    the host through gexc's `GEXC_HOST_EXEC` (an opt-in build aid for
    build tooling only, never set by a demo or image). POSIX, Digest::MD5,
    Storable, Encode and threads load. Not shipped: Debian's arm64
    perl-base stays until ours provides perlapi-5.40.1 and Debian's
    layout for arm64 Perl packages.
  - Next: ship perl, bc's dc, /usr/bin/git duplicate, GTK, item 3.
- **Next (corpus):**
  1. Rerun the corpus clean.
  2. coreutils, bash, make.
  3. perl, python3 (recipes written), curl+mbedtls, libevent+tmux, vim,
     fmt (C++), openssh, git.
  4. The loopback TCP EPIPE/hang.
  5. Measure GexPoll's cost on edger.

## 6. Demo: a native Debian rootfs (plan, 2026-10-08)

Goal (user, 2026-10-08): a slim but usable Debian root filesystem whose
userland is built from Debian's sources with gexcc into native wasm32
programs (not translated x86 binaries), plus developer tools and GUI apps,
booting in the browser page. Native gexc is for debugging; numbers come
from edger (bench.sh).

### 6.1 What "Debian" means here

- **Release:** trixie (13.7 on 2026-09-12). `InRelease` verified with the
  host's `debian-archive-keyring`, `Sources.xz` checked against it, every
  `.dsc` against `Sources`, every tarball against its `.dsc`
  (`tools/gexcc/debian/debsrc.py`; binaries: `debfetch.py`, same chain
  through `Packages.xz`).
- **Sources:** Debian source packages as Debian ships them:
  `dpkg-source -x` unpacks the orig tarball and applies `debian/patches`.
  Our changes on top are small patch files per package
  (`tools/gexcc/debian/patches/PKG/*.patch`), applied after Debian's.
- **Not `dpkg-buildpackage`:** `debian/rules` cross-builds through
  `dpkg-architecture`, debhelper and build-profiles, none of which know a
  wasm32 musl architecture. Each package gets a recipe
  (`tools/gexcc/debian/recipes.sh`) that runs its own build system with
  gexcc and the configure options `debian/rules` uses, minus what we lack
  (selinux, audit, systemd; nls where it costs size).
- **Binary packages:** each recipe installs into a staging tree; the staged
  files are split into Debian's binary package names and written as real
  `.deb`s with `dpkg-deb --build`. Control fields come from `debian/control`
  and `debian/changelog`.

### 6.1a Two architectures: arm64 from Debian, wasm32 from us (user, 2026-10-08)

Requirements: apt is in the rootfs and works (`apt update`, `apt install X`
against real Debian mirrors, through gex's network path); what apt
downloads are Debian's **arm64** builds, run translated; our gexcc packages
live in a separate repository that apt never overwrites.

**Why arm64, measured** (edger, browser runtime under Node, Debian trixie's
own arm64 and amd64 builds of the same versions, whole-process wall time,
median of 3, ABBA; `scratch/CC/archbench`, 2026-10-08):

| workload | fjit arm64 | fjit amd64 | tier-1 arm64 | tier-1 amd64 |
|---|---|---|---|---|
| `xz --version` (start-up) | 3.48 s | 9.28 s | 1.62 s | 1.77 s |
| `xz -6` of 5.3 MB | 9.70 s | 16.10 s | 9.61 s | 12.15 s |
| `gzip -6` of 5.3 MB | 3.00 s | 10.76 s | 2.42 s | 3.39 s |
| python3.13, dict/sort/fib | 17.16 s | crash | 10.39 s | 13.84 s |
| sqlite3, 100k rows | 12.52 s | 24.58 s | 10.84 s | 12.61 s |
| lua5.4, fib/sort | 5.11 s | 10.69 s | 5.83 s | 7.82 s |

- arm64 is faster than amd64 in every case: on tier-1 by 9-29% (e.g.
  python 10.39 s against 13.84 s), with fjit by 1.6-3.6 times (gzip 3.00 s
  against 10.76 s). The user's claim holds.
- amd64 python3.13 under fjit crashes: one translated function (CPython's
  eval loop) is 8,298,855 bytes, over V8's 7,654,321-byte function limit.
  An fjit bug (sent to the coordinator).
- For short command-line runs fjit is slower than tier-1 on both
  architectures (arm64 `xz --version` 3.48 s against 1.62 s, python
  17.16 s against 10.39 s): its compile cost is not repaid in a few
  seconds. The shell demo runs translated programs on tier-1 unless a
  program is known to be long-running.

**dpkg's architectures.**
- Native architecture **arm64**: apt fetches Debian's arm64 index and
  packages as native, its dependency resolution unmodified. `dpkg
  --print-architecture` says arm64. dpkg, apt and gpgv are ours (below);
  perl-base and debconf are Debian's arm64 builds, translated.
- Foreign architecture **`musl-linux-wasm32`** (`dpkg --add-architecture`;
  the name dpkg's tables give a musl wasm32 tuple) for every package we
  build. Tested with the host's dpkg: accepted, and an arm64 package's
  `Depends: coreutils (>= 9.7)` is satisfied by
  `coreutils:musl-linux-wasm32`.

**Our dpkg and apt (user, 2026-10-08).** Each translated process in the
page pays about 1.5-2.5 s before it does its own work (Worker, translated
ld.so, retranslation; profile in §6.4), and `apt install` runs dozens of
dpkg, dpkg-deb, dpkg-split and gpgv processes. So dpkg, apt and the
OpenPGP verifier are gexcc builds:
- **dpkg** 1.22.22, C, static (libmd, zlib, xz, zstd, bzip2 from tier 1).
  configure asks dpkg-architecture for the host triplet's name; the recipe
  answers `arm64`, so `ARCHITECTURE` is compiled in as arm64 and
  `dpkg --print-architecture` says arm64 (checked natively).
- **apt** 3.0.3, C++ against gexcc's libc++, CMake. libapt-pkg and
  libapt-private are static (linked into each program); no seccomp, udev,
  systemd, NLS, docs, tests, apt-ftparchive (Berkeley DB) or dselect.
  `COMMON_ARCH=arm64`. Two libc++ patches (`patches/apt/`: no
  `char_traits<unsigned char>`, no `deque` of an incomplete type).
  Deps: xxhash (new, static only), zlib, bzip2, xz, lz4, zstd, OpenSSL.
- **Packaging.** Both are labelled `Architecture: arm64` (binpkgs.txt
  `arch dpkg arm64`), version Debian's + `+gex1`, from our repository,
  which now carries `binary-arm64` beside `binary-musl-linux-wasm32`. The
  `o=gex` pin (1001) keeps them over Debian's, also after a Debian point
  release (1.22.23 > 1.22.22+gex1 still loses to 1001); the `-1` pin list
  covers only our wasm32 names, so it does not block them.
- **Essential/Pre-Depends.** Debian's dpkg is `Essential: yes` with
  `Pre-Depends: ${shlibs}` (libc6, libbz2, liblzma, libmd0, libselinux1,
  libzstd1) and `Depends: tar (>= 1.28-1)`. Ours keeps `Essential: yes`
  and has no library Pre-Depends (static); tar's is a Pre-Depends, met by
  our tar (Multi-Arch: foreign). Upgrading dpkg is still dpkg's own
  unpack-then-configure; nothing in the closure Pre-Depends on a library
  that only our dpkg brought. Those libraries stay installed (other arm64
  packages need them) but are no longer pulled in for dpkg.
- **apt's dependencies.** Debian's apt Depends on libapt-pkg7.0 (exact
  binary version), sqv, base-passwd | adduser and the keyring. Ours:
  `base-passwd (>= 3.6.1) | adduser, gpgv, debian-archive-keyring`;
  libapt-pkg7.0 and sqv leave the image. Consequence: Debian's
  apt-utils (Depends `apt (= 3.0.3)`) is not installable against
  3.0.3+gex1; build it here if wanted.
- **OpenPGP.** apt's `sqv` method is built only when the build machine has
  sqv; without it apt verifies with its `gpgv` method, which runs
  `APT::Key::gpgvcommand` (default `gpgv`). Ours is gnupg2 2.4.7's gpgv,
  configured as Debian's gpgv udeb (no npth, gpgsm, dirmngr, ldap, tofu,
  sqlite...), with libgcrypt 1.11 (no asm), libgpg-error, libassuan and
  libksba static. Debian's keyring files are unchanged
  (`/usr/share/keyrings/debian-archive-keyring.gpg`, `Signed-By`).
  What translated sqv cost before: in the page it ran three times per
  `apt-get update` at 5.9-7.8 s each (Rust, about 20 s per update).
  Tampering with a cached InRelease is rejected (BADSIG, exit 100).
- **Hashing.** apt hashes downloads itself (OpenSSL EVP, in-process) and
  dpkg checks md5sums in-process; neither execs a hashing program during
  install. TLS is apt's http method's OpenSSL, native.
- **debconf.** apt's `DPkg::Pre-Install-Pkgs` hook runs debconf's
  dpkg-preconfigure (perl) before every install: about 19 s in the page
  for jq. The image clears it (`#clear DPkg::Pre-Install-Pkgs;` in
  99gex); postinst scripts still ask (noninteractively) at configure.
- **Later: a Rust target for gexcc** (sqv, ripgrep, fd...): a rustc
  target over gexcc's sysroot and wasm32 container. Not needed for apt
  once gpgv is native.

**How arm64 packages' dependencies are satisfied.**
- **Tools** (bash, coreutils, dash, grep, sed, tar, gzip, findutils,
  diffutils, debianutils, mawk, hostname, ncurses-bin, util-linux,
  bsdutils, bsdextrautils, sysvinit-utils, base-files, base-passwd, and
  later perl, python3, git...) are ours, `Multi-Arch: foreign`, so they
  satisfy any architecture's dependency by name. Version = Debian's +
  `+gex1` (above that Debian revision and its `+debNuM` point updates).
- **Libraries** (`Multi-Arch: same`: libc6, libzstd1, libtinfo6...): an
  arm64 program's ld.so loads arm64 ELF objects, so these are Debian's
  arm64 packages, installed by apt as usual. The dpkg database stays true.
  Native code reaches them through gex's loader, not through dpkg (below).
- **Essential:** `apt dist-upgrade` installs missing Essential packages of
  the native architecture. Our repository carries every Essential tool by
  name and the pins below stop the arm64 copies; simulated against the
  real trixie arm64 index, `dist-upgrade` then pulls only libraries and the
  arm64 system pieces the rootfs ships anyway (dpkg, apt, perl-base,
  debconf, sqv, libc-bin, ca-certificates), and `apt install python3 git
  vim` (128 packages) touches none of ours.

**Pinning** (`/etc/apt/preferences.d/gex-native`):
```
Package: bash:arm64 coreutils:arm64 ... (every package name we ship)
Pin: release *
Pin-Priority: -1

Package: *
Pin: release o=gex
Pin-Priority: 1001
```
Libraries are deliberately not pinned (pinning libc6:arm64 makes every
arm64 program uninstallable).

**Repositories** (`/etc/apt/sources.list.d/`):
- `debian.sources`: `Types: deb`, `URIs: http://deb.debian.org/debian`
  (https once the guest's TLS is checked), `Suites: trixie
  trixie-updates`, plus `trixie-security`, `Architectures: arm64`,
  `Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg`.
- `gex.sources`: our repository, `Architectures: musl-linux-wasm32`,
  Origin `gex`, Suite `trixie`, signed by a gex archive key
  (`/usr/share/keyrings/gex-archive-keyring.gpg`; `trusted=yes` only for
  local test builds). Layout: `dists/trixie/{InRelease,Release}`,
  `dists/trixie/main/binary-musl-linux-wasm32/Packages.xz`,
  `pool/main/<initial>/<srcpkg>/*.deb`. In the rootfs it is a `file:`
  repository at `/var/lib/gex-apt`; the page serves the same tree beside
  the rootfs, so packages not preinstalled (tier 2 extras) can be fetched
  on `apt install` once the page exposes its own origin to the guest
  (a host name the page's network layer answers with `fetch()` instead of
  Wisp).

**The loader path for translated programs.**
- An arm64 program execs as today: gex maps it and Debian's
  `ld-linux-aarch64.so.1` (translated), which maps Debian's arm64
  `libc.so.6`, `libz.so.1`, ...
- When an object maps, gex's ELF-provider hook (docs/elf-providers.md)
  binds native code over its exports, by soname and versioned symbol:
  first glibc's mem*/str*, printf family and libm, as the x86-64 providers
  do now; later whole libraries (zlib, zstd, lzma, sqlite, pcre2, expat:
  pure-C ABIs) built by gexcc for an LP64 layout; finally the whole libc
  (merge-plan §10.1). Unbound exports keep running translated, so this
  degrades to correct.
- The providers ship as a package of ours (`gex-providers`, files under
  `/usr/lib/gex/providers/`), so they are versioned with the rootfs.
- Our native programs link musl statically and need no loader. A
  translated program that execs `/bin/sh` gets our dash; ours that exec an
  arm64 program get it translated.

**gex work this needs (expected):**
1. **wasm32 programs in the aarch64 build: done** (0366efa6). gex-ld
   accepts a wasm32 container (ELF32 i386 with the `GEXWASM` header) on
   the AArch64 guest; the kernel keys the i386 ABI on the process
   (`compat`) instead of the build; `ctx::sp_slot(compat)` (esp is `x(4)`
   in every build) and `ctx::COMPAT_SCRATCH` (the vector registers of an
   AArch64 context, which wasm32 programs never use) cover the two layout
   differences. Checked on an AArch64-only gexc (tests/wasm32 5/5,
   tests/gexcc 15/15, a wasm32 bash running Debian's arm64 xz, python3.13
   and sqlite3, python's `os.system` running the wasm32 dash) and on the
   aarch64 browser shim (tests/wasm32 5/5; tests/gexcc as on x86).
2. **AAPCS64 ELF providers: ready on `merge/gexcc-a64prov`.**
   - `provider.rs` gains an explicit architecture (`PROVIDES ... i386 |
     x86_64 | aarch64`, `provider::Arch`, `guest_arch(class32)`):
     i386/x86-64 on an x86 build, aarch64 for 64-bit images on an aarch64
     build, none on PPC. Before, `for_dll` returned nothing off x86.
   - `providers/aapcs64.inc` maps the `gp64_` helpers to AAPCS64 (x0-x7,
     then the stack; result in x0/v0; return to x30). `glibc_mem_a64` and
     `glibc_math_a64` are the x86-64 bodies with it in front (Rust
     `concat!`); `glibc_mem_a64.inc` replaces strcmp with a step-for-step
     port of aarch64 glibc's (its result is not the byte difference, and
     depends on mutual alignment). A later `GEX_PROVIDE` of an export now
     replaces an earlier one.
   - **No x86 unit re-keys:** `provider.h`, the x86 sources and the header
     are unchanged, so every x86 provider fingerprint (and every unit keyed
     on one) is the same. Only arm64 libc/libm translations change.
   - Checked on an AArch64-only gexc with tests/elf-prov cross-built for
     Debian's arm64 glibc (scratch/CC/a64prov/run-a64.sh): mem, printf,
     malloc and fork agree with providers off and on (mem binds 14
     addresses in libc, math 8 in libm). math differs only on round()'s
     halfway values, where the **translated libm is wrong**: gex-lift
     lowers FRINTA (ties away from zero) to wasm `nearest` (ties to even),
     so translated `round(0.5)` is 0 and `round(-2.5)` is -2; the provider
     gives 1 and -3 as C requires.
   - Not ported yet: printf (its va_list code is SysV) and malloc.
3. **arm64-ABI native libraries in the guest's own wasm32 memory** (user,
   2026-10-08: no memory64 at run time). Whole-library providers for
   arm64 programs need aarch64's LP64 data layout. Approach to evaluate:
   - Compile with an LP64 layout matching AAPCS64: clang's wasm64 target
     for the layout, plus `-funsigned-char`. A layout test compares
     `sizeof`/`offsetof`/alignment of every exported struct against
     `aarch64-linux-gnu`: `long double` (16-byte quad), `wchar_t`,
     `__int128` alignment, bitfields.
   - Lower to wasm32 before shipping (binaryen `--memory64-lowering`, or
     the same lowering in gex-cgen): addresses are i64 in the IR and become
     i32 at each access. Guest addresses stay under 4 GB, so this is safe
     today.
   - **The address step is one hook** (one helper or one pass), not an
     assumption the library code relies on. If memory ever runs short, the
     plan is the dual heap (a guard on loads and stores of high pointers
     that routes them to a second 4 GB heap), never memory64, and that
     guard replaces the hook without changing the approach.
   - The real work is glibc's aarch64 ABI over musl: opaque type sizes
     (`pthread_mutex_t`, `sigset_t`), `struct stat`/`dirent`, `jmp_buf`,
     `FILE` and the `stdin`/`stdout` symbols, `__errno_location`, symbol
     versions (`memcpy@GLIBC_2.17`), `va_list` adapters (aarch64's is a
     struct), and callbacks into translated code through the provider
     guest-call path.
   - First, measure what the lowering's i64 address arithmetic costs in V8
     on a small benchmark (edger, browser runtime).
4. **fjit:** split or refuse functions over V8's size limit; a hotness
   threshold so short processes stay on tier-1.

**Network.** Wisp, already in gex: the page's `WispNetwork`
(web/src/net.js), the kernel's socket shapes and its own DNS answers
(crates/gex-kernel/src/net.rs: a private address per name, mapped back to
the name when the guest connects, so apt's getaddrinfo works over a
TCP-only proxy). The demo's default endpoint is `wss://anura.pro/`
(user's), overridable with `?wisp=`. https goes through the guest's own TLS
(apt's OpenSSL). Test loops never use the public endpoint: natively gexc
has real sockets, and repeated runs go to a local HTTP mirror of the
cached trixie subset (`scratch/CC/deb`) instead of deb.debian.org.

### 6.2 Packages, in tiers

Tier 1, the shell (first milestone):
bash, dash, coreutils (single binary, symlinks), mawk (Debian's Essential awk), sysvinit-utils,
util-linux (the basics:
`more`, `column`, `kill`, `getopt`, `script`, `cal`, `hexdump`, `lsblk`
not), grep, sed, gawk, findutils, diffutils, tar, gzip, xz-utils, bzip2,
less, ncurses (libs + `tput`, `clear`, `reset`), procps (`ps`, `top`,
`free`, `uptime`, `pgrep`, `kill`), debianutils (`which`, `run-parts`,
`tempfile`), hostname, nano, vim (vim-tiny-sized `vim` with the Debian
runtime trimmed), file (+ libmagic), base-files, base-passwd,
sensible-utils, ncurses-base, tzdata (trimmed). Libraries: zlib, libbz2,
liblzma, libzstd, libmd, readline, pcre2, libacl/libattr where needed.
Help: `--help` everywhere; `man` (man-db + groff + libpipeline + gdbm) is
tier 2 since groff is C++ and large.

Tier 2, developer tools:
git (local clone/commit/log/diff; `git-remote-http` needs curl), python3.13
(stdlib trimmed: no test/, idlelib, tkinter, ensurepip), perl 5.40
(perl-base + perl-modules), make, sqlite3, lua5.4, jq, tree, patch, bc, ed,
strace-free: `gdb` not. tmux (libevent, ptys), htop (needs /proc), man-db,
dpkg itself (in the guest: `dpkg -l`, `dpkg -S`), apt if its dependency set
(OpenSSL, xxhash, lz4, zstd, gcrypt-free in 3.0) builds; curl + OpenSSL or
mbedtls (network over Wisp in the page). A C compiler in the guest: tcc
targeting i386 (its output then runs translated) is the cheap option;
clang/wasm-ld as wasm32 programs (gexcc in the guest) the impressive one,
about 80-100 MB of wasm, decided after measuring.

Tier 3, GUI (gex's Wayland compositor; Xwayland for X11, never Xvfb):
- Wayland: foot (wayland, libxkbcommon, pixman, fcft, freetype, fontconfig,
  tllist, utf8proc), wayland-utils; fonts-dejavu-core.
- SDL2 (Wayland backend; software rendering, GLES through gex-gl's EGL):
  chocolate-doom with freedoom (SDL2_mixer, SDL2_net), maybe another small
  game.
- X11 under Xwayland (`hostx=1` in the page): libX11, libxcb, Xt, Xaw, Xmu,
  Xext, Xrender; xterm, x11-apps (xeyes, xclock), xcalc.
- GTK 3 (glib, cairo, pango, harfbuzz, fribidi, gdk-pixbuf, epoxy, atk)
  only if tiers 1-3 above are done: it is the biggest dependency graph.

Not attempted: ripgrep (Rust has no wasm32 Linux musl target; would need
`-Zbuild-std` and a custom target), systemd, anything needing kernel
modules or real devices.

### 6.3 Build layout

- `tools/gexcc/debian/` (state 2026-10-08):
  - `debsrc.py`: verified source fetch, `dpkg-source -x`, our patches
    (`patches/SRCPKG/`). `debfetch.py`: verified binaries and their
    closure.
  - `build.sh TIER|SRCPKG...` with `recipes.sh` (`build_SRCPKG`, installing
    into `$GEXCC_DEB/stage/SRCPKG`; libraries also into the toolchain's
    sysroot); `packages.txt` pins the versions and tiers.
  - `mkdebs.py`: staging tree to binary packages (`binpkgs.txt`: the split
    and `depends` lines), `Architecture: musl-linux-wasm32`, version
    `+gex1`, Provides/Conflicts/Breaks/Replaces from `debian/control`.
  - `mkrootfs.py`: Debian's arm64 closure (apt, dpkg, perl-base, debconf,
    ca-certificates, base-files, ...) plus our packages standing in for
    Debian's tools by name; every .deb unpacked under a merged /usr, no
    maintainer scripts; dpkg's status/info (multiarch layout) written from
    the packages; what the scripts would do (accounts, /etc/shells,
    alternatives, CA bundle); apt sources, our `file:` repository at
    /var/lib/gex-apt, the pins; Debian's minimized-image path excludes
    (written to /etc/dpkg/dpkg.cfg.d/excludes too).
- `web/build-native-debian.sh [TIER...]`: all of the above, then
  `web/public/rootfs/native-debian.tar.zst`. One command, cached.
- Builds run in `systemd-run --user --scope -p TasksMax=4096 -p
  MemoryMax=32G nice -n 19`, configure in cross mode (§2: runs nothing it
  builds).
- Reproducible: Debian versions pinned in `packages.txt`, our patches in
  the tree.

### 6.4 The page

Demo `native-debian` ("Debian, built from source to wasm") on the aarch64
page (web/src/demos.js): fetches `/rootfs/native-debian.tar.zst`
(`?rootfs=` overrides; decoded with the vendored zstddec), bundles it with
its symbolic links as links (`gex_add_symlink`), and boots `/bin/bash -l`
(`-l -i`) as pid 1 with uid 0, `HOME=/root`, `TERM`, `PATH`. Network: Wisp,
default `wss://anura.pro/`, `?wisp=URL` overrides (`?wisp=` empty: none).
The page holds the one Wisp connection and serves it to every
guest Worker over shared memory (`shims/browser/js/net-bridge.mjs`).
Translated programs run on tier 1 (`?fjit=1` turns the function JIT on).
`?cmd=` runs one command for tests. Under Node the same runtime runs the
tar with `run-node.mjs --tar FILE` (`GEX_UID=0`, `GEX_NETWORK=1` for real
sockets instead of Wisp); there apt updates from Debian and installs
arm64 packages (§5).

Size budget (zstd, transferred): tier 1 at most 15 MB, tier 1+2 at most
45 MB, everything at most 80 MB. Tiers 1 and 2 with our dpkg/apt/gpgv
today: 39.3 MB.
If the whole-tar load is too slow, split into a base tar and per-package
tars fetched when first opened (the kernel's demand-fill hook).

Startup (edger, bench.sh): page open to bash prompt; the first exec of
each program pays a WebAssembly compile; measured, then cut with a
compiled-module cache shared across Workers (exists) and, if needed,
ahead-of-page compilation of the hot set while the rootfs downloads.

### 6.5 Expected gex gaps

- See §6.1a for the arm64 items (wasm32 in the aarch64 build, AAPCS64
  providers, LP64 libraries, fjit).
- Browser: exec/fork churn of a login shell (bash's startup forks for
  `$(...)` in Debian's `/etc/bash.bashrc` and profile.d); module compile
  per distinct program; the 2 GiB arena per process (python, git fine).
- ptys and job control for tmux, htop, vim, nano (`/dev/ptmx`, `TIOCSCTTY`,
  `SIGWINCH` from the page's terminal size).
- `/proc` coverage for procps/htop (`/proc/PID/stat`, `statm`, `status`,
  `/proc/meminfo`, `/proc/stat`, `/proc/uptime`, `/proc/loadavg`).
- `getpwnam` etc. through musl reading `/etc/passwd` (fine);
  locales: musl's C.UTF-8 only.
- dlopen: python extension modules and perl XS are linked in statically
  (Setup.local / static_ext); gexcc's shared-library archives make the
  rest link.
- GUI: wl_shm + xdg-shell clients against gex-wayland; fcft/fontconfig
  file access; SDL2's Wayland backend; X11 clients' unix socket to the
  page's Xwayland.
- Program sizes: every program links libc statically; coreutils as one
  binary keeps that once.

### 6.6 The demo today: what runs how, and measured (2026-10-10)

What runs in the page, and as what:

- **Native wasm32, built by us** (gexcc from Debian's sources,
  architecture `musl-linux-wasm32`, tools/gexcc/debian/packages.txt): bash,
  dash, coreutils, findutils, grep, sed, gawk/mawk, diffutils, patch, tar,
  gzip/bzip2/xz/lz4, less, nano, vim, tmux, htop, jq, tree, curl, git,
  sqlite3, python3.13, perl, lua5.4, make, gnupg2, openssl, and dpkg/apt
  themselves. These are `:arm64` to apt but pinned (`Pin-Priority -1`, also for the
  names they `Provides`), so apt never replaces them with Debian's.
- **Translated arm64, from Debian over Wisp**: everything `apt install`
  fetches, plus the base it needs (libc6, perl-base, libgcc-s1,
  libstdc++6...). Tried in the page: subversion, tcc, ripgrep, fd-find,
  gcc (with libc6-dev), neovim, nodejs (V8's JIT runs translated too).
- **Replaced libraries (item 3, §7.8, §7.13)**: an arm64 program's
  `libz.so.1`, `libzstd.so.1` and `libsqlite3.so.0` are wasm builds,
  lowered from LP64, bound by soname from `/usr/lib/gex/aarch64/`
  (package `gex-providers-arm64`, in the image). svn uses all three, and
  node uses libz and libzstd. On top of that, per-function providers bind
  14 libc functions (glibc_mem_a64) and 8 libm ones (glibc_math_a64).
  libc.so.6 itself is never replaced.

Measured in the page on edger (`scratch/CC/pipe/devtools4.sh` and
`devtools5.sh`; `web/test/site.mjs native-debian-run`, Chrome, tier 1,
Wisp through `wss://wisp.mercurywork.shop/`). Each figure is one run, and
each timing includes the first-exec compile of every program involved:

| step | run 4 | run 5 |
|---|---|---|
| `apt-get update` | 22.0 s | 21.7 s |
| `apt install subversion tcc ripgrep fd-find libc6-dev` | 76.5 s | |
| svn create / checkout / add / commit 40 files | 2.2 / 3.4 / 2.4 / 4.7 s | |
| svn status / diff / commit / update / log | 2.1 / 2.3 / 4.0 / 2.9 / 2.5 s | |
| `tcc -O2`, run it, `tcc -run` | 2.1, 1.2, 1.8 s | |
| `rg -c define /usr/include/stdio.h` | 3.3 s | |
| `apt install gcc` / `gcc -O2 h.c` / run it | 134.2 / 39.7 / 0.9 s | |
| `apt install neovim` | 32.6 s | 31.6 s |
| `nvim --version` / `nvim --headless +q` | 4.4 / 14.2 s | 4.3 / 6.5 s |
| nvim scripted edit (`-c 'normal ...' -c wq`), file checked | 39.0 s | 7.6 s |
| `apt install nodejs` | 67.6 s | 76.1 s |
| `node --version` | 3.3 s | 3.4 s |
| `node -e 'console.log(1+1)'` | trapped (fixed since) | 40.1 s |
| `node -e 0` (twice) | | 31.9, 29.1 s |
| fs + zlib + crypto + timer + http on localhost | | 39.4 s |
| node http server, fetched by curl (incl. node start) | | 34.1 s |
| child_process: spawnSync, stdin input, spawn | | 169.7 s |
| nvim `system()` running node | | 44.4 s |

Every command in run 5 returned 0 with the expected output. node starts
in about 30 s because V8 lifts and runs its builtins and JIT code
translated. That is the next cost to cut, not a correctness gap.

What had to change for these runs:

- **Kernel.** `F_GETFL` reports the real access mode, and a stream
  `socketpair` is made of global pipes so it survives fork (libuv spawns
  every child through one). Socket kinds are reported, which libuv's
  `uv_guess_handle` needs.
- **Shim.** The provider re-bridge guard no longer gives up in a
  long-lived process.
- **Page.** It redials a Wisp proxy that drops the WebSocket. Since
  2026-10-10, wss://anura.pro/ closes every WebSocket after 2-6 s.

## 7. Shared libraries: wasm32 programs, and wasm32 libraries under arm64 programs (design, 2026-10-09)

Two requests from the user, designed together because both load native
wasm32 code as a library into a process at run time:

1. **gexcc programs with real shared libraries**: side modules sharing the
   program's memory and function table, loaded at exec from `DT_NEEDED`
   and later with `dlopen`, instead of every program linking its own libc
   and libraries. For smaller images, one compile of each library per page
   instead of one per program, and less memory per process. Static linking
   stays available (and the default until the shared path is proven).
2. **Item 3: arm64 programs loading gexcc-built wasm32 libraries**: whole
   libraries (zlib, zstd, then libc) built with aarch64's LP64 layout,
   lowered to wasm32 addressing, bound in place of Debian's arm64 objects.

### 7.1 What exists

- **Programs are already position-independent.** gexcc links every
  program `wasm-ld --experimental-pic -pie --shared-memory
  --import-memory`: a PIE main module in emscripten's terms, importing
  `__memory_base`, `__table_base`, `__stack_pointer`, the memory and the
  function table, with a `dylink.0` section. Its objects are `-fPIC`
  (`flags.sh`). So the shared-library cost below is not "PIC against
  static": that cost is already paid.
- **But with hidden visibility.** clang's wasm targets default to
  `-fvisibility=hidden`, so gexcc's objects (and the sysroot's libc.a)
  export nothing: a side module linked from them is empty. A shared build
  needs `-fvisibility=default` (what emscripten passes for side modules).
- **Loading.** A wasm32 program is installed as a stateful provider
  (`cgen/wasmprog.rs`, native `wasmprog.rs`): its module occupies a range
  of the process's function table (`gex-kernel` `ftab`: a base per module,
  indices never reused, every thread's table replays one log) and its data
  sits at the container's `__memory_base` (a fixed address chosen at link
  time). Each Worker/store instantiates it; JS supplies `env` and `gex`.
- **Fork.** asyncify (§4): every function that can reach `gex.fork_point`
  is instrumented (an onlylist from `forkreach.py`, a whole-program call
  graph). The child re-instantiates the module over the copied memory at
  the parent's table base (bdd34eb3).
- **Side modules exist already for Wine and for ejected projects**:
  `wasmdll.rs` (Wine's PE side as one wasm module behind PE skeletons),
  `libprov.rs` (stateful providers: `-shared` PIC modules with their own
  data, stack and heap, for a translated program's DLL or ELF library,
  including wasm64 ones for x86-64 libraries). They parse `dylink.0`
  (`libprov::dylink_memory`) and bind exports into the function table.
  What none of them do is resolve one wasm module's imports against
  another's exports (GOT, cross-module calls) or `DT_NEEDED`.
- **Compiled modules** are shared between a process's threads and passed
  to its forks (`host.mjs` `#moduleCache`, keyed by provider file name).
  A new process (every `execve`) compiles its program again; nothing
  survives the page.
- `dlopen` is libgex's registry (`__gex_dl_libs`, Wine's closed world):
  `dlopen(NULL)` + `dlsym` on the program's own functions do not work,
  which is what stops GTK's `GtkBuilder` signal connection today.

### 7.2 Choice of ABI

**Emscripten-style side modules (`dylink.0`), not an ABI of our own.**
LLVM writes them (`wasm-ld --experimental-pic -shared`), binaryen
understands them, and the shapes gex already parses are these:

- a library is `libfoo.so.N`, a wasm module (in the same ELF32 container
  as programs, `GEXWASM` header version 2 marking a library, so `file`,
  dpkg and the kernel treat it as an object of ours) with `dylink.0`:
  memory size/alignment, table size/alignment, `NEEDED` sonames,
  `RUNTIME_PATH`, export/import symbol flags (TLS, weak);
- data references between modules go through `GOT.mem.<sym>` imported
  mutable i32 globals, function pointers through `GOT.func.<sym>` (a table
  index); calls between modules are plain function imports (`env.<sym>`),
  which V8 and wasmtime call directly, wasm to wasm, without JS;
- `__memory_base`/`__table_base` per module, one `__stack_pointer` per
  thread shared by all modules, `__indirect_function_table` and the memory
  shared.

What we add (as emscripten adds its own on top):
- **The loader is gex's, not JS's.** Symbol resolution, placement and
  relocation are a crate shared by both shims (`gex-dylink`): the browser
  shim runs it in its own wasm, JS only does `new WebAssembly.Instance`
  from a plan (module, imports by name → (instance, export)). Same order
  natively in wasmtime. One implementation, tested natively.
- **ELF semantics where emscripten has none:** load order and lookup scope
  as `ld.so` (the program, then `NEEDED` breadth-first, then `dlopen`ed
  with `RTLD_GLOBAL`); search path `RUNPATH`, `LD_LIBRARY_PATH`,
  `/usr/lib/wasm32-linux-musl`, `/lib/wasm32-linux-musl` (the multiarch
  directory our packages already own; Debian's arm64 objects stay in
  `aarch64-linux-gnu`); weak undefined resolves to 0 (fixes the GTK
  `GOT.func::__lsan_ignore_object` failure class without patches);
  `dlopen(NULL)`/`dlsym(RTLD_DEFAULT)` see every export of the global
  scope, the program's included when it is linked `-rdynamic`.
- **No symbol interposition into a library's own calls.** A call inside
  a side module to its own default-visibility function is a direct wasm
  call; a program defining `malloc` replaces it for the program and for
  modules loaded after it, not inside libc. Documented limit (no package
  in the image interposes libc today).
- `dlclose` keeps the module (as musl does).

### 7.3 Measured: codegen cost in V8 (edger, node 26, one core)

Harness `tools/gexcc/bench-codegen` (build.sh with ZLIB_SRC, ZSTD_SRC and GEXCC_PREFIX set; run.sh on edger under bench.sh): zlib 1.3.1
(compress2 + uncompress of 4 MiB at level 6), zstd 1.5.7 (level 3, both
ways), binary trees (malloc/free and pointer chasing) and a strings
kernel (strdup, qsort with a comparator, strlen, strchr), over a small
libc (size-class malloc, bulk-memory memcpy). Each variant in a fresh
node, warm-up run then median of 5, 5 rounds in rotating order; table
values are the median of the rounds' medians, in ms.

| variant | what | zlib | zstd | trees | strings | wasm bytes |
|---|---|---|---|---|---|---|
| static | `-fno-PIC`, absolute addresses | 283.5 | 34.1 | 26.0 | 43.9 | 478,507 |
| pie | gexcc today (`-fPIC`, hidden, `-pie`) | 284.7 | 35.0 | 27.1 | 43.0 | 483,388 |
| pie-dv | the same, `-fvisibility=default` | 285.7 | 34.3 | 26.3 | 44.3 | 483,413 |
| split | libc a side module, the rest a PIE importing it | 285.6 | 34.5 | 26.5 | 47.7 | 485,815 |
| lib | kernels and libc in a side module (a shared library) | 285.0 | 34.6 | 26.4 | 43.9 | 578,870 |
| lp64 | wasm64 (LP64) lowered to wasm32 (`--memory64-lowering`) | 321.4 | 31.1 | 28.9 | 45.5 | 486,193 |
| pie-opt | pie through `wasm-opt -O2` | 240.7 | 34.2 | 26.6 | 42.2 | 440,564 |
| asyncify-ind | pie-opt, instrumented where an indirect call is reachable | 241.4 | 34.2 | 26.3 | 46.0 | 466,120 |
| asyncify-all | pie-opt, every function instrumented | 245.7 | 37.4 | 34.9 | 49.8 | 600,482 |

(Two full runs agreed within 2-3%: the first, without the asyncify-ind
row, gave static 282.7/33.8/25.1/42.9 and pie 283.3/34.5/26.2/43.2.)

How to read it:
- **PIC is not where the cost is.** pie against static: zlib 0.4% slower
  (284.7 vs 283.5 ms), zstd 3% slower (35.0 vs 34.1), trees 4% slower
  (27.1 vs 26.0), strings 2% faster (43.0 vs 43.9): within a few percent,
  and gexcc pays it already. Default visibility changes nothing (pie-dv).
- **Calls across modules cost where a program makes many small libc
  calls:** split against pie is the same for zlib, zstd and trees, and 11%
  slower for strings (47.7 vs 43.0 ms: strdup/strlen/strchr/free per
  string, every one an import call into libc's instance). Code inside a
  library is as fast as in a program (lib against pie: all within 2%),
  though its module is 20% larger (578,870 vs 483,388 bytes: exports and
  GOT accesses for every default-visibility symbol).
- **LP64 lowered to wasm32 costs 7-13% on pointer-heavy code**: lp64
  against static, zlib 13% slower (321.4 vs 283.5 ms), trees 11% slower
  (28.9 vs 26.0), strings 4% slower (45.5 vs 43.9), and zstd 9% *faster*
  (31.1 vs 34.1: its 64-bit arithmetic on `size_t` matches the wasm64
  build's natural width). For item 3 that is acceptable against running
  the same library translated (several times slower).
- **asyncify:** instrumenting only what can reach an indirect call (what a
  shared library built without knowing its callers must do) costs
  nothing measurable on zlib, zstd and trees and 9% on strings (46.0 vs
  42.2 ms, the qsort comparator's callers) against the same binaryen -O2
  build. Instrumenting everything costs 2-31% (trees 34.9 vs 26.6 ms). So
  libraries take the indirect-call rule, never "everything".
- **Unplanned finding: binaryen -O2 makes zlib 16% faster** (pie-opt
  240.7 vs pie 284.7 ms), other kernels unchanged. gexcc runs `wasm-opt
  -O2` only on programs that fork (asyncify). Running it on every program
  is a separate, cheap change to evaluate on real programs.
- Compile time (V8, synchronous `new WebAssembly.Module`, Liftoff) was
  3.5-4.6 ms for every variant of these ~0.5 MB modules: not a
  differentiator here; the page-wide cache (§7.7) is about not repeating
  it per process for 2-30 MB programs.

### 7.4 How libc is shared

- `libc.so` = musl + libgex (dlopen, vfork, the setjmp runtime, signal
  frames) + libgexcc (loop safepoints), built `-fPIC -fvisibility=default`.
  Linked whole it is 761 KB of wasm (3898 functions; `scratch/CC/dl/out2`,
  musl's `pthread_cancel.lo` left out: its `__cp_*` labels are assembly
  markers wasm does not have). compiler-rt's builtins stay static in each
  module, as libgcc does.
- `crt1.o`, `main3.o` stay in the program. The program's `__gex_start`
  and `__gex_thread_start` move to libc (the entries gex calls), so they
  are found by export, not by the program's table.
- libc++/libc++abi/libunwind become `libc++.so.1`; the wasm exception
  tags (`__cpp_exception`, and `__c_longjmp` for setjmp) are defined once,
  in libc.so and libc++.so, and imported by everything else, so a throw
  or longjmp crosses modules.
- **TLS** per module: each module with TLS has its own `__tls_base`
  global and `__wasm_init_tls`. libc keeps musl's list of TLS modules
  (`libc.tls_head`), which the loader fills before `__gex_start` (size,
  alignment, the module's `__wasm_init_tls` as a table index); a thread's
  TLS area holds every module's block, and `__gex_thread_c` initialises
  each. A module `dlopen`ed later gets its blocks on each thread's next
  catch-up (§7.6).
- **Static stays.** `gexcc -static` (and the default until the shared
  path passes the suites) links today's way. `-shared` stops writing an
  archive and writes a side module when `GEXCC_SHARED=1`; `-lfoo` then
  prefers `libfoo.so` (as ld does) unless `-static`.

### 7.5 Fork, exec and the function table

- **Layout per process (the link map):** for each loaded module, its
  file's content hash, `__memory_base` (data allocated from the guest's
  address space with mmap by the loader, like `ld.so`) and
  `__table_base` (a range from the process's `ftab`). The program keeps
  its container's fixed data address.
- **Fork** copies memory, so every module's data and every function
  pointer in it is valid in the child if the child has every module at
  the same table base. The fork record (today: entry and one base,
  bdd34eb3) becomes the whole link map, `dlopen`ed modules included; the
  child instantiates them in order with `publish_at` (no data
  relocations, no constructors: the data is the parent's) and sets each
  module's `__tls_base` to the forking thread's value (exported as
  `__gex_tls_base`, a setter the loader calls: a mutable global's value
  cannot be passed in at instantiation).
- **asyncify across modules:** every module is instrumented with binaryen's
  `asyncify-relocatable` (its state and data pointer become imported
  globals, one pair per thread shared by all modules), so an unwind
  passes through frames of any module and the rewind replays them.
  Which functions are instrumented:
  - in a library: those that can reach `gex.fork_point` through its own
    calls, those that can reach an import that is itself marked
    "may fork" by the providing library, and those that can reach an
    indirect call (a callback that might fork). Each library records its
    exports' answers in a custom section (`gex.forkreach`: exports that
    may fork; exports that may call back);
  - in a program: `forkreach.py` as today, with the libraries' sections
    standing in for their code: an import may fork if the library says
    so, or if it may call back and the program has an address-taken
    function that may fork.
  The cost of the library rule is the `asyncify-ind` row above.
- **exec** replaces the image as today; the new program's modules take new
  table ranges (indices are never reused), its libraries new data.
- **Threads:** each Worker instantiates every module of the link map
  (they replay the `ftab` log already), with the thread's one
  `__stack_pointer` global for all of them.

### 7.6 dlopen

- libgex's `dlopen` asks the host (`gex.dlopen(path, flags)`): the loader
  maps the module and its `NEEDED`, resolves, publishes the installs in
  the `ftab` log, runs data relocations and constructors in the calling
  thread, and returns a handle; `dlsym` looks names up in the loader's
  symbol table (functions get a table slot on first `dlsym`, as
  `GOT.func` does).
- **Other threads** must not call into the new module before they
  instantiate it: a `call_indirect` to the resolve stub with another
  signature traps rather than resolving. So `dlopen` returns only after
  every thread of the process has caught up with the log: threads catch
  up at their next system call or loop safepoint (gexcc's GexPoll), and
  `dlopen` waits on that, as `membarrier` would.
- `dlopen` of a library that is already linked statically into the
  program (python's extension modules today) still finds it in the
  registry first, so nothing that works today changes.

### 7.7 Compile once per page

- Every module (program or library) is keyed by its content hash
  (sha-256 of the wasm), not its path: today's key (`wasmprog:<path>`)
  goes stale when apt replaces a file.
- The page keeps one map hash → `WebAssembly.Module` for all processes
  (host.mjs's per-process `#moduleCache` becomes page-wide for these
  keys) and hands modules to new Workers by reference. A second process
  running the same program, or any program using the same library, does
  not compile it again. This helps static programs too (bash exec'd by
  every `$(...)`), so it lands first, before shared libraries.
- **Across visits:** a module compiled from `WebAssembly.compileStreaming`
  of a same-origin URL gets V8's code cache (the optimised code is kept
  with the HTTP cache entry). The dist ships the image's libraries and
  hot programs as `wasm/<sha256>.wasm` beside the rootfs; the loader
  compiles from that URL when the hash matches, and from the bytes in
  the rootfs otherwise. Same for the providers of §7.9.

### 7.8 Item 3: wasm32 libraries under arm64 programs

- **Layout:** compile the library for aarch64's LP64 layout with clang's
  `wasm64` target (`long`, pointers, `size_t` 64-bit) plus
  `-funsigned-char`, and check every exported struct against
  `aarch64-linux-gnu` (`sizeof`/`offsetof`/alignment; `long double` is
  16 bytes, `__int128`, `wchar_t`, bitfields) with a layout test.
- **Addressing:** lower memory64 to the guest's 32-bit memory before
  shipping (`wasm-opt --memory64-lowering`: every address is an i64 in
  the code, wrapped to i32 at each access). The one place an address
  becomes a memory index is that wrap, so the dual heap (a check on high
  pointers routing them to a second heap) later replaces the lowering
  pass's wrap and nothing else. Measured cost: the `lp64` row of §7.3.
- **Loading** is the same `gex-dylink` loader, from the other side: a
  Debian arm64 object (`libz.so.1`) maps as today; the ELF-provider hook
  (docs/elf-providers.md) finds a matching wasm library (by soname and
  version, in `/usr/lib/gex/aarch64/`, packaged as `gex-providers`),
  loads it with its data from the guest's mmap, and binds each export at
  the arm64 export's address in the function table, through the AAPCS64
  adapter of provider.rs (x0-x7, v0-v7; `TPIDR_EL0` for errno). The
  library's own imports resolve to an **LP64 libc**: first a few
  functions (malloc/free/memcpy/abort for zlib and zstd, served by the
  provider heap), later musl built LP64 and lowered the same way, with
  glibc's aarch64 ABI on top (`struct stat`, `FILE`, `jmp_buf`,
  `pthread_*_t` sizes, symbol versions, `va_list` as aarch64's struct).
  Callbacks into translated code go through the provider guest-call path.
- **Order:** the V8 cost (§7.3, done), zlib end to end under a
  translated arm64 program (python3.13's zlib module, or `gzip` from
  Debian), then libzstd, then libc.

### 7.9 Per-function providers in the demo (today)

The existing aarch64 export providers (glibc mem*/str*, printf family,
malloc, libm: gex-cgen `providers/`) bind only through a function-tier
unit cache, which the page reaches only with `?cgen=1` and a server for
`/gex-cgen/`; the static dist has neither, so the demo's translated
programs run glibc translated. Plan: ship the provider modules in the
dist (`gex/providers/<name>-<fingerprint>.wasm`, the unit cache's layout,
preloaded like an ejected page's list) and bind them without units;
measure apt, python3.13 and sqlite3 in the page with and without.

### 7.10 Order of work

1. Page-wide compiled-module cache keyed by content (§7.7): helps today's
   static programs.
2. Providers in the demo (§7.9), measured.
3. `gex-dylink` loader natively (wasmtime): `NEEDED`, GOT, TLS, fork link
   map, `dlopen`/`dlsym` (GTK's builder needs the program's own symbols);
   libc.so; tests/gexcc built shared and static, both passing.
4. The same in the browser shim; the page's code cache from the dist.
5. Debian packages shared: libc, libc++, then the large ones (glib/GTK,
   openssl, python's libpython, ncurses, readline); image sizes and page
   start-up measured against static.
6. Item 3: zlib under an arm64 program, then zstd, then LP64 libc.

### 7.11 As built (natively, 2026-10-09)

Step 3 of §7.10 is in: `crates/gex-dylink` (the shared half),
`shims/native/src/dylink.rs` (the native loader), gexcc's `GEXCC_SHARED=1`,
musl's `__gex_dsos` registry (our clone's `gex-wasm32-cc` branch).
tests/gexcc/run.sh passes with `GEXCC_SHARED=1` (every program against
libc.so) and without. Where it differs from the design above, or adds to it:

- **A library is a plain wasm module** (`\0asm`), not an ELF container:
  wasm-ld links against it directly, which build systems need. Programs
  keep their container.
- **NEEDED is the soname.** wasm-ld records the file name a library was
  given; gexcc hands it each library as a link named by its soname (the
  name section's module name, from `-Wl,-soname`), so a program linked
  against `libfoo.so` (the -dev symlink) needs `libfoo.so.1`.
- **Self-binding.** wasm-ld calls every weak definition of a shared
  library through an import of itself, `-Bsymbolic` or not (musl's weak
  aliases: `malloc`, `lseek`; every C++ inline function). `gex-dylink
  selfbind` rewrites those into direct calls at build time: the design's
  "no interposition into a library's own calls" made literal. One case
  needed care: musl's `__libc_start_main` now calls the `_init` crt1 hands
  it (in libc.so its own `_init` is libc's dummy).
- **Function pointers**: every exported function has a slot in its
  module's range (element positions, then the rest by name), so
  `GOT.func.f`, `dlsym` and the module's own `&f` agree.
- **libc.so** (`tools/gexcc/shared/mklibc.sh`, run by install.sh): musl,
  libgex and libgexcc but the loop safepoint, built by gexcc itself; 800 KB.
  `libc_nonshared.a` holds what stays in the program (`__main_argc_argv`).
  The safepoint counter is every module's own (a wasm global cannot be
  linked through a .so's symbols; libgexcc.a is linked into each module,
  its symbols hidden). setjmp's tag `__c_longjmp` is imported from libc by
  every module, so a longjmp crosses modules.
- **TLS**: a module's `_Thread_local` data works (its block is laid out by
  libc from the registry); a TLS variable *exported* to another module does
  not link (wasm-ld has no TLS GOT): export an accessor. A `dlopen`ed
  library with TLS is refused for now.
- **Fork summary (`gex.forkreach`)**, the format the page loader also
  reads: a custom section, UTF-8 text, first line `gex.forkreach 1`, then
  one export name per line, each an export that may unwind for a fork.
  gexcc computes it with forkreach.py over the module: reaching
  `gex.fork_point`, any `env` import the providing library's summary lists
  (or whose provider is unknown), or any indirect call. Two calls are taken
  not to reach: libgex's signal-handler call and `pthread_exit`'s cleanup
  (forking from a signal handler that interrupted a libc call, or from a
  cancellation cleanup, is unsupported in a shared program). libc.so's
  summary lists 312 of its exports: stdio (FILE's function pointers),
  qsort/bsearch, exit, pthread_once, fork; not malloc, the string
  functions, read/write. Instrumentation is binaryen's
  `asyncify-relocatable` on the reaching functions (its two globals are the
  store's, shared by its modules).
- **dlopen/dlsym**: libgex's dl.c asks the loader (`gex.dlopen`,
  `gex.dlsym`) after its static registry; handles are 0xfff00000 and up.
  `dlsym(RTLD_DEFAULT or dlopen(NULL), name)` sees every module's exports,
  the program's when linked `-rdynamic`, static programs included. Another
  thread sees a `dlopen`ed module when it next catches up with the function
  table's log (not yet forced at `dlopen`).
- **fpcast**: a static `-fgex-fpcast-emu -rdynamic` program (GTK's demos)
  exports a thunk of each exported function (`__fpcast$name`, `gex-dylink
  fpcast-prep/-post` around binaryen's pass) and function pointers to its
  exports are the thunks, so GtkBuilder's handlers found by `dlsym` are
  called through the uniform signature. Shared and fpcast do not mix yet:
  every module of a process, libc included, would need the pass with one
  parameter count.
- Not yet: the browser shim (`gex.dlopen` answers "not yet" there), C++
  across modules (libc++ is linked statically into each), shared Debian
  packages.

### 7.12 Replacing libc.so.6 itself: design, against a narrower step (2026-10-09)

Item 3 now has an LP64 libc *inside* its libraries (musl's `wasm64a64`
arch, tools/gexcc/item3/sysroot.sh): zlib, zstd (with its worker threads
and dictionary builder) link it, and nothing of it is visible to the
guest. The next step §7.8 names is libc.so.6 itself, glibc's, under a
translated program. What that requires, what a narrower step would do
instead, and what each buys.

**Where python3 and sqlite3 spend time in libc today.** `gexc run --record`
(exact block counts, natively; instruction counts are block sizes times
entries, so these are shares of guest instructions, not of time):

| workload (archbench) | libc.so.6 share | what it is |
|---|---|---|
| python3.13 py.py | 0.49% (3.5M of 707M) | pthread_self (500k calls), malloc/free, strerror_r, wcs* |
| sqlite3 sq.sql | 4.1% (44M of 1090M) | memcpy/memcmp/memset (the page already serves these as intrinsics), malloc/free, pthread_mutex_lock/trylock/unlock |

The program's own code is 95.9% (sqlite) to 99.5% (python): neither is
libc-bound, so neither replacement moves these two by more than a few
percent. Programs that would gain are stdio- and formatting-heavy
(apt/dpkg's text processing, `sort`, `grep` with locales), not these.

**A. Replace libc.so.6 whole** (an LP64 wasm provider bound at every
export, as libz.so.1 is now). What the guest and its loader see of a libc
has to stay glibc's, because translated code (the program, every other
library, ld.so) is compiled against it:

- *Symbol versions.* ~2,900 versioned exports (`memcpy@GLIBC_2.17`,
  `pthread_cond_wait@@GLIBC_2.17` and its older twins). Binding is by
  address (libprov), so each version's address gets the right adapter;
  where two versions differ in behaviour, two adapters.
- *Data the guest reads directly.* `stdin/stdout/stderr` are glibc
  `FILE *` that translated code passes to `putc_unlocked`-style macros
  and reads fields of (`_IO_read_ptr`/`_IO_write_ptr`), `errno` through
  `__errno_location`, `environ`, `__libc_single_threaded`, `optarg/optind`,
  `h_errno`, the `ctype` tables behind `__ctype_b_loc` (the guest indexes
  them inline), `timezone/daylight/tzname`. All of these must be glibc's
  layout in guest memory, kept current by the wasm side.
- *TLS and the thread pointer.* glibc's `struct pthread` lives at
  `TPIDR_EL0` with the stack guard and pointer guard at fixed offsets that
  translated code reads inline (`-fstack-protector`: `__stack_chk_guard`
  is a global on aarch64, but `setjmp`'s pointer mangling and the dynamic
  TLS (`__tls_get_addr`, the DTV) are ld.so's and libc's together).
  `pthread_t` is the TP-relative struct address; `pthread_mutex_t`,
  `pthread_cond_t`, `pthread_attr_t`, `sem_t` are caller-allocated with
  glibc's sizes (mutex 48 bytes, cond 48, attr 64) and static initializers
  baked into translated code. Our implementation must use those layouts
  (or keep a side table keyed by address).
- *ld.so.* glibc's ld.so and libc.so.6 are one unit: `_rtld_global`,
  `_dl_*` hooks, `__libc_early_init`, `dl_iterate_phdr`, `dlopen`
  (`libdl` is in libc since 2.34) share private structures. Either ld.so
  stays translated and our libc implements its private interface
  (GLIBC_PRIVATE exports: dozens, undocumented, version-specific), or ld.so
  is replaced too, which means our own loader for translated ELF objects
  (gex-ld has the parts: relocation, symbol lookup, TLS) inside the guest.
- *Signals.* Handlers are translated code; `sigaction`'s `SA_RESTORER`,
  `ucontext_t` (aarch64 `mcontext` with the FP/SIMD record) must be the
  kernel's. Delivery at a wasm libc's syscall works now (rtmusl.c runs the
  handler as a guest call and sigreturns); a signal while wasm code runs
  long (a big `qsort`) waits for the next syscall.
- *setjmp/longjmp crossing.* A translated `longjmp` over wasm frames (a
  `qsort` comparator, a `bsearch`, `pthread_once`, `atexit` handlers
  that longjmp) must unwind the wasm stack: the provider's `provider_try`
  / exception tag path (Unity's) handles throws through provider frames;
  `jmp_buf` itself is glibc's 22-word aarch64 layout with mangled SP/LR.
- *fork.* Done for a provider's heap and state (be9eba23); with all of
  libc in wasm, `fork`'s atfork handlers and the stdio locks are ours.
- *Locale and iconv.* glibc's locale archive format, `LC_*` categories
  and gconv modules (dlopen'd .so's, translated) would be ours or
  re-implemented over musl's much smaller set; programs expecting glibc
  collation would see differences.

Size: weeks of agent time, mostly in the glibc-compat layer, with a long
tail of programs that depend on a glibc detail; and the gain on the two
measured workloads is at most their 0.5% and 4%.

**B. The narrower step: more libc exports as per-function providers**
(docs/elf-providers.md: bound by address, the rest of glibc stays
translated and owns its state). What exists: mem*/str* (served as
intrinsics or `glibc_mem_a64.inc`), printf's formatting core
(`glibc_printf.c`, x86_64), libm, and a malloc family (`glibc_malloc.c`,
which measured no faster than translated ptmalloc: 177 vs 175 ms for 400k
pairs, so it is not registered). Candidates by the profile above:
`pthread_mutex_*` (sqlite: 3.4M calls; the uncontended path is a CAS that
tier 1 translates well already), `pthread_self` (python: 500k, already
two instructions), `strtod`/`printf` family and `qsort` for text tools.
Each is a few hundred lines with the stateless/stateful rules elf-providers
already has, and each is measured alone. Gain: a few percent at most on
python3/sqlite3; more on formatting-heavy tools.

**C. What actually moves these workloads** is the program's own code: the
function JIT and AOT tiers on python3.13 (99.5%) and libsqlite3 (95.9%),
and whole libraries as wasm providers where a library is the hot part
(item 3's zlib under python3.13: compress 1.73x faster than translated,
306 vs 530 ms; zstd level 3 at tier 1: 3.6x faster than translated, 23 vs
82 ms; edger, medians of an ABBA set). libsqlite3 itself is a pure-C ABI like zlib's and
could be the next item-3 library: 96% of sqlite3's instructions.

**Recommendation:** not A for now. B only per measured hot function. Make
libsqlite3 the next item-3 library, and keep the LP64 libc as the inside
of such libraries.

**Decision (user, 2026-10-09): libraries, not libc.** libc.so.6 is not
replaced. Whole hot libraries become item-3 providers on the LP64 libc
(libsqlite3 next), and per-function libc providers are added only where a
profile shows a hot spot.

### 7.13 libsqlite3: function pointers across the boundary (2026-10-09)

`tools/gexcc/item3/build-sqlite.sh` builds Debian's libsqlite3 (3.46.1, the
amalgamation with debian/rules's options and what its configure adds: FTS3/4/5,
RTREE, JSON, SESSION, threadsafe) on the LP64 libc. It replaces
libsqlite3.so.0 under Debian's arm64 sqlite3 shell, bound by soname from
`/usr/lib/gex/aarch64/libsqlite3.so.0.wasm`. 284 exports have adapters
(everything the build defines). Unlike zlib's and zstd's API, sqlite's has
callbacks in both directions, plus doubles and varargs:

- **Exits** (the library calls the guest). sqlite stores and calls the
  caller's function pointers: sqlite3_exec's callback, the methods of a
  virtual table module (the shell's generate_series), application SQL
  functions (the shell's ieee754, decimal, sha3), destructors, and a
  registered vfs. The pass `item3/a64icall/GexA64Icall.cpp` is the arm64
  counterpart of Wine's ICALL pass, and every indirect call goes through it:
  `__a64_set_icall_target(p); callee = p < 0x3f0000 ? p : __a64x.<sig>`.
  A table index (the library's own function) is called directly. A guest
  address goes to an exit stub, one per signature, that lays the arguments
  out as AAPCS64 does and calls `a64_call_guest_regs`:
  - integers and pointers go in x0-x7, floating point in v0-v7, and the
    rest in 8-byte stack slots below the guest's SP;
  - the guest's x30 and SP are restored afterwards.
  In sqlite3.c that is 5187 call sites. 23 are left alone (byval or
  variadic: only the library's own functions are called through those).
- **Entries** (the guest calls the library's function pointers). Examples
  are the methods of `sqlite3_vfs_find("unix")`, which the shell's
  appendvfs calls, and the io methods of a file it opened. Every
  address-taken function gets an F_body entry thunk: it reads the AAPCS64
  arguments, calls the function, and returns in x0 or v0. That is 895 in
  sqlite3.c. Their {function, thunk} pairs go in the data section
  `gexa_entries` (exported bounds `__gexa_entries`, `gexa.c`). After the
  install, the shell (`gex_libprov_installed`) binds each thunk at its
  function's table index as a guest address, through wasmdll's late
  binding. Table indices are below 0x3f0000, and guest code is above it.
- **Varargs.** sqlite3_mprintf, sqlite3_config, sqlite3_db_config and the
  other variadic exports, plus the va_list ones (sqlite3_vmprintf, which
  the shell calls with its own va_list), go through `gexva.h`. It is
  force-included and turns sqlite's `va_list` into a two-kind struct:
  - kind 0 is wasm's own varargs, for the library's calls among its own
    functions;
  - kind 1 is an AAPCS64 va_list (`__stack`, `__gr_top`, `__vr_top` and
    the offsets), read as aarch64 code would read it.

  A variadic export's adapter announces the guest's call by name
  (`gexva_pending`). The export's `va_start` takes the announcement when
  `__func__` matches, and snapshots x0-x7 and v0-v7 from the context. A
  va_list argument is the guest's own struct (passed by reference, being
  over 16 bytes).
- **Doubles** are v registers both ways (`a64_varg_double`, `a64_ret_v`).
- **compiler-rt's tf2 comparisons** (`__lttf2`...) are typed `long` for
  every LP64 target but aarch64, while clang's wasm64 calls expect `int`.
  The signature mismatch became a trap stub in the link, so sysroot.sh now
  builds them from a patched copy.

Checked: the shell's output is identical, translated against the provider,
on sq.sql and on a callback-heavy script (cb.sql). That script covers
generate_series, ieee754/decimal, sha3_query (a guest function that calls
back into prepare/step), box mode, `.schema`, FTS5, `.dbinfo`, `.stats`,
and a file database (the unix vfs, locks, journal). The only difference is
`.stats`'s memory lines, by 8 bytes: Debian's build sizes allocations with
glibc's malloc_usable_size.

Measured (edger, run-sqlite.sh: the browser runtime under node, one ABBA
set, medians of two runs; sq.sql makes 100k rows, an index, a LIKE scan and
a GROUP BY). The time is the statements' summed real time from sqlite's
timer:

| tier | translated | provider | provider vs translated | wall, translated vs provider |
|---|---|---|---|---|
| tier 1 (the page's default) | 7712 ms | 666.5 ms | 11.6x faster | 10288 vs 2627 ms |
| function JIT | 13201 ms | 1309.5 ms | 10.1x faster | 18872 vs 5741 ms |

The output is the same in every run. The function JIT is slower than tier 1
here in both modes (13.2 vs 7.7 s translated, 1.31 vs 0.67 s provider),
which is open.

**Fork across a provider.** A fork's child installs each inherited provider
at its parent's table base (`publish_at`; the base is kept at offset 8 of
the provider's code page, which the child inherits). The data the child
copied holds the parent's function pointers: zlib's `zalloc = zcalloc` in an
open stream, and sqlite's methods. Before this, a program exec'd from a
shell got a different base in its child, so zlib's own allocator looked like
a guest function.

**In the image.** `tools/gexcc/item3/mkdeb.sh` builds the three providers
into the package `gex-providers-arm64` (/usr/lib/gex/aarch64/*.wasm).
web/build-native-debian.sh (and scratch/CC/mkgui.sh) put it in the pool, so
the native-debian images carry it, and an arm64 program that maps one of
the libraries (apt's libz and libzstd) gets the provider by default.

**Gate leg:** `tests/gexcc/item3/run.sh GEXC`. It builds the providers, zt
and zstt, then runs every check twice, translated and with the providers,
each time in one guest process tree under a wasm32 dash: zt check/checkgz,
zstt check/checkd/checkm, and sqlite3 on sq.sql and cb.sql. It passes when
19 checks say ok in both modes, all three libraries are replaced in the
provider run only, and the sqlite3 output matches.

## 8. A translation cache that survives a page reload (design, 2026-10-11)

### Why
In the page, a cold `node -e 0` takes 40.6 s and a warm one about 30 s. Most of the difference is 9.6 s spent translating 224k blocks (§6.6). Every reload starts with an empty module store, so it pays that 9.6 s again. Backlog item 3 (C) and T5.

### What is cached, and what is not
The cache saves the **global kernel's module store** and the slot numbering its modules were built against. It does not save compiled `WebAssembly.Module`s or any process's registry. Each saved record holds:

- every stored module: its wasm bytes (what `k_mod_store` received), its `slots`, and its `dirs` (`(page, base)` pairs);
- the claims: `by_page` (`(page, key) -> ids`), `by_object` (`obj -> ids`), and the `translated` object set;
- the slot map (`(page, key) -> base`, from `k_slot_put`) and the slot mark.

### What the cache keys on
- **A fingerprint for the whole cache.** It hashes:
  - the shim and kernel wasm bytes the page loaded (arch, mem64 and shared variants are different files);
  - the query switches that change emission (`fjit`, `dense`, `dual`, `fjitlibs`).

  If the fingerprint differs, the whole cache is dropped. This also covers Rust's `DefaultHasher`, which the object and page identities use and whose output is not promised to stay the same across toolchains.
- **The content of each object.** Today an object's identity is SipHash of its file path, and a page's identity is SipHash of path plus offset (`object_identity` and `page_identity` in shims/browser/src/lib.rs). Neither depends on the file's bytes, so a library that apt replaced at the same path would keep its old translations.

  The cache therefore stores, per key, its path, and per path the file's size and mtime as they were when saved; nothing is hashed. The shim tells the kernel once per key per Worker which path the key belongs to (`k_key_path(key, path, size, mtime)`, from `note_key`).

  A restored key is checked the first time a process computes it, not at restore: in the page, pid 1 restores before apt has installed anything, so at restore most files are not there yet (dpkg keeps the package's mtimes, so a reinstalled file matches). The same size and mtime confirm every restored key of that path. Anything else, or the file gone, drops those keys' modules, claims, `translated` marks and slot-map entries, and the path's record takes the file as it is now. The dropped slots are left unused, which wastes them but is safe.

- **Placement.** Keys include absolute dispatch pages. A library loaded at a different address simply misses; nothing is relocated.

### Renumber on install
- The kernel store already holds modules before `dense::register` numbers them. `share_module` runs before `push_pending`, and `push_pending` numbers each process's copy from its own `NEXT` (shims/browser/src/dense.rs).
- So the saved bytes are unnumbered, and every process that installs a restored module renumbers it, exactly as it renumbers a module another process stored this session.
- This answers T1 for the cache. Within one session, a Worker's dense base already comes from its own numbering; fork adoption takes the parent's base on purpose. A stored compiled `Module` or registry entry would carry an earlier session's base, so neither is ever saved.
- Before saving, each module's dense section is checked for base 0, and zeroed with `gex_emit::dense::set_base` if not.

### Slot-map replay
- Classic tier-1 slot bases come from `k_slot_alloc`. They are baked into each module's element segments and into the `PAGE_GEN` values that function-JIT code checks. A saved module is valid only where `(page, key)` maps to the bases it was built with.
- Restore therefore runs in the kernel before pid 1 starts, which is before any `k_slot_alloc`:
  - it replays every saved `(page, key) -> base` with `k_slot_put`;
  - it raises the mark with `k_slot_floor` to the highest saved base plus that page's slots.
- Modules are re-stored in their saved order, so module ids, which are store positions, come back the same. The claims are then replayed.
- If the restored slots would leave less than half of `k_slot_left` free, nothing is restored. Slots are a budget for the whole page, and no slot is ever reclaimed.

### Where it lives
- **Storage:** one OPFS file per fingerprint, holding a header and the records, written from the page's main thread.
- **Saving:** a kernel export serialises the store (`k_store_save`). The host calls it when 30 s pass with no new module, and on `pagehide`. Writes are capped at 128 MiB of module bytes, against the store's own 256 MiB `MOD_BUDGET`.
- **Restoring:** `k_store_restore(bytes)` runs once at boot, after the filesystem is ready and before the first process.
- **Opt-in first:** behind `?xcache=1`. It becomes the default only after the measurement below.

### Who else uses the store
- W's wasm DLLs and the Mesa work use the same store and slot map. Their objects get keys, paths and content hashes the same way. Nothing changes for code that does not opt in.
- The one change visible in the shim is the new `k_obj_name` call next to the existing claims.

### Measured how
- On edger, in the page, measure cold `node -e 0` and `apt install ripgrep tcc libc6-dev`.
- Load twice in one browser profile. The second load runs with `?xcache=1` for "after", and without it for "before". This needs site.mjs to have a mode that reloads with a persistent profile.
- Report translation counts as well as times.

## 9. The demo: a desktop, a small base, the rest on demand (plan, 2026-10-11)

What a visitor opens is one page that looks like a desktop, not a list of
demos. It is its own build target, `web/build-demo.sh` → `web/dist-demo`,
always stripped, and no test runs on it (one smoke leg: the desktop comes
up with both windows).

**The page.** The compositor's output (`compositor.js`, WebGPU scene mode,
which draws window decorations itself) fills the page. The shell chrome --
a panel with the launcher button, a clock and the console toggle -- is DOM,
drawn by the page: it costs the guest's rootfs nothing and needs no guest
process. Two windows are open at start:

- **xterm running bash.** Debian's arm64 xterm (translated) on the page's
  X server (`hostX`, `web/public/gex/xwayland/`), its shell our wasm32 bash.
- **The launcher**, a page-side window listing what can be added: gears,
  the GTK 3 demo, foot, nvim, node, apt, and the programs of the old demo
  list worth keeping. Choosing one downloads it (once), adds it to the
  filesystem and runs it.

pid 1 is bash reading commands from the page's terminal (hidden; the panel
shows it as a console). The launcher starts a program by writing its command
line there, so no guest-side launcher exists.

**The base rootfs** is what bash and xterm need and nothing else:
`mkrootfs.py --roots base-files,base-passwd,ncurses-base --extra xterm
--ours bash,mawk,sysvinit-utils` (mawk and sysvinit-utils only satisfy
base-files' and x11-common's dependencies), then pruned of the fonts xterm
does not use (core fonts come from the X server; DejaVu Sans Mono stays for
`-fa`), X11 locales other than C/en_US.UTF-8, and debconf's perl.

**Everything else comes at run time** as zstd-compressed downloads, in one
of two forms, and the numbers decide which the launcher uses:

- (a) a **tar overlay** per launcher item: the item's closure minus the
  base, built by `mkrootfs.py` as base+item and diffed against the base. The
  page fetches `pkgs/ITEM.tar.zst`, decodes it with zstddec and writes the
  files straight into the global kernel's filesystem from a Worker
  (`k_fs_add`, `k_fs_symlink`, `k_fs_chmod`), while the guest runs. Its dpkg
  status stanzas go along, so a later `apt` sees the item as installed.
- (b) **.deb**: `dpkg -i` of prefetched .debs, or `apt install` from the
  network (Wisp). apt and dpkg are themselves an overlay (they are not in
  the base).

Measured on edger for two or three items (apt from the network, dpkg -i of a
prefetched .deb, the tar overlay); the expectation is tar overlays for the
launcher and apt for what the user installs, but the measurement decides.

**Stripped, always.** `wasm-tools strip` takes debug sections off every
.wasm (the name section stays: a trap still names its function). Runtime
.wasm files ship only as `.wasm.zst` (`zstd --ultra -22`) and a demo build's
page decodes them with zstddec: one `fetch` wrapper in the page and in every
Worker, so the shim, kernel, providers, Xwayland and Mesa all go through it.
The base rootfs ships as `rootfs/base.tar.zst` (`--ultra -22`).

**Milestones**, each served at a directly openable https URL (100.64.0.3):

1. base rootfs and the stripped build (sizes, compressed and not);
2. the desktop with both windows;
3. the launcher and on-demand packages, with the apt / dpkg / tar numbers;
4. time to desktop and download size, on edger.

### 9.1 As built and measured (2026-10-11)

`web/build-demo.sh` (items: `web/demo-items.json`, overlays:
`tools/gexcc/debian/mkoverlay.py`); the smoke and every number below:
`tests/demo/desktop.mjs` (`DEMO_STEPS`), headless Chromium on edger's
Quadro P620, fresh profile per visit, the dist served from 127.0.0.1 (so
fetch times are local-disk, not a real network).

**Base rootfs** (user, same day: "the base first-load should have coreutils
at least"): bash, dash, xterm and its arm64 closure, and ours coreutils,
grep, sed, findutils, procps, less, ncurses-bin, gzip, tar, util-linux,
hostname, nano, debianutils: **38.8 MiB tree / 38.8 MiB tar / 8.3 MiB zstd**
(bash + xterm alone: 20.6 / 20.4 / 5.7). What each tool adds is listed in
build-demo.sh; the rest (file, gawk, diffutils, zstd, xz, bsdextrautils,
tree: 1.5 MiB) is the launcher's "More tools".

**Runtime modules**, stripped (name section kept) then zstd -22: shim
50.5 → 5.7 → 1.2 MiB, kernel 15.2 → 0.4 → 0.1, Xwayland 2.0 → 2.0 → 0.5,
its bridge 11.9 → 0.2 → 0.05; Mesa (fetched on the first GL item only)
19.6 → 19.6 → 3.3. **First load: 11.4 MiB** (runtime 1.9, rootfs 8.3, page
JS about 1).

**Time to desktop** (navigation → xterm mapped, launcher open): 4.4–5.1 s
over 12 visits (median about 4.6 s). Downloads and unpack are done by about
0.6 s and pid 1 starts by about 0.9 s; the rest is the X server and the
translated xterm/libX11 coming up.

**Launcher items, three ways** (same items, ms; tar = fetch + zstd decode +
write into the running kernel; dpkg/apt = the guest's own clock, after the
apt overlay, 10.1 MiB zstd, 0.4–0.5 s):

| item | packages | tar overlay | dpkg -i of prefetched .debs | apt-get install (network) |
|---|---|---|---|---|
| htop | 1 | 26–41 (0.27 MB) | 1,176–1,381 (.debs 0.25 MB) | – (ours; pinned) |
| x11-apps | 18 | 106–112 (2.5 MB) | 20,218–21,115 (4.8 MB) | 26,138–26,353 |
| neovim | 12 | 317–352 (7.2 MB) | 13,420–13,449 (7.6 MB) | 28,617–28,737 |

`apt-get update` alone (trixie arm64 indexes over Wisp): 9.1–9.3 s on
wss://anura.pro/, 19.0–26.0 s on wss://wisp.mercurywork.shop/. dpkg -i takes 29 to 199 times as long as the overlay
for the same files (maintainer scripts, triggers and dpkg itself run
translated), and apt adds the network on top. So: **the launcher uses tar
overlays; apt stays for what the user installs themselves.** Overlays also
record their packages in dpkg's status, so apt sees launcher items as
installed.

Two fixes the measurements forced: the base needs dash (dpkg refuses to
run without `sh`), and the trees keep debconf (`man-db`'s postinst sources
`/usr/share/debconf/confmodule`). apt installing a Debian package that
depends on one only we build (man-db → bsdextrautils) needs our .debs:
the apt overlay registers the whole pool (56 MB) as lazy files under
/var/lib/gex-apt, fetched from the page only when apt reads one.

### 9.2 What the first users hit, and the fixes (2026-10-12)

- **Blurry output.** The canvas was sized in CSS pixels. It is now sized in
  device pixels, and guest Wayland clients are told the rounded
  devicePixelRatio as `wl_output.scale`. `set_buffer_scale` is honoured, and
  each record carries the buffer and client scales in its flags word, bits
  8–15 and 16–23. X windows stay at scale 1 in device pixels, and xterm is
  sized through `/root/.Xdefaults` (`Xft.dpi`). Where nothing is scaled, a
  draw samples nearest, so a terminal's glyphs are its own pixels.
- **Two pointers.** No cursor is composited any more. The client's cursor
  surface (or Xwayland's) becomes the canvas's CSS cursor as a `url()` with
  its hotspot, cached by content; a null cursor becomes `none`, and the
  desktop and window frames get `default`.
- **glxgears from the xterm: "couldn't get an RGB, Double-buffered visual".**
  Debian's glvnd `libGL` had no vendor library, because the Mesa drivers were
  pruned from the trees. The gears item maps gex's marshallers onto
  Debian's `libGL.so.1`, `libEGL.so.1` and `libGLESv2.so.2`, and the page
  starts Mesa on a guest's first GL request (`glLazy`).
- **htop with no processes.** `/proc` listed only `self`. Each process now
  tells the process table its name and command line once per image, and
  `/proc` lists every pid with `stat` (all 52 fields), `status`, `statm`,
  `cmdline` and `comm`, plus `loadavg`.
- **nvim ignored typing.** The pty answered FIONBIO with ENOTTY, so libuv's
  `uv_pipe_open` failed on the terminal. FIONBIO and the close-on-exec
  ioctls (FIONCLEX, FIOCLEX, FIOASYNC) are now left to the kernel.
- **xterm not presenting until refocused: open.** With no input, the
  compositor draws one frame per line the xterm prints, and the page's canvas
  holds each of them; `tests/demo/desktop.mjs`'s presentation leg checks
  that by drawing the canvas into a 2D canvas. Headless Chromium's
  screenshots never contain a worker's WebGPU canvas, so what a person's
  browser displays is not covered by any test here.

Legs: `node tests/demo/desktop.mjs web/dist-demo` (the desktop, then the
cursor and presentation legs; `DEMO_DPR=2` for the device-pixel output).
