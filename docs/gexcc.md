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
