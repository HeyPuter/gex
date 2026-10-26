#!/bin/sh
# Thread-local data in shared libraries: one the program starts with (libc's
# registry gives each thread its block) and one it dlopens (gex's loader
# does), read and written from the main thread and three others -- one of
# them already running when the library is opened -- each seeing its own
# value and the initial one first; then a fork's child, which keeps the
# forking thread's values.
#
#   tests/gexcc/dltls.sh DIR    (gexcc on PATH; DIR: where gex can see,
#                                not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (tools/gexcc/install.sh)"; exit 2; }
W=$(mktemp -d "${1:?DIR}/dltls.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/st.c" <<'X'
_Thread_local int st_v = 9;
int st_get(void) { return st_v; }
void st_set(int x) { st_v = x; }
X
cat > "$W/dl.c" <<'X'
_Thread_local int dl_v = 5;
_Thread_local char dl_pad[100] = { 1 };
int dl_get(void) { return dl_v + dl_pad[0] - 1; }
void dl_set(int x) { dl_v = x; }
X
cat > "$W/m.c" <<'X'
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
int st_get(void); void st_set(int);
static int (*get)(void); static void (*set)(int);
static pthread_barrier_t opened, wrote;
static int bad;
static void check(const char *who, int got, int want) {
    if (got != want) { printf("FAIL %s: %d, want %d\n", who, got, want); __atomic_store_n(&bad, 1, __ATOMIC_SEQ_CST); }
}
static void *th(void *a) {
    int k = (int)(long)a;
    pthread_barrier_wait(&opened);
    check("thread's first library value", get(), 5);
    check("thread's first startup value", st_get(), 9);
    set(10 + k); st_set(20 + k);
    pthread_barrier_wait(&wrote);
    check("thread's library value", get(), 10 + k);
    check("thread's startup value", st_get(), 20 + k);
    return 0;
}
int main(int argc, char **argv) {
    pthread_t t[3];
    pthread_barrier_init(&opened, 0, 4);
    pthread_barrier_init(&wrote, 0, 3);
    st_set(1);
    /* one thread before the dlopen: it instantiates the library later */
    pthread_create(&t[0], 0, th, (void *)0);
    void *h = dlopen(argc > 1 ? argv[1] : "libdl.so", RTLD_NOW);
    if (!h) { printf("FAIL dlopen: %s\n", dlerror()); return 1; }
    get = (int (*)(void))dlsym(h, "dl_get");
    set = (void (*)(int))dlsym(h, "dl_set");
    if (!get || !set) { printf("FAIL dlsym\n"); return 1; }
    check("main's first library value", get(), 5);
    set(100);
    for (int k = 1; k < 3; k++) pthread_create(&t[k], 0, th, (void *)(long)k);
    pthread_barrier_wait(&opened);
    for (int k = 0; k < 3; k++) pthread_join(t[k], 0);
    check("main's library value", get(), 100);
    check("main's startup value", st_get(), 1);
    pid_t c = fork();
    if (c == 0) {
        int ok = get() == 100 && st_get() == 1;
        set(7);
        _exit(ok && get() == 7 ? 0 : 1);
    }
    int s = 0;
    waitpid(c, &s, 0);
    check("fork child", WIFEXITED(s) ? WEXITSTATUS(s) : 99, 0);
    check("main's library value after the fork", get(), 100);
    printf("%s\n", bad ? "FAIL" : "ok");
    return bad;
}
X
GEXCC_SHARED=1 gexcc -fPIC -shared -o "$W/libst.so" "$W/st.c" &&
    GEXCC_SHARED=1 gexcc -fPIC -shared -o "$W/libdl.so" "$W/dl.c" &&
    GEXCC_SHARED=1 gexcc -pthread -o "$W/m" "$W/m.c" -L"$W" -lst -Wl,-rpath,"$W" || { echo "FAIL build"; exit 1; }
out=$(cd "$W" && gexcc-exec "$W/m" "$W/libdl.so" 2>&1)
if [ "$(printf '%s\n' "$out" | tail -1)" = ok ]; then echo "ok dltls"; else printf '%s\n' "$out" | tail -8; echo "FAIL dltls"; exit 1; fi
