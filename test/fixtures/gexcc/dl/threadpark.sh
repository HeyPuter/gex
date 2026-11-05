#!/bin/sh
# Threads started one after another on a Worker whose last thread ended
# (host.mjs `#park`, gex.mjs `parkThread`): none of a thread's state reaches
# the next. A wasm32 program starts 48 short threads, each joined before
# the next. Each checks what a new thread has, then dirties it all:
#   - its thread-locals (zero, and an initialised one at its value)
#   - its tid (not main's, not the last thread's, gettid as the kernel says)
#   - an empty signal mask and nothing pending; it blocks three signals and
#     sends itself one
#   - errno 0; it sets it
#   - a dlopened library's thread-local at its initial value; the library
#     is dlopened by main halfway, while the last thread's Worker waits,
#     so that Worker must install it before the next thread runs
# Run natively (gexcc-exec, the control) and in the browser runtime
# (run-node, with every program given a spare, GEX_TEST_SPARE_MODULES=1).
# There, at least half the threads must have run on a Worker whose last
# thread ended, and one of them after it installed the library.
#
#   test/fixtures/gexcc/dl/threadpark.sh DIR   (gexcc on PATH; GEX_ROOT: a gex checkout
#                                   with page/public/gex built; DIR: where gex
#                                   can see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/page/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED"; exit 2; }
W=$(mktemp -d "${1:?DIR}/threadpark.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/dl.c" <<'X'
_Thread_local int dl_v = 5;
int dl_get(void) { return dl_v; }
void dl_set(int x) { dl_v = x; }
X
cat > "$W/m.c" <<'X'
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>
#define N 48
static _Thread_local int tl_zero;
static _Thread_local char tl_buf[256];
static _Thread_local int tl_init = 7;
static int (*get)(void);
static void (*set)(int);
static pid_t main_tid, last_tid;
static int bad;
static void fail(int k, const char *what, long got) {
    printf("FAIL thread %d: %s (%ld)\n", k, what, got);
    __atomic_store_n(&bad, 1, __ATOMIC_SEQ_CST);
}
static void *th(void *a) {
    int k = (int)(long)a;
    if (errno != 0) fail(k, "errno at start", errno);
    if (tl_zero != 0) fail(k, "thread-local not zero", tl_zero);
    if (tl_init != 7) fail(k, "thread-local not at its initial value", tl_init);
    for (int i = 0; i < (int)sizeof tl_buf; i++)
        if (tl_buf[i]) { fail(k, "thread-local array not zero at", i); break; }
    pid_t me = (pid_t)syscall(SYS_gettid);
    if (me == main_tid || me == last_tid || me != gettid()) fail(k, "tid", me);
    sigset_t s;
    pthread_sigmask(SIG_BLOCK, 0, &s);
    for (int sig = 1; sig < 65; sig++)
        if (sigismember(&s, sig) == 1) { fail(k, "signal blocked at start", sig); break; }
    sigpending(&s);
    for (int sig = 1; sig < 65; sig++)
        if (sigismember(&s, sig) == 1) { fail(k, "signal pending at start", sig); break; }
    if (get) {
        if (get() != 5) fail(k, "library thread-local not at its initial value", get());
        set(100 + k);
    }
    tl_zero = k + 1;
    memset(tl_buf, 0xa5, sizeof tl_buf);
    tl_init = -k;
    sigset_t b;
    sigemptyset(&b);
    sigaddset(&b, SIGUSR1);
    sigaddset(&b, SIGUSR2);
    sigaddset(&b, SIGRTMIN + 3);
    pthread_sigmask(SIG_BLOCK, &b, 0);
    syscall(SYS_tgkill, getpid(), me, SIGUSR2);
    sigpending(&s);
    if (sigismember(&s, SIGUSR2) != 1) fail(k, "own SIGUSR2 not pending", 0);
    last_tid = me;
    errno = 1234;
    return 0;
}
int main(int argc, char **argv) {
    main_tid = gettid();
    for (int k = 0; k < N; k++) {
        if (k == N / 2) {
            /* while the last thread's Worker waits for the next */
            usleep(300000);
            void *h = dlopen(argv[1], RTLD_NOW);
            if (!h) { printf("FAIL dlopen: %s\n", dlerror()); return 1; }
            get = (int (*)(void))dlsym(h, "dl_get");
            set = (void (*)(int))dlsym(h, "dl_set");
            if (!get || !set) { printf("FAIL dlsym\n"); return 1; }
            usleep(300000);
        }
        pthread_t t;
        if (pthread_create(&t, 0, th, (void *)(long)k)) { printf("FAIL pthread_create %d\n", k); return 1; }
        pthread_join(t, 0);
    }
    if (tl_zero != 0 || tl_init != 7 || get() != 5) fail(-1, "main's thread-locals changed", tl_zero);
    printf("%s\n", bad ? "FAIL" : "ok");
    return bad;
}
X
LIBDIR=$(dirname "$(command -v gexcc)")/../sysroot/lib
GEXCC_SHARED=1 gexcc -fPIC -shared -o "$W/libdl.so" "$W/dl.c" &&
    GEXCC_SHARED=1 gexcc -pthread -o "$W/m" "$W/m.c" || { echo "FAIL build"; exit 1; }
fail=0
# The control: natively.
out=$(cd "$W" && timeout "${TMO:-300}" gexcc-exec "$W/m" "$W/libdl.so" 2>&1)
last=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$' | tail -1)
[ "$last" = ok ] || { printf '%s\n' "$out" | tail -8; echo "FAIL threadpark: natively the program said '$last'"; fail=1; }
# The browser runtime.
out=$(GEX_TEST_SPARE_MODULES=1 GEX_LOG=spare GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" \
    node "$ROOT/hosts/browser/js/run-node.mjs" \
    --file "/usr/lib/wasm32-linux-musl/libc.so=$LIBDIR/libc.so" \
    --file "/t/m=$W/m" --file "/t/libdl.so=$W/libdl.so" --cwd /t --argv0 ./m \
    "$SEED" "$W/m" /t/libdl.so 2>&1)
last=$(printf '%s\n' "$out" | grep -v '^\[' | grep -v '^$' | tail -1)
[ "$last" = ok ] || { echo "FAIL threadpark: in the browser runtime the program said '$last'"; fail=1; }
parked=$(printf '%s\n' "$out" | grep -c '^\[spare\] [0-9]*:spare[0-9]*: [0-9]*:[0-9]* ended, waits')
[ "$parked" -ge 24 ] || { echo "FAIL threadpark: $parked threads' Workers waited for the next thread, want 24 or more"; fail=1; }
printf '%s\n' "$out" | grep -q '^\[spare\] pid [0-9]*: caught up with [0-9]* modules while waiting\|^\[spare\] tid [0-9]*: adopted, [1-9][0-9]* modules more' ||
    { echo "FAIL threadpark: no waiting Worker installed the library dlopened meanwhile"; fail=1; }
if [ $fail = 0 ]; then echo "ok threadpark: natively and in the browser runtime, 48 threads clean, $parked on a Worker whose last thread ended"; else printf '%s\n' "$out" | grep -v '^\[spare\]' | tail -20; fi
exit $fail
