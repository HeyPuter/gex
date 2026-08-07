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
# A host device has to survive an `execve`.
#
# The compositor and the virtio-gpu renderer each bind a name in the guest's
# socket namespace, and `execve` re-enters the function that created them.
# Building a second one rebinds the name: a client that connects afterwards is
# accepted by the new device while the process that would service it holds the
# old, and the guest waits forever for a reply nobody will write. A shell's
# fork-then-exec hits it every time.
#
# QEMU is no oracle -- there is no renderer behind it -- so what is checked is
# that both the parent and the program it exec'd got an answer.
virgl_exec() {
    label="$1"; prog="$2"
    out="$WORK/$label"
    if ! "$GEXC" run --virgl /tmp/.gex-test-virgl "$prog" >"$out.stdout" 2>"$out.stderr"; then
        echo "skip $label: no host renderer"
        return
    fi
    if grep -q "no host renderer" "$out.stderr"; then
        echo "skip $label: no host renderer"
        return
    fi
    if grep -q "^parent: read 8" "$out.stdout" &&
        grep -q "^exec.d child: read 8" "$out.stdout"; then
        echo "ok   $label"
        pass=$((pass + 1))
    else
        echo "FAIL $label: a client was accepted but never serviced"
        sed 's/^/     /' "$out.stdout"
        fail=$((fail + 1))
    fi
}

# Run a Wayland client against gex's own compositor and check the pixels.
#
# QEMU is no oracle here: there is no compositor on the other side of it, so
# the client would find nothing to connect to. What is checked instead is that
# the client got all the way through the protocol -- registry, bind, surface,
# toplevel, shm buffer, attach, commit, frame callback -- and that the pixels
# the host read back out of the guest's memory are the ones the client drew.
#
# Offsets rather than a decoder: the header is a fixed 13 bytes for this size,
# and a pixel is three bytes at 13 + (y * 64 + x) * 3.
wl_pixel() {
    od -An -tu1 -j "$2" -N 3 "$1" | tr -s ' ' | sed 's/^ //;s/ $//'
}

wayland() {
    label="$1"; prog="$2"
    out="$WORK/$label"
    rm -f "$out"-*.ppm
    set +e
    "$GEXC" run --wayland /run/user/0/wayland-0 --wayland-size 64x48 \
        --wayland-out "$out" "$prog" >"$out.stdout" 2>"$out.stderr"
    rc=$?
    set -e
    if [ "$rc" != 0 ]; then
        echo "FAIL $label: exit status $rc"
        head -3 "$out.stderr" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    if ! grep -q "^frame callback yes$" "$out.stdout"; then
        echo "FAIL $label: the client never got its frame callback"
        head -10 "$out.stdout" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    if [ ! -f "$out-000.ppm" ]; then
        echo "FAIL $label: the compositor produced no frame"
        fail=$((fail + 1))
        return
    fi
    # The client draws a green border, a blue field and a red diagonal.
    got="$(wl_pixel "$out-000.ppm" 103)|$(wl_pixel "$out-000.ppm" 3943)|$(wl_pixel "$out-000.ppm" 4717)"
    want="0 200 0|0 0 200|200 0 0"
    if [ "$got" != "$want" ]; then
        echo "FAIL $label: pixels are [$got], expected [$want]"
        fail=$((fail + 1))
        return
    fi
    echo "ok   $label"
    pass=$((pass + 1))
}

# The GPU compositor must produce exactly what the CPU one does.
#
# With the screen the same size as the window there is nowhere for a surface
# to be placed but the origin, so the two paths have to agree byte for byte:
# same pixels, same swizzle, same everything. That is a stronger check than
# any list of sample points, and it is why the sizes match here rather than
# testing placement -- placement is covered by the unit tests in gex-gpu,
# which can ask for a screen bigger than the window.
#
# A machine with no graphics adapter falls back to the CPU and the comparison
# becomes vacuous, so the run is required to say which adapter it got.
wayland_gpu() {
    label="$1"; prog="$2"
    out="$WORK/$label"
    rm -f "$out"-*.ppm
    set +e
    "$GEXC" run --verbose --wayland /run/user/0/wayland-0 --wayland-size 64x48 \
        --wayland-gpu --wayland-out "$out" "$prog" >"$out.stdout" 2>"$out.stderr"
    rc=$?
    set -e
    if [ "$rc" != 0 ]; then
        echo "FAIL $label: exit status $rc"
        head -3 "$out.stderr" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    if ! grep -q "compositing on " "$out.stderr"; then
        echo "skip $label: no graphics adapter"
        grep "no GPU compositor" "$out.stderr" | head -1 | sed 's/^/       /' || true
        return
    fi
    if [ ! -f "$out-000.ppm" ]; then
        echo "FAIL $label: the compositor produced no frame"
        fail=$((fail + 1))
        return
    fi
    if ! cmp -s "$WORK/wayland-000.ppm" "$out-000.ppm"; then
        echo "FAIL $label: the GPU and CPU screens differ"
        fail=$((fail + 1))
        return
    fi
    echo "ok   $label ($(sed -n 's/.*compositing on //p' "$out.stderr" | head -1))"
    pass=$((pass + 1))
}

# An unmodified OpenGL ES program, run against the host's GL.
#
# QEMU is no oracle: there is no graphics device behind it. What is checked is
# that a program which includes the standard headers and links against
# libEGL/libGLESv2 by their usual sonames gets a real context, compiles its own
# GLSL, draws, and reads back the pixels it drew -- with the guest's GL library
# being a marshaller and the host's driver doing the work.
#
# The version string is the host's and varies, so it is only required to be
# non-empty. The pixels are the test.
gles() {
    label="$1"; prog="$2"; root="$3"
    out="$WORK/$label"
    set +e
    "$GEXC" run --gl --sysroot "$root" --env LD_LIBRARY_PATH=/usr/lib \
        "$prog" >"$out.stdout" 2>"$out.stderr"
    rc=$?
    set -e
    if grep -q "^EGL " "$out.stdout" && ! grep -q "^GL_VERSION .\+" "$out.stdout"; then
        echo "skip $label: the host has no GL"
        return
    fi
    if [ "$rc" != 0 ]; then
        echo "FAIL $label: exit status $rc"
        head -3 "$out.stderr" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    got=$(grep -E "^(gl error|inside|outside)" "$out.stdout" | tr '\n' '|')
    want="gl error 0|inside  0 255 0|outside 0 0 255|"
    if [ "$got" != "$want" ]; then
        echo "FAIL $label: got [$got], expected [$want]"
        head -8 "$out.stdout" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    echo "ok   $label ($(sed -n 's/^GL_VERSION //p' "$out.stdout" | head -1))"
    pass=$((pass + 1))
}

# The gears, on the host's GL: real geometry, real shaders, real per-frame
# uniforms. Checked by how much of the frame they cover, which catches a
# transform that smears them across it or a depth test that loses them, and by
# GL having reported no error over the whole run.
gears() {
    label="$1"; prog="$2"; root="$3"
    out="$WORK/$label"
    set +e
    "$GEXC" run --gl --sysroot "$root" --env LD_LIBRARY_PATH=/usr/lib \
        "$prog" 30 /gears.ppm >"$out.stdout" 2>"$out.stderr"
    rc=$?
    set -e
    if ! grep -q "^renderer .\+" "$out.stdout"; then
        echo "skip $label: the host has no GL"
        return
    fi
    if [ "$rc" != 0 ]; then
        echo "FAIL $label: exit status $rc"
        head -3 "$out.stderr" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    covered=$(sed -n 's/^covered \([0-9]*\)%$/\1/p' "$out.stdout")
    errors=$(sed -n 's/^gl error //p' "$out.stdout")
    if [ "$errors" != "0" ]; then
        echo "FAIL $label: GL reported error $errors"
        fail=$((fail + 1))
        return
    fi
    if [ -z "$covered" ] || [ "$covered" -lt 5 ] || [ "$covered" -gt 60 ]; then
        echo "FAIL $label: the gears covered ${covered:-?}% of the frame"
        head -8 "$out.stdout" | sed 's/^/       /'
        fail=$((fail + 1))
        return
    fi
    echo "ok   $label (${covered}% covered, $(sed -n 's/^triangles //p' "$out.stdout") triangles)"
    pass=$((pass + 1))
}

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
"$CC" -static -no-pie -O2 -o "$WORK/neon-varshift" "$HERE/programs/neon-varshift.c"
"$CC" -static -no-pie -O2 -o "$WORK/neon-move" "$HERE/programs/neon-move.c"
"$CC" -static -O2 -o "$WORK/itimer" "$HERE/programs/itimer.c"
"$CC" -static -O2 -o "$WORK/unixsock" "$HERE/programs/unixsock.c"
"$CC" -static -O2 -o "$WORK/hostfacilities" "$HERE/programs/hostfacilities.c"
"$CC" -static -O2 -o "$WORK/wlclient" "$HERE/programs/wlclient.c"
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
"$CC" -O2 -static -o "$WORK/termios-size" "$HERE/programs/termios-size.c"
"$CC" -O2 -static -o "$WORK/neon-cmp64" "$HERE/programs/neon-cmp64.c"
"$CC" -O2 -static -o "$WORK/neon-lane-stride" "$HERE/programs/neon-lane-stride.c"
"$CC" -O2 -static -o "$WORK/neon-fixedconv" "$HERE/programs/neon-fixedconv.c"
"$CC" -O2 -static -o "$WORK/neon-interleave" "$HERE/programs/neon-interleave.c"
"$CC" -O2 -static -o "$WORK/neon-clz" "$HERE/programs/neon-clz.c"
"$CC" -O2 -static -o "$WORK/neon-pairwise" "$HERE/programs/neon-pairwise.c"
"$CC" -O2 -static -o "$WORK/fp-literal" "$HERE/programs/fp-literal.c"
"$CC" -O2 -static -o "$WORK/unixsock-visible" "$HERE/programs/unixsock-visible.c"
"$CC" -O2 -static -o "$WORK/virgl-exec" "$HERE/programs/virgl-exec.c"
"$CC" -O2 -static -o "$WORK/iov-partial" "$HERE/programs/iov-partial.c"
"$CC" -O2 -static -o "$WORK/signal-kill" "$HERE/programs/signal-kill.c"
"$CC" -O2 -static -o "$WORK/signal-alarm" "$HERE/programs/signal-alarm.c"
"$CC" -O2 -static -o "$WORK/signal-sigchld" "$HERE/programs/signal-sigchld.c"
# Inline assembly, because the fixed-point conversions have no C spelling.
"$CC" -static -no-pie -O2 -o "$WORK/fixedpoint" "$HERE/programs/fixedpoint.c"
# Every result this prints is order-independent, so the comparison is of the
# arithmetic rather than of the scheduler.
"$CC" -static -no-pie -O2 -o "$WORK/threads" "$HERE/programs/threads.c" -lpthread
# Exits while other threads are mid-work, including two parked on a futex.
"$CC" -static -no-pie -O2 -o "$WORK/exitrace" "$HERE/programs/exitrace.c" -lpthread
"$CC" -static -no-pie -O2 -o "$WORK/fork" "$HERE/programs/fork.c"
"$CC" -static -no-pie -O2 -o "$WORK/exec" "$HERE/programs/exec.c"
"$CC" -static -no-pie -O2 -o "$WORK/brk-zero" "$HERE/programs/brk-zero.c"
"$CC" -static -O2 -o "$WORK/selfmod" "$HERE/programs/selfmod.c"
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
compare "neon-varshift" "$WORK/neon-varshift"
compare "neon-move" "$WORK/neon-move"
# Interval timers: see programs/itimer.c.
compare "itimer" "$WORK/itimer"
# Unix sockets and descriptor passing: see programs/unixsock.c.
compare "unixsock" "$WORK/unixsock"
# Shebang scripts, flock and epoll. Not compared against QEMU: the flock and
# epoll halves agree with it exactly, but qemu-aarch64 can only re-exec itself
# for the program it was given, so every `execve` of a script fails there with
# ENOEXEC and succeeds here. The expectation is what Linux does.
expect "hostfacilities" "$(cat <<'HFEOF'
interp arg1 [--interp]
interp got: [shebang1] [extra]
script 1 exited 4
interp arg1 [--interp with two words]
interp got: [shebang2] [extra]
script 2 exited 4
exec 3 failed: ENOEXEC
script 3 exited 9
exclusive: 0
second on the same file: -1 EWOULDBLOCK
a different file: 0
through a dup: 0
unlock: 0
after unlock: 0
shared then shared: 0 0
add: 0
add twice: -1 EEXIST
nothing ready: 0
after a write: 1 events=EPOLLIN data=feedface
mod: 0
after mod: 1 data=1234
del: 0
del twice: -1 ENOENT
after del: 0
HFEOF
)" "$WORK/hostfacilities"
# The compositor, end to end: see programs/wlclient.c.
wayland "wayland" "$WORK/wlclient"
# And the same again on the GPU, which must agree exactly.
wayland_gpu "wayland-gpu" "$WORK/wlclient"

# An unmodified GLES2 program on the host's GL: see programs/gles-triangle.c.
GLROOT="$WORK/glroot"
if "$HERE/../../guest/gl/build.sh" "$WORK/glout" >/dev/null 2>&1 &&
   [ -f "$WORK/glout/libGLESv2.so.2" ] && [ -d /usr/include/GLES2 ]; then
    rm -rf "$GLROOT"
    mkdir -p "$GLROOT/lib" "$GLROOT/usr/lib" "$WORK/glinc"
    cp "$MUSL_LIBC" "$GLROOT/lib/ld-musl-aarch64.so.1"
    cp "$WORK/glout/libGLESv2.so.2" "$WORK/glout/libEGL.so.1" "$GLROOT/usr/lib/"
    # The GL headers have no architecture in them, so the host's serve for a
    # cross build; copied out so the host's libc headers are not dragged in.
    cp -r /usr/include/EGL /usr/include/GLES2 /usr/include/KHR "$WORK/glinc/" 2>/dev/null || true
    if "$CC" -O2 -o "$GLROOT/triangle" "$HERE/programs/gles-triangle.c" \
        -I "$WORK/glinc" -L "$WORK/glout" -lEGL -lGLESv2 2>/dev/null; then
        gles "gles-triangle" "$GLROOT/triangle" "$GLROOT"
        if "$CC" -O2 -o "$GLROOT/gears" "$HERE/programs/gles-gears.c" \
            -I "$WORK/glinc" -L "$WORK/glout" -lEGL -lGLESv2 -lm 2>/dev/null; then
            gears "gles-gears" "$GLROOT/gears" "$GLROOT"
        fi
    else
        echo "skip gles-triangle: could not build it"
    fi
else
    echo "skip gles-triangle: no guest GL driver"
fi
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
cross_engine "neon-varshift" "$WORK/neon-varshift"
cross_engine "neon-move" "$WORK/neon-move"

# And under the browser runtime: the kernel itself in wasm. This is the
# deliverable, so it gets the same breadth the native host does -- the whole
# conformance suite, the VFS, signals, and C++ exceptions.
browser "numeric" "$WORK/numeric"
browser "fixedpoint" "$WORK/fixedpoint"
browser "vector" "$WORK/vector"
browser "neon-structure" "$WORK/neon2"
browser "neon-glibc" "$WORK/neon3"
browser "neon-varshift" "$WORK/neon-varshift"
browser "neon-move" "$WORK/neon-move"
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
compare "neon-cmp64" "$WORK/neon-cmp64"
compare "neon-lane-stride" "$WORK/neon-lane-stride"
compare "neon-fixedconv" "$WORK/neon-fixedconv"
compare "neon-interleave" "$WORK/neon-interleave"
compare "neon-clz" "$WORK/neon-clz"
compare "neon-pairwise" "$WORK/neon-pairwise"
compare "fp-literal" "$WORK/fp-literal"
compare "unixsock-visible" "$WORK/unixsock-visible"
compare "iov-partial" "$WORK/iov-partial"
# What `brk` moves over must read as zero. Lowering the break and raising it
# again is the case that shows it: growing into never-touched memory is clean
# by accident, because a fresh wasm memory is zero to begin with.
compare "brk-zero" "$WORK/brk-zero"
# A guest that writes its own machine code -- a JVM, a JavaScript engine, a
# regex compiler -- and a plugin host that maps a different library over an
# address it has already run code at. A dispatch slot answers for an address,
# not for the bytes at it, so without invalidation the guest calls its new
# function and runs the old one, silently and with no fault anywhere. Every
# round returns a different constant so a stale translation reads as the
# previous answer.
compare "selfmod" "$WORK/selfmod"
compare "signal-kill" "$WORK/signal-kill"
compare "signal-alarm" "$WORK/signal-alarm"
compare "signal-sigchld" "$WORK/signal-sigchld"
virgl_exec "virgl-exec" "$WORK/virgl-exec"
compare "nojit-vector" "$WORK/vector"
compare "nojit-neon-structure" "$WORK/neon2"
compare "nojit-cross-block-flags" "$WORK/flags"
compare "nojit-loops" "$WORK/loops"
compare "nojit-fixed-point" "$WORK/fixedpoint"
compare "nojit-threads" "$WORK/threads"
GEXC_ARGS=""

# How much of `struct termios` a TCGETS writes. Needs a terminal on both
# sides, so both run under `script` -- without one the program has nothing to
# ask and says so, and a vacuous pass is worse than no test. See
# programs/termios-size.c for why the size is the whole point.
if command -v script >/dev/null 2>&1; then
    if [ -x "$WORK/termios-size" ]; then
        g="$(script -qec "$GEXC run $WORK/termios-size" /dev/null 2>&1 | tr -d '\r')"
        q="$(script -qec "$QEMU $WORK/termios-size" /dev/null 2>&1 | tr -d '\r')"
        if [ "$g" = "$q" ] && [ "${g#*clobbered 0}" != "$g" ]; then
            echo "ok   termios-size"
            pass=$((pass + 1))
        else
            echo "FAIL termios-size: gex and QEMU disagree, or the guard was overwritten"
            echo "     gex:  $g"
            echo "     qemu: $q"
            fail=$((fail + 1))
        fi
    fi
else
    echo "skip termios-size: no script(1) to make a terminal with"
fi

# Debian's gcc compiling and running a program, inside the guest. The most
# demanding thing gex runs, and it needs a 257 MB root filesystem that is not
# built by default, so it skips when that is absent. See the script's header.
if out=$("$HERE/../gcc-in-guest.sh" 2>&1); then
    echo "$out" | grep -E "^(ok|skip)" || true
    echo "$out" | grep -q "^ok" && pass=$((pass + 1))
else
    echo "$out" | sed 's/^/     /'
    fail=$((fail + 1))
fi

# The same toolchain at the scale real software is written at: SQLite's 9 MB
# amalgamation, compiled and then run. Minutes rather than seconds, so it is
# opt-in -- but it is the case that found the per-store instance ceiling, and
# a ceiling is exactly the kind of bug a small test cannot reach.
if [ "${GEX_SLOW:-0}" = "1" ]; then
    if out=$("$HERE/../sqlite-build-in-guest.sh" 2>&1); then
        echo "$out" | grep -E "^(ok|skip)" || true
        echo "$out" | grep -q "^ok" && pass=$((pass + 1))
    else
        echo "$out" | sed 's/^/     /'
        fail=$((fail + 1))
    fi
else
    echo "skip sqlite-build-in-guest: set GEX_SLOW=1 (it takes minutes)"
fi

# The browser shim still compiles to wasm.
#
# Not a behaviour test -- it is a build. It is here because the browser is the
# only target that matters and it is the one nothing else in this file
# touches: the terminal test below runs against `web/public`, which holds
# whatever the last `build-assets.sh` produced, so the shim can stop compiling
# entirely and every test still passes. It did. A `BlockOp` variant was added
# for `rt_sigsuspend`, the native shim learned it, and the browser shim was
# left with two non-exhaustive matches for however long it took someone to
# rebuild the page.
# virglrenderer, compiled to wasm, coming up on a WebGL 2 context in a real
# browser. Not a translation test at all -- it is the other half of the
# graphics path: the native runtime gives the guest a GPU by handing VirGL to
# the host's libvirglrenderer, and a page has no host to hand it to, so the
# renderer itself is what crosses. Skips without the wasm build or a browser.
if [ -n "$NODE" ]; then
    if out=$("$NODE" "$HERE/../../web/test/virgl-webgl.mjs" 2>&1); then
        echo "$out" | grep -E "^(ok|skip|     )" || true
        echo "$out" | grep -q "^ok" && pass=$((pass + 1))
    else
        echo "$out" | sed 's/^/     /' | head -8
        fail=$((fail + 1))
    fi
else
    echo "skip virgl-webgl: no node"
fi

# Debian's own glxinfo and glxgears, on the host's real OpenGL, through the
# guest's own unmodified Mesa. Minutes on a cold cache -- it translates all of
# Mesa and LLVM -- so it is opt-in with the rest of the slow ones.
if [ "${GEX_SLOW:-0}" = "1" ]; then
    if out=$("$HERE/../opengl-in-guest.sh" 2>&1); then
        echo "$out" | grep -E "^(ok|skip|     )" || true
        echo "$out" | grep -q "^ok" && pass=$((pass + 1))
    else
        echo "$out" | sed 's/^/     /'
        fail=$((fail + 1))
    fi
else
    echo "skip opengl-in-guest: set GEX_SLOW=1 (it takes minutes)"
fi

#
# `cargo check`, not `cargo build`: a plain `cargo build` for wasm32 writes
# `gex_browser.wasm` *without* the link flags in `shims/browser/build.sh`, and
# a shim linked without `--export=__stack_pointer` loads and then dies on the
# first call. Checking never produces an artifact, so it cannot clobber the
# one the browser tests above just ran against.
if rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown; then
    if cargo check --quiet --release --target wasm32-unknown-unknown \
        -p gex-browser >"$WORK/browser-check.out" 2>&1; then
        echo "ok   browser-builds"
        pass=$((pass + 1))
    else
        echo "FAIL browser-builds: the browser shim does not compile to wasm"
        grep -E "^error" -A 6 "$WORK/browser-check.out" | head -20 | sed 's/^/     /'
        fail=$((fail + 1))
    fi
else
    echo "skip browser-builds: no wasm32-unknown-unknown target installed"
fi

# The demo page's terminal. Not a translation test: it checks the seam above
# the runtime, where a keystroke has to travel from an xterm through the line
# discipline into a parked guest read. That seam broke once without any test
# noticing, because a terminal that ignores input looks exactly like a program
# that is busy.
if [ -n "$NODE" ] && [ -f web/public/rootfs/busybox.tar ] \
    && [ -f web/public/gex/gex_browser.wasm ]; then
    if "$NODE" web/test/terminal-input.mjs >"$WORK/terminal-input.out" 2>&1; then
        sed -n 's/^ok   /ok   terminal-/p' "$WORK/terminal-input.out"
        pass=$((pass + 1))
    else
        echo "FAIL terminal-input"
        sed 's/^/     /' "$WORK/terminal-input.out"
        fail=$((fail + 1))
    fi
else
    echo "skip terminal-input: no node, or the web assets are not built"
fi

echo
if [ "$flaky" -gt 0 ]; then
    echo "$pass passed ($flaky flaky), $fail failed"
else
    echo "$pass passed, $fail failed"
fi
[ "$fail" -eq 0 ]
