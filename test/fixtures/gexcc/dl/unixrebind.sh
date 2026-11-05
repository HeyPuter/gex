#!/bin/sh
# A unix socket's name is its file's: unlinked, the name can be bound again,
# by a new socket, whether the old one is closed or still open, and the old
# one's connections go on working; renamed, the name moves with the file.
# While the file is there, a bind of the name is EADDRINUSE, and that failed
# bind leaves the socket free to bind once the file is gone. (wineserver
# unlinks a stale socket and binds its own; with the name still taken every
# Wine program after the first wineserver said "couldn't bind to the socket
# even though we hold the lock".) Natively (GEXC) and in the browser runtime
# with the global kernel (run-node, GEX_SHARED=1, GEX_ROOT's shims).
#
#   test/fixtures/gexcc/dl/unixrebind.sh DIR   (gexcc on PATH; GEXC: the gexc to test;
#                                    GEX_ROOT: a gex checkout with
#                                    page/public/gex built; DIR: where gex can
#                                    see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "skip unixrebind: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
G=${GEXC:?GEXC}
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
SEED=$ROOT/page/public/gex/seed.shared.wasm
W=$(mktemp -d "${1:?DIR}/unixrebind.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/r.c" <<'X'
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
static struct sockaddr_un at(const char *p) {
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strcpy(a.sun_path, p);
    return a;
}
static int bind_to(int s, const char *p) {
    struct sockaddr_un a = at(p);
    return bind(s, (struct sockaddr *)&a, sizeof a) ? -errno : 0;
}
static int listener(const char *p) {
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (bind_to(s, p) || listen(s, 4)) { close(s); return -1; }
    return s;
}
static int dial(const char *p) {
    struct sockaddr_un a = at(p);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (connect(s, (struct sockaddr *)&a, sizeof a)) { close(s); return -1; }
    return s;
}
int main(void) {
    const char *p = "/tmp/rb1", *q = "/tmp/rb2";
    unlink(p); unlink(q);
    int a = listener(p);
    close(a);
    unlink(p);
    int b = listener(p);
    printf("closed, unlinked, bound again: %s\n", b >= 0 ? "yes" : "no");
    int c1 = dial(p), s1 = c1 >= 0 ? accept(b, 0, 0) : -1;
    int t = socket(AF_UNIX, SOCK_STREAM, 0);
    int e = bind_to(t, p);
    printf("bound over the file: %s\n", e == -EADDRINUSE ? "EADDRINUSE" : strerror(-e));
    unlink(p);
    e = bind_to(t, p);
    printf("open, unlinked, the same socket bound again: %s\n", e == 0 && listen(t, 4) == 0 ? "yes" : strerror(-e));
    char buf[8] = {0};
    int old = write(c1, "old", 3) == 3 && read(s1, buf, sizeof buf) == 3 && !strcmp(buf, "old");
    printf("the old connection: %s\n", old ? "works" : "broken");
    int c2 = dial(p), s2 = c2 >= 0 ? accept(t, 0, 0) : -1;
    printf("a new connection reaches the new socket: %s\n", s2 >= 0 ? "yes" : "no");
    rename(p, q);
    int c3 = dial(q), s3 = c3 >= 0 ? accept(t, 0, 0) : -1;
    int c4 = dial(p);
    printf("renamed: the new name %s, the old %s\n", s3 >= 0 ? "answers" : "is dead", c4 < 0 ? "is free" : "answers");
    int d = listener(p);
    printf("the old name bound again: %s\n", d >= 0 ? "yes" : "no");
    return 0;
}
X
gexcc -o "$W/r" "$W/r.c" || { echo "FAIL unixrebind: build"; exit 1; }
want='closed, unlinked, bound again: yes
bound over the file: EADDRINUSE
open, unlinked, the same socket bound again: yes
the old connection: works
a new connection reaches the new socket: yes
renamed: the new name answers, the old is free
the old name bound again: yes'
fail=0
mkdir -p "$W/root/t" "$W/root/tmp" && cp "$W/r" "$W/root/t/r"
out=$(timeout 120 "$G" run --sysroot "$W/root" "$W/root/t/r" 2>&1 | grep -v '^\[')
[ "$out" = "$want" ] && echo "ok unixrebind (native)" || { echo "FAIL unixrebind (native):"; printf '%s\n' "$out"; fail=1; }
if [ -f "$SEED" ]; then
    out=$(GEX_SHARED=1 timeout 120 node "$ROOT/hosts/browser/js/run-node.mjs" --file /t/r="$W/r" --argv0 /t/r \
        "$SEED" "$W/r" 2>&1 | grep -v '^\[')
    [ "$out" = "$want" ] && echo "ok unixrebind (browser runtime)" || { echo "FAIL unixrebind (browser runtime):"; printf '%s\n' "$out"; fail=1; }
else
    echo "skip unixrebind (browser runtime): no $SEED"
fi
exit $fail
