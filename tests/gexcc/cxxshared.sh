#!/bin/sh
# C++ exceptions across shared modules: a library and a program, each with
# its own libc++ and libunwind, throw and catch through each other (one
# exception tag for the process, libc.so's), and each catches its own.
#
#   tests/gexcc/cxxshared.sh DIR    (gexcc on PATH; DIR: where gex can see,
#                                    not /dev/shm)
set -u
command -v gexc++ > /dev/null || { echo "gexc++ not on PATH (tools/gexcc/install.sh)"; exit 2; }
W=$(mktemp -d "${1:?DIR}/cxxshared.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/l.cpp" <<'X'
#include <stdexcept>
void thrower() { throw std::runtime_error("from the library"); }
int catcher(void (*f)()) { try { f(); } catch (const std::exception &) { return 4; } return 0; }
X
cat > "$W/m.cpp" <<'X'
#include <stdexcept>
#include <cstdio>
void thrower(); int catcher(void (*)());
static void own() { throw std::logic_error("from the program"); }
int main() {
    int r = 0;
    try { throw 1; } catch (int) { r |= 1; }
    try { thrower(); } catch (const std::exception &e) { r |= 2; }
    r |= catcher(own);
    std::printf("%d\n", r);
    return r != 7;
}
X
GEXCC_SHARED=1 gexc++ -fPIC -shared -o "$W/libl.so" "$W/l.cpp" &&
    GEXCC_SHARED=1 gexc++ -o "$W/m" "$W/m.cpp" -L"$W" -ll -Wl,-rpath,"$W" || { echo "FAIL build"; exit 1; }
out=$(gexcc-exec "$W/m" 2>&1 | tail -1)
if [ "$out" = 7 ]; then echo "ok cxxshared"; else echo "FAIL cxxshared: $out"; exit 1; fi
