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
compare() {
    oracle=""
    check_stderr=1
    while :; do
        case "$1" in
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
        echo "FAIL $label: exit status qemu=$q_rc gex=$g_rc"
        head -3 "$g_out.stderr" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    if ! cmp -s "$q_out.stdout" "$g_out.stdout"; then
        echo "FAIL $label: stdout differs"
        diff -u "$q_out.stdout" "$g_out.stdout" | head -20 || true
        fail=$((fail + 1))
        return
    fi
    if [ "$check_stderr" -eq 1 ] && ! cmp -s "$q_out.stderr" "$g_out.stderr"; then
        echo "FAIL $label: stderr differs"
        diff -u "$q_out.stderr" "$g_out.stderr" | head -20 || true
        fail=$((fail + 1))
        return
    fi
    echo "ok   $label"
    pass=$((pass + 1))
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

if [ -n "$DYNAMIC" ]; then
    QEMU_ARGS="-L $SYSROOT"
    GEXC_ARGS="--sysroot $SYSROOT"
    for area in arith float mem flow; do
        compare "dynamic-$area" "$WORK/dyn-suite" "$area"
    done
    compare "dynamic-vector" "$WORK/dyn-vector"
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
GEXC_ARGS=""

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
