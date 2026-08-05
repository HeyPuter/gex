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

# Compare one invocation between the two engines.
#   compare <label> <program> [args...]
compare() {
    label="$1"; shift
    q_out="$WORK/$label.qemu"
    g_out="$WORK/$label.gex"

    # `set -e` must not abort on a guest that exits non-zero, and the status
    # has to be read before any other command overwrites it.
    set +e
    "$QEMU" "$@" >"$q_out.stdout" 2>"$q_out.stderr"
    q_rc=$?
    "$GEXC" run "$@" >"$g_out.stdout" 2>"$g_out.stderr"
    g_rc=$?
    set -e

    # The oracle is not infallible: QEMU 7.2 segfaults on the -O0 longjmp
    # case before producing any output. The suite is written so its output is
    # architecture-independent, which lets a natively compiled build stand in
    # as a second oracle rather than losing the coverage.
    if [ "$q_rc" -ge 128 ] && [ ! -s "$q_out.stdout" ] && [ -n "$NATIVE" ]; then
        set +e
        "$NATIVE" "$@" >"$q_out.stdout" 2>/dev/null
        q_rc=$?
        set -e
        # Drop the program path, which differs between the two builds.
        shift
        echo "note $label: qemu failed; comparing against the native build"
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
    if ! cmp -s "$q_out.stderr" "$g_out.stderr"; then
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
"$CC" -static -no-pie -O3 -o "$WORK/vector" "$HERE/programs/vector.c"
"$CC" -static -O3 -o "$WORK/vector-pie" "$HERE/programs/vector.c"

printf 'first line\nsecond line\n' >"$WORK/sample.txt"

# A host-native build of the same source, used only when QEMU itself fails.
NATIVE=""
if command -v cc >/dev/null 2>&1 && cc -O0 -o "$WORK/suite-native" "$HERE/programs/suite.c" 2>/dev/null; then
    NATIVE="$WORK/suite-native"
fi

for area in arith float mem flow sys; do
    compare "static-$area" "$WORK/suite" "$area"
done
for area in arith float mem flow; do
    compare "pie-$area" "$WORK/suite-pie" "$area"
    compare "O0-$area" "$WORK/suite-O0" "$area"
done
compare "vfs" "$WORK/vfs"
compare "vector-O3" "$WORK/vector"
compare "vector-pie" "$WORK/vector-pie"

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
