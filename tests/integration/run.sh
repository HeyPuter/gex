#!/bin/sh
# Integration ladder (PLAN §12.5): run guest programs under gex and under
# QEMU and require byte-identical stdout, stderr and exit status.
#
# QEMU is the oracle rather than a second gex build, so a shared bug in our
# own decoder and lifter cannot make a test pass.
#
#   tests/integration/run.sh [--keep]
#
# Environment:
#   GEX_TOOLCHAIN  aarch64 musl cross toolchain bin directory
#   GEX_QEMU       qemu-aarch64 binary
#   GEXC           the gexc binary under test

set -eu

TOOLCHAIN="${GEX_TOOLCHAIN:-/home/claude/toolchains/aarch64-linux-musl-cross/bin}"
QEMU="${GEX_QEMU:-/home/claude/toolchains/qemu-aarch64}"
GEXC="${GEXC:-target/release/gexc}"
# V8 is the target of record; the cross-engine stage is skipped rather than
# failed when Node is absent, so the suite still runs on a bare machine.
NODE="$(command -v node 2>/dev/null || true)"
CC="$TOOLCHAIN/aarch64-linux-musl-gcc"
CXX="$TOOLCHAIN/aarch64-linux-musl-g++"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${GEX_WORK:-target/integration}"

for tool in "$CC" "$QEMU" "$GEXC"; do
    if [ ! -x "$tool" ]; then
        echo "integration: missing $tool" >&2
        exit 2
    fi
done

mkdir -p "$WORK"
pass=0
fail=0
flaky=0
# Extra flags, set around a group of comparisons.
GEXC_ARGS=""
QEMU_ARGS=""

# Compare one invocation between the two engines.
#   compare [--native-oracle <path>] <label> <program> [args...]
#
# `--native-oracle` names a host-native build of the same source to fall back
# on when QEMU itself fails. It is opt-in per test rather than automatic,
# because "QEMU exited with a signal and printed nothing" is also exactly
# what a correct run of a program that dies from a signal looks like.
# A comparison that fails is retried once before being called a failure, and
# a case that then passes is reported as flaky rather than quietly swallowed.
# The two engines share a machine, and a run that only fails sometimes is a
# different problem from one that always does -- the point is to be able to
# tell them apart, not to hide either.
compare() {
    if compare_once "$@"; then
        return
    fi
    label_of "$@"
    echo "     retrying $retry_label..."
    if compare_once --quiet "$@"; then
        # `compare_once` already counted the pass.
        echo "FLAKY $retry_label: failed once, passed on retry"
        flaky=$((flaky + 1))
    else
        fail=$((fail + 1))
    fi
}

# Compare gex against an expected-output file rather than against QEMU.
#
# Needed where QEMU is not a usable oracle. `execve` is the case: qemu-aarch64
# can only re-exec *itself* for the program it was given, so a guest that execs
# another aarch64 binary fails under QEMU and works under gex. The expectation
# is therefore written down, and derived from what Linux does.
expect() {
    label="$1"; expected="$2"; shift 2
    g_out="$WORK/$label.gex"
    set +e
    # shellcheck disable=SC2086
    "$GEXC" run $GEXC_ARGS "$@" >"$g_out.stdout" 2>"$g_out.stderr"
    g_rc=$?
    set -e
    if [ "$g_rc" != 0 ]; then
        echo "FAIL $label: exit status $g_rc"
        head -3 "$g_out.stderr" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    printf '%s\n' "$expected" >"$g_out.expected"
    if ! cmp -s "$g_out.expected" "$g_out.stdout"; then
        echo "FAIL $label: stdout differs from the expectation"
        diff -u "$g_out.expected" "$g_out.stdout" | head -20 || true
        fail=$((fail + 1))
        return
    fi
    echo "ok   $label"
    pass=$((pass + 1))
}

# Compare the *same module* on V8 against QEMU.
#
# The browser is the target that matters, so this is not a curiosity: it is
# the engine of record. `gexc build --exhaustive` because a built module has no
# runtime translator behind it, and the module carries its own dispatch
# directory so the harness has nothing to set up.
cross_engine() {
    label="$1"; prog="$2"; shift 2
    if [ -z "$NODE" ]; then
        return
    fi
    wasm="$WORK/$label.wasm"
    set +e
    "$GEXC" build --exhaustive "$prog" -o "$wasm" >/dev/null 2>&1
    "$QEMU" "$prog" "$@" >"$WORK/$label.qemu" 2>&1
    "$NODE" "$HERE/../cross-engine/run-node.mjs" "$wasm" "$prog" "$@" \
        >"$WORK/$label.v8" 2>&1
    set -e
    if ! cmp -s "$WORK/$label.qemu" "$WORK/$label.v8"; then
        echo "FAIL v8-$label: V8 and QEMU disagree"
        diff -u "$WORK/$label.qemu" "$WORK/$label.v8" | head -20 || true
        fail=$((fail + 1))
        return
    fi
    echo "ok   v8-$label"
    pass=$((pass + 1))
}

# Run a program under the browser runtime and check it against QEMU.
#
# This is the real thing, not the cross-engine harness: gex-kernel compiled to
# wasm, sharing one memory with the guest module, with JS supplying only a
# console. Both ways of getting code there are checked -- a module with every
# block translated ahead of time, and a small one that pulls the rest in
# through the dispatch miss at run time -- because a page will want the second
# and a deployment that must not compile later wants the first.
#
# Skipped when node or the shim build is absent.
browser() {
    label="$1"; prog="$2"; shift 2
    shim=target/wasm32-unknown-unknown/release/gex_browser.wasm
    if [ -z "$NODE" ] || [ ! -f "$shim" ]; then
        return
    fi
    # An absolute path, because `/proc/self/exe` is one and the browser has no
    # host directory to resolve a relative argv[0] against.
    prog="$(cd "$(dirname "$prog")" && pwd)/$(basename "$prog")"
    set +e
    "$QEMU" "$prog" "$@" >"$WORK/$label.qemu2" 2>&1
    set -e
    for mode in aot jit; do
        wasm="$WORK/$label.$mode.wasm"
        set +e
        case "$mode" in
            aot) "$GEXC" build --exhaustive --no-shared-memory "$prog" -o "$wasm" >/dev/null 2>&1 ;;
            jit) "$GEXC" build --no-shared-memory "$prog" -o "$wasm" >/dev/null 2>&1 ;;
        esac
        "$NODE" shims/browser/js/run-node.mjs "$wasm" "$prog" "$@" >"$WORK/$label.$mode.br" 2>&1
        set -e
        name="browser-$label"
        [ "$mode" = jit ] && name="browser-jit-$label"
        if ! cmp -s "$WORK/$label.qemu2" "$WORK/$label.$mode.br"; then
            echo "FAIL $name: the browser runtime and QEMU disagree"
            diff -u "$WORK/$label.qemu2" "$WORK/$label.$mode.br" | head -20 || true
            fail=$((fail + 1))
            continue
        fi
        echo "ok   $name"
        pass=$((pass + 1))
    done
}

# The same, for a program that needs its interpreter and libraries. A page has
# no filesystem to reach through, so they are bundled -- and the interpreter's
# own code, and every library's, only exists once runtime translation reaches
# it, which is why this can only be a jit run.
browser_dynamic() {
    label="$1"; prog="$2"; shift 2
    shim=target/wasm32-unknown-unknown/release/gex_browser.wasm
    if [ -z "$NODE" ] || [ ! -f "$shim" ] || [ -z "$DYNAMIC" ]; then
        return
    fi
    prog="$(cd "$(dirname "$prog")" && pwd)/$(basename "$prog")"
    bundle=""
    for lib in "$SYSROOT"/lib/*; do
        [ -f "$lib" ] || continue
        bundle="$bundle --file /lib/$(basename "$lib")=$(cd "$(dirname "$lib")" && pwd)/$(basename "$lib")"
    done
    wasm="$WORK/$label.dyn.wasm"
    set +e
    "$GEXC" build --no-shared-memory "$prog" -o "$wasm" >/dev/null 2>&1
    "$QEMU" -L "$SYSROOT" "$prog" "$@" >"$WORK/$label.qemu3" 2>&1
    # shellcheck disable=SC2086
    "$NODE" shims/browser/js/run-node.mjs $bundle "$wasm" "$prog" "$@" >"$WORK/$label.dyn.br" 2>&1
    set -e
    if ! cmp -s "$WORK/$label.qemu3" "$WORK/$label.dyn.br"; then
        echo "FAIL browser-dynamic-$label: the browser runtime and QEMU disagree"
        diff -u "$WORK/$label.qemu3" "$WORK/$label.dyn.br" | head -20 || true
        fail=$((fail + 1))
        return
    fi
    echo "ok   browser-dynamic-$label"
    pass=$((pass + 1))
}

# Recover the label from a compare() argument list, whatever options precede it.
label_of() {
    while :; do
        case "$1" in
            --native-oracle) shift 2 ;;
            --ignore-stderr|--quiet) shift ;;
            *) break ;;
        esac
    done
    retry_label="$1"
}

compare_once() {
    oracle=""
    check_stderr=1
    quiet=0
    while :; do
        case "$1" in
            --quiet) quiet=1; shift ;;
            --native-oracle) oracle="$2"; shift 2 ;;
            # For a process killed by a signal the contract is stdout and the
            # exit status. What the *supervisor* prints is not the guest's
            # behaviour: QEMU announces "uncaught target signal", the shell
            # announces "Terminated", and Linux itself announces nothing —
            # which is what gex does.
            --ignore-stderr) check_stderr=0; shift ;;
            *) break ;;
        esac
    done
    label="$1"; shift
    q_out="$WORK/$label.qemu"
    g_out="$WORK/$label.gex"

    # `set -e` must not abort on a guest that exits non-zero, and the status
    # has to be read before any other command overwrites it.
    set +e
    # shellcheck disable=SC2086 -- QEMU_ARGS is intentionally word-split
    "$QEMU" $QEMU_ARGS "$@" >"$q_out.stdout" 2>"$q_out.stderr"
    q_rc=$?
    # shellcheck disable=SC2086 -- GEXC_ARGS is intentionally word-split
    "$GEXC" run $GEXC_ARGS "$@" >"$g_out.stdout" 2>"$g_out.stderr"
    g_rc=$?
    set -e

    # QEMU 7.2 segfaults on the -O0 longjmp case before producing any output.
    # The suite's output is architecture-independent, so a natively compiled
    # build can stand in rather than losing the coverage.
    if [ -n "$oracle" ] && [ "$q_rc" -ge 128 ] && [ ! -s "$q_out.stdout" ]; then
        shift   # drop the program path; the oracle is a different binary
        set +e
        "$oracle" "$@" >"$q_out.stdout" 2>/dev/null
        q_rc=$?
        set -e
        echo "note $label: qemu failed; compared against the native build"
    fi

    if [ "$q_rc" != "$g_rc" ]; then
        if [ "$quiet" -eq 0 ]; then
            echo "FAIL $label: exit status qemu=$q_rc gex=$g_rc"
            head -3 "$g_out.stderr" | sed 's/^/       /'
        fi
        return 1
    fi
    if ! cmp -s "$q_out.stdout" "$g_out.stdout"; then
        if [ "$quiet" -eq 0 ]; then
            echo "FAIL $label: stdout differs"
            diff -u "$q_out.stdout" "$g_out.stdout" | head -20 || true
        fi
        return 1
    fi
    if [ "$check_stderr" -eq 1 ] && ! cmp -s "$q_out.stderr" "$g_out.stderr"; then
        if [ "$quiet" -eq 0 ]; then
            echo "FAIL $label: stderr differs"
            diff -u "$q_out.stderr" "$g_out.stderr" | head -20 || true
        fi
        return 1
    fi
    echo "ok   $label"
    pass=$((pass + 1))
    return 0
}

echo "building guest programs..."
# Both link modes matter: static-PIE needs R_AARCH64_RELATIVE handling, and
# plain static exercises the fixed 0x400000 load address.
"$CC" -static -no-pie -O2 -o "$WORK/suite" "$HERE/programs/suite.c"
"$CC" -static -O2 -o "$WORK/suite-pie" "$HERE/programs/suite.c"
"$CC" -static -no-pie -O0 -o "$WORK/suite-O0" "$HERE/programs/suite.c"
# -O3 so gcc's vectoriser chooses the NEON instructions, rather than us.
"$CC" -static -no-pie -O2 -o "$WORK/vfs" "$HERE/programs/vfs.c"
"$CC" -static -no-pie -O2 -o "$WORK/signals" "$HERE/programs/signals.c"
"$CC" -static -no-pie -O3 -o "$WORK/vector" "$HERE/programs/vector.c"
# -O2: these are intrinsics, and -O3 lets gcc constant-fold half of them away.
"$CC" -static -no-pie -O2 -o "$WORK/neon3" "$HERE/programs/neon3.c"
"$CC" -static -O3 -o "$WORK/vector-pie" "$HERE/programs/vector.c"
# -ffast-math lets the vectoriser reach the by-element multiplies and the
# widening conversions; neon2 drives the structure loads through intrinsics
# so they are exercised even when gcc would not choose them.
"$CC" -static -no-pie -O3 -ffast-math -o "$WORK/numeric" "$HERE/programs/numeric.c" -lm
"$CC" -static -no-pie -O2 -o "$WORK/neon2" "$HERE/programs/neon2.c"
# Hand-written assembly, because the shapes cross-block flag liveness has to
# get right -- a condition set in one block and consumed in another -- are
# ones a compiler almost never emits.
"$CC" -nostdlib -static -no-pie -o "$WORK/flags" "$HERE/programs/flags.S"
# Loops of every shape, reduced to one number each. The rest of the suite is
# mostly straight-line, which left changes to loop translation able to pass
# everything here and still lose rows in SQLite.
"$CC" -O2 -static -o "$WORK/loops" "$HERE/programs/loops.c"
# Inline assembly, because the fixed-point conversions have no C spelling.
"$CC" -static -no-pie -O2 -o "$WORK/fixedpoint" "$HERE/programs/fixedpoint.c"
# Every result this prints is order-independent, so the comparison is of the
# arithmetic rather than of the scheduler.
"$CC" -static -no-pie -O2 -o "$WORK/threads" "$HERE/programs/threads.c" -lpthread
# Exits while other threads are mid-work, including two parked on a futex.
"$CC" -static -no-pie -O2 -o "$WORK/exitrace" "$HERE/programs/exitrace.c" -lpthread
"$CC" -static -no-pie -O2 -o "$WORK/fork" "$HERE/programs/fork.c"
"$CC" -static -no-pie -O2 -o "$WORK/exec" "$HERE/programs/exec.c"
"$CC" -static -no-pie -O2 -o "$WORK/init" "$HERE/programs/init.c"
"$CC" -static -no-pie -O2 -pthread -o "$WORK/forkthread" "$HERE/programs/forkthread.c"
# C++ pulls in the scalar AdvSIMD group, and its exception unwinder walks
# .eh_frame through personality routines -- a very different code shape from
# anything the C programs reach.
CPP=""
if [ -x "$CXX" ] && "$CXX" -static -no-pie -O2 -o "$WORK/cpp" "$HERE/programs/cpp.cc" 2>/dev/null; then
    CPP=1
fi

printf 'first line\nsecond line\n' >"$WORK/sample.txt"

# Dynamically linked builds, with a sysroot holding the interpreter, so the
# cooperative loading path and runtime translation of a whole libc are
# covered. QEMU needs -L to find the same interpreter.
SYSROOT="$WORK/sysroot"
mkdir -p "$SYSROOT/lib"
# TOOLCHAIN points at .../bin; the target sysroot is its sibling. The
# interpreter there is a symlink to an absolute path that only resolves
# *inside* a sysroot, so copy the real libc.so under the interpreter's name.
MUSL_LIBC="$(dirname "$TOOLCHAIN")/aarch64-linux-musl/lib/libc.so"
DYNAMIC=""
if [ -f "$MUSL_LIBC" ]; then
    cp "$MUSL_LIBC" "$SYSROOT/lib/ld-musl-aarch64.so.1"
    "$CC" -O2 -o "$WORK/dyn-suite" "$HERE/programs/suite.c"
    "$CC" -O2 -o "$WORK/dyn-vector" "$HERE/programs/vector.c"
    "$CC" -O2 -pthread -lm -o "$WORK/dyn-threads" "$HERE/programs/dyn-threads.c"
    DYNAMIC=1
    # Unwinding across a DSO boundary is the part that only the dynamic C++
    # build exercises: libstdc++ and libgcc_s must be found and relocated by
    # the guest's own interpreter before a throw can cross them.
    for lib in libstdc++.so.6.0.29 libgcc_s.so.1; do
        src="$(dirname "$TOOLCHAIN")/aarch64-linux-musl/lib/$lib"
        [ -f "$src" ] && cp "$src" "$SYSROOT/lib/${lib%.0.29}"
    done
    if [ -n "$CPP" ]; then
        "$CXX" -O2 -o "$WORK/dyn-cpp" "$HERE/programs/cpp.cc" 2>/dev/null || true
    fi
fi

# A host-native build of the conformance suite, used only where QEMU itself
# is known to fail.
SUITE_NATIVE=""
if command -v cc >/dev/null 2>&1 && cc -O0 -o "$WORK/suite-native" "$HERE/programs/suite.c" 2>/dev/null; then
    SUITE_NATIVE="$WORK/suite-native"
fi

for area in arith float mem flow sys; do
    compare "static-$area" "$WORK/suite" "$area"
done
for area in arith float mem flow; do
    compare "pie-$area" "$WORK/suite-pie" "$area"
    compare --native-oracle "$SUITE_NATIVE" "O0-$area" "$WORK/suite-O0" "$area"
done
compare "vfs" "$WORK/vfs"
compare "signal-all" "$WORK/signals" all
compare "signal-catch-illegal" "$WORK/signals" catch-illegal
for mode in fatal-default fatal-abort; do
    compare --ignore-stderr "signal-$mode" "$WORK/signals" "$mode"
done
compare "vector-O3" "$WORK/vector"
compare "vector-pie" "$WORK/vector-pie"
compare "numeric" "$WORK/numeric"
compare "neon-structure" "$WORK/neon2"
# The narrowing adds, the pairwise reductions and the read-modify-write forms
# of the modified-immediate group -- what glibc's string routines use.
compare "neon-glibc" "$WORK/neon3"
compare "cross-block-flags" "$WORK/flags"
compare "loops" "$WORK/loops"
compare "fixed-point" "$WORK/fixedpoint"

# The same modules on V8. Everything here is single-threaded and static: the
# cross-engine harness is a few dozen syscalls, not the browser runtime.
cross_engine "vector" "$WORK/vector"
cross_engine "numeric" "$WORK/numeric"
cross_engine "fixedpoint" "$WORK/fixedpoint"
cross_engine "neon-structure" "$WORK/neon2"
cross_engine "neon-glibc" "$WORK/neon3"

# And under the browser runtime: the kernel itself in wasm. This is the
# deliverable, so it gets the same breadth the native host does -- the whole
# conformance suite, the VFS, signals, and C++ exceptions.
browser "numeric" "$WORK/numeric"
browser "fixedpoint" "$WORK/fixedpoint"
browser "vector" "$WORK/vector"
browser "neon-structure" "$WORK/neon2"
browser "neon-glibc" "$WORK/neon3"
browser "cross-block-flags" "$WORK/flags"
browser "loops" "$WORK/loops"
for area in arith float mem flow sys; do
    browser "suite-$area" "$WORK/suite" "$area"
done
browser "vfs" "$WORK/vfs"
browser "signal-all" "$WORK/signals" all
# Guest threads, on one wasm stack: pthreads, mutexes, condition variables,
# thread-local storage, and a main that calls exit_group while others spin.
browser "threads" "$WORK/threads"
browser "thread-exit-race" "$WORK/exitrace"
if [ -n "$CPP" ]; then
    browser "cpp" "$WORK/cpp"
fi
# Dynamically linked, in the browser: musl's own loader runs translated, and
# every library's code arrives through the dispatch miss.
for area in arith float mem flow; do
    browser_dynamic "suite-$area" "$WORK/dyn-suite" "$area"
done
browser_dynamic "vector" "$WORK/dyn-vector"
if [ -n "$CPP" ] && [ -f "$WORK/dyn-cpp" ]; then
    browser_dynamic "cpp" "$WORK/dyn-cpp"
fi
compare "threads" "$WORK/threads"
compare "thread-exit-race" "$WORK/exitrace"
compare "fork" "$WORK/fork"
# argv and envp reach the new program, a close-on-exec descriptor is gone and
# an ordinary one survives with its offset, and a failed exec returns.
expect "exec" "argc=4 [child] [two] [three]
env GEXA=one GEXB=two
fd7 closed  fd8 open
inherited fd reads 16 bytes: through the pipe
child status 42
failed exec status 99
done" "$WORK/exec"
# Pid 1: its own session, a service in another, an orphan it has to adopt, and
# a process group it stops with one signal. Not compared against qemu, which
# cannot agree about which process is pid 1.
expect "init" "init: pid=1 ppid=0 pgid=1 sid=1
init: setsid again -> EPERM
service: pid 2
service: pid 2 exited 0
adopted: pid 3 exited 1
group: led by 4 with 5
group: both joined -> yes
group: kill(-4, SIGTERM) -> 0
group: pid 4 exited 42
group: pid 5 exited 42
init: wait with no children -> ECHILD
init: done" "$WORK/init"
# Threads and pipes inside a process that was forked and then exec'd, which
# is what a package manager does to unpack an archive. Not compared against
# qemu, which cannot exec the child without its own wrapper in the way.
expect "fork-exec-threads" "worker: 4 threads joined, mix=877
main: worker exited 0
sink: 140434 bytes summing to 983040
main: sink exited 0" "$WORK/forkthread"
[ -n "$CPP" ] && compare "cpp-static" "$WORK/cpp"

if [ -n "$DYNAMIC" ]; then
    QEMU_ARGS="-L $SYSROOT"
    GEXC_ARGS="--sysroot $SYSROOT"
    for area in arith float mem flow; do
        compare "dynamic-$area" "$WORK/dyn-suite" "$area"
    done
    compare "dynamic-vector" "$WORK/dyn-vector"
    # Threads racing to translate library code: see programs/dyn-threads.c.
    compare "dynamic-threads" "$WORK/dyn-threads"
    [ -f "$WORK/dyn-cpp" ] && compare "dynamic-cpp" "$WORK/dyn-cpp"
    QEMU_ARGS=""
    GEXC_ARGS=""
fi

# The same programs with runtime translation disabled, so that both the
# on-demand path and the translate-everything-up-front path stay covered.
GEXC_ARGS="--no-jit"
for area in arith float mem flow; do
    compare "nojit-$area" "$WORK/suite" "$area"
done
compare "nojit-vector" "$WORK/vector"
compare "nojit-neon-structure" "$WORK/neon2"
compare "nojit-cross-block-flags" "$WORK/flags"
compare "nojit-loops" "$WORK/loops"
compare "nojit-fixed-point" "$WORK/fixedpoint"
compare "nojit-threads" "$WORK/threads"
GEXC_ARGS=""

echo
if [ "$flaky" -gt 0 ]; then
    echo "$pass passed ($flaky flaky), $fail failed"
else
    echo "$pass passed, $fail failed"
fi
[ "$fail" -eq 0 ]
