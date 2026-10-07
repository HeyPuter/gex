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

**Browser** (shims/browser under Node, `tests/gexcc/run-browser.sh`):
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

- **Slots** (`gexcc-exec`): at most `GEXCC_SLOTS` (4) host-started gexcc
  programs run at once per user. The rest wait on an flock. Guest execs take
  none. Tested by `tests/gexcc/slots/run.sh`: at most 2 at once with 2
  slots, all 6 overlap without.
- **Every run** is wrapped in `systemd-run --user --scope -p TasksMax=256
  -p MemoryMax=32G nice -n 19`, one suite at a time, `-j2`.
- **Not converted yet** (refused without `HOSTRUN=1`): gawk, tar (autotest),
  make and curl (perl runners), bash, mbedtls and fmt (ctest), perl, python3.

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
- **Next:**
  1. Rerun the corpus clean.
  2. coreutils, bash, make.
  3. perl, python3 (recipes written), curl+mbedtls, libevent+tmux, vim,
     fmt (C++), openssh, git.
  4. The loopback TCP EPIPE/hang.
  5. Measure GexPoll's cost on edger.
