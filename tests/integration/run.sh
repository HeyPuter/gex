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
# Inline assembly, because the fixed-point conversions have no C spelling.
"$CC" -static -no-pie -O2 -o "$WORK/fixedpoint" "$HERE/programs/fixedpoint.c"
# Every result this prints is order-independent, so the comparison is of the
# arithmetic rather than of the scheduler.
"$CC" -static -no-pie -O2 -o "$WORK/threads" "$HERE/programs/threads.c" -lpthread
# Exits while other threads are mid-work, including two parked on a futex.
"$CC" -static -no-pie -O2 -o "$WORK/exitrace" "$HERE/programs/exitrace.c" -lpthread
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
compare "cross-block-flags" "$WORK/flags"
compare "fixed-point" "$WORK/fixedpoint"
compare "threads" "$WORK/threads"
compare "thread-exit-race" "$WORK/exitrace"
[ -n "$CPP" ] && compare "cpp-static" "$WORK/cpp"

if [ -n "$DYNAMIC" ]; then
    QEMU_ARGS="-L $SYSROOT"
    GEXC_ARGS="--sysroot $SYSROOT"
    for area in arith float mem flow; do
        compare "dynamic-$area" "$WORK/dyn-suite" "$area"
    done
    compare "dynamic-vector" "$WORK/dyn-vector"
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
