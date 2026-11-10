#!/bin/sh
# A container linked without GEXCC_BAKE_RUNNER, or with it empty, names no
# runner and no path of the gexcc install (what is built may go into an
# image): on the build machine it still runs, through GEXCC_RUNNER or the
# install's gexcc-exec on PATH, and says so (127) when there is neither. One
# linked with GEXCC_BAKE_RUNNER=PATH names PATH. A C++ program (libc++abi's
# assert()s, the runtime, the sysroot's headers) names no install path
# either way.
#
#   test/fixtures/gexcc/dl/bakerunner.sh DIR   (gexcc on PATH; DIR: where gex can see)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
export GEX_LOG=${GEX_LOG:-off}
B=$(cd "$(dirname "$(command -v gexcc)")" && pwd)
P=$(dirname "$B")
RUNNER=$(cat "$P/lib/gexcc/runner")
W=$(mktemp -d "${1:?DIR}/bakerunner.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/p.c" <<'X'
#include <stdio.h>
int main(int argc, char **argv) { printf("ran %s %d\n", argc > 1 ? argv[1] : "-", argc); return 3; }
X
cat > "$W/q.cpp" <<'X'
#include <cassert>
#include <cstdio>
#include <stdexcept>
#include <string>
int main(int argc, char **)
{
    std::string s(argc + 2, 'x');
    assert(s.size() == 3);
    try { throw std::runtime_error(s); } catch (const std::exception &e) { std::printf("caught %s\n", e.what()); }
    return 0;
}
X
fail=0
ok() { echo "ok   bakerunner $1"; }
bad() { echo "FAIL bakerunner $1"; fail=1; }
names() { LC_ALL=C grep -caF -- "$2" "$1"; }
# the install's own paths: its prefix and the runner it names (which a copied
# install can have elsewhere)
local_paths() { # file -> how many of them it names
    n=0; for x in "$P/" "$(realpath "$P")/" "$RUNNER"; do n=$((n + $(names "$1" "$x"))); done; echo $n; }
# where libc++abi's sources were: their assert()s name them, under /llvm
cxxabi_paths() { LC_ALL=C grep -aoE '/[A-Za-z0-9._/+-]*libcxxabi/src/' "$1" | grep -v '^/*llvm/' | sort -u; }
GEXCC_BAKE_RUNNER= gexcc -w -o "$W/none" "$W/p.c" || { echo "FAIL build none"; exit 1; }
GEXCC_BAKE_RUNNER=/opt/elsewhere/gexcc-exec gexcc -w -o "$W/other" "$W/p.c" || { echo "FAIL build other"; exit 1; }
env -u GEXCC_BAKE_RUNNER gexcc -w -o "$W/default" "$W/p.c" || { echo "FAIL build default"; exit 1; }
GEXCC_BAKE_RUNNER= gexc++ -w -o "$W/cxx" "$W/q.cpp" || { echo "FAIL build cxx"; exit 1; }
[ "$(local_paths "$W/none")" = 0 ] && ok "empty: names no install path" || bad "empty: names $P or $RUNNER"
[ "$(names "$W/other" /opt/elsewhere/gexcc-exec)" != 0 ] && [ "$(local_paths "$W/other")" = 0 ] &&
    ok "a path: names it" || bad "a path: not baked as given"
[ "$(local_paths "$W/default")" = 0 ] && ok "unset: names no install path" || bad "unset: names $P or $RUNNER"
[ "$(local_paths "$W/cxx")" = 0 ] && ok "c++: names no install path" || bad "c++: names $P or $RUNNER"
# libc++abi's own names come from how build-cxx.sh built it: an install whose
# libc++abi.a predates the map is said, not failed (it is the install's)
if [ -n "$(cxxabi_paths "$P/sysroot/lib/libc++abi.a")" ]; then
    echo "skip bakerunner c++: this install's libc++abi.a names its build tree (built before build-cxx.sh mapped it)"
elif [ -z "$(cxxabi_paths "$W/cxx")" ]; then ok "c++: libc++abi's sources as /llvm"
else bad "c++: names libc++abi's build tree"; cxxabi_paths "$W/cxx" | head -3; fi
want="ran a 2"
got=$(GEXCC_RUNNER=$RUNNER "$W/none" a 2>&1); s=$?
[ "$got" = "$want" ] && [ $s = 3 ] && ok "empty: runs through GEXCC_RUNNER" || bad "empty: GEXCC_RUNNER run gave '$got' (exit $s)"
got=$(env -u GEXCC_RUNNER PATH="$B:/usr/bin:/bin" "$W/default" a 2>&1); s=$?
[ "$got" = "$want" ] && [ $s = 3 ] && ok "unset: runs through gexcc-exec on PATH" || bad "unset: gave '$got' (exit $s)"
got=$(env -u GEXCC_RUNNER PATH=/nonexistent "$W/none" a 2>&1); s=$?
[ $s = 127 ] && ok "empty, no runner anywhere: says so (127)" || bad "empty, no runner: exit $s ($got)"
got=$(GEXCC_RUNNER=$RUNNER "$W/cxx" 2>&1); s=$?
[ "$got" = "caught xxx" ] && [ $s = 0 ] && ok "c++: runs" || bad "c++: gave '$got' (exit $s)"
[ $fail = 0 ] && echo "bakerunner: all ok"
exit $fail
