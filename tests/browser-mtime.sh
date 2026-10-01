#!/bin/sh
# File times under the browser runtime, both of its filesystems:
#
#   - the single-Worker shim's own tmpfs (`Gex` on its own, as an ejected
#     project's run.mjs and the bench harness use it), run with the syscall
#     trace on and every block translated at run time -- the combination that
#     re-entered the shim's kernel and panicked ("the kernel was re-entered");
#   - the global kernel's, through `RemoteFs` (`run-node.mjs`, ProcessHost).
#
# A bundled file must report the host's mtime through both `stat` and `fstat`
# (gzip records it from `fstat`), a file the guest writes must report the time
# it was written, and `utimensat` must stick.
#
#   tests/browser-mtime.sh [a64|x86]
set -eu
ARCH="${1:-a64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-mtime-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}"; INFIX="" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; INFIX=".x86"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86]" >&2; exit 2 ;;
esac
for need in "$GEXC" "$ROOT/web/public/gex/gex_browser$INFIX.wasm" "$ROOT/web/public/gex/gex_kernel$INFIX.plain.wasm"; do
    if [ ! -e "$need" ]; then
        echo "skip: $need is not built" >&2
        exit 0
    fi
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/times.c" <<'EOF'
#include <fcntl.h>
#include <stdio.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
int main(int argc, char **argv) {
    struct stat a, b, c;
    if (stat(argv[1], &a) != 0) { perror("stat"); return 1; }
    int fd = open(argv[1], O_RDONLY);
    if (fd < 0 || fstat(fd, &b) != 0) { perror("fstat"); return 1; }
    printf("bundled %lld %lld\n", (long long)a.st_mtime, (long long)b.st_mtime);
    time_t before = time(0);
    int w = open("/tmp/new", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (w < 0 || write(w, "x", 1) != 1 || fstat(w, &c) != 0) { perror("write"); return 1; }
    printf("written %s\n", c.st_mtime >= before - 2 && c.st_mtime <= time(0) + 2 ? "now" : "WRONG");
    struct timespec t[2] = {{0, UTIME_OMIT}, {1234567890, 0}};
    if (utimensat(AT_FDCWD, "/tmp/new", t, 0) != 0 || stat("/tmp/new", &c) != 0) { perror("utimensat"); return 1; }
    printf("set %lld\n", (long long)c.st_mtime);
    return 0;
}
EOF
zig cc -target "$TARGET" -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/times" "$WORK/times.c"
echo data > "$WORK/data"
touch -d '2021-06-15 12:00:00 UTC' "$WORK/data"
t=$(stat -c %Y "$WORK/data")
want="bundled $t $t
written now
set 1234567890"

# 1. The single-Worker shim, alone; tracing on, every block translated.
"$GEXC" seed --no-shared-memory -o "$WORK/seed.plain.wasm" >/dev/null 2>&1
cat > "$WORK/single.mjs" <<EOF
import { readFileSync, statSync } from 'node:fs';
import { Gex, Env } from '$ROOT/shims/browser/js/gex.mjs';
if (process.env.GEX_ARCH === 'x86') globalThis.GEX_ARCH = 'x86';
class Out extends Env {
  write(fd, b) { (fd === 2 ? process.stderr : process.stdout).write(Buffer.from(b)); }
}
const gex = new Gex(new Out());
const shim = readFileSync('$ROOT/web/public/gex/gex_browser$INFIX.wasm');
const entry = await gex.load(shim, readFileSync('$WORK/seed.plain.wasm'), readFileSync('$WORK/times'),
  { '/data': readFileSync('$WORK/data') }, ['/times', '/data'],
  { dirs: ['/tmp'], mtimes: { '/data': Math.trunc(statSync('$WORK/data').mtimeMs / 1000) }, strace: true });
process.exitCode = await gex.run(entry);
EOF
# Diagnostics go to the console as well (`[gex]`, `[host]`); keep the guest's.
got=$(node "$WORK/single.mjs" 2> "$WORK/single.err" | grep -v '^\[') || true
if [ "$got" != "$want" ]; then
    echo "FAIL: single-Worker shim ($ARCH): got" >&2
    echo "$got" >&2
    grep -i "panic\|re-entered" "$WORK/single.err" | head -3 >&2
    exit 1
fi

# 2. ProcessHost, with the global kernel's filesystem.
got=$(node "$ROOT/shims/browser/js/run-node.mjs" --file "/data=$WORK/data" --argv0 /times \
    "$WORK/seed.plain.wasm" "$WORK/times" /data 2> "$WORK/host.err" | grep -v '^\[') || true
if [ "$got" != "$want" ]; then
    echo "FAIL: ProcessHost ($ARCH): got" >&2
    echo "$got" >&2
    tail -3 "$WORK/host.err" >&2
    exit 1
fi
echo "ok: file times ($ARCH): bundled, written and set, in both filesystems; traced translation does not re-enter"
