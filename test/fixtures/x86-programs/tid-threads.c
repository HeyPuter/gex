/* set_tid_address from many threads at once: the word a thread's exit
 * clears and wakes is the one it last named, its own.
 *
 * - Rounds of eight pthreads. Half of them name a word of their own with
 *   set_tid_address (whose answer is their tid) and end with a raw exit:
 *   musl's own exit takes its thread-list lock, which the word the clone
 *   named releases, so those threads never go through it (and are never
 *   joined). Main waits on each named word with futex until the thread's
 *   exit clears it; the others are joined, and their words stay as they
 *   were.
 * - Meanwhile another thread creates and joins pthreads of its own (whose
 *   exits release musl's lock through the word their clone named). */
#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 25
#define N 8
#define JOINS 200

struct kid {
    _Atomic int word;
    int names;
    long answer, tid;
};

static struct kid kids[ROUNDS][N];
static atomic_int bad, joined;

static void fail(const char *what, long n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%ld)\n", what, n);
}

static void *kid_main(void *p) {
    struct kid *k = p;
    k->tid = syscall(SYS_gettid);
    if (k->names) {
        k->answer = syscall(SYS_set_tid_address, &k->word);
        for (;;) syscall(SYS_exit, 0);
    }
    return 0;
}

/* Wait until *w is zero, at most about thirty seconds (a loaded host). */
static int wait_clear(_Atomic int *w) {
    for (int i = 0; i < 3000; i++) {
        int v = atomic_load(w);
        if (v == 0) return 1;
        struct timespec ts = {0, 10 * 1000 * 1000};
        syscall(SYS_futex, w, FUTEX_WAIT, v, &ts, 0, 0);
    }
    return atomic_load(w) == 0;
}

static void *nothing(void *p) { return p; }

static void *joiner(void *p) {
    (void)p;
    for (int i = 0; i < JOINS; i++) {
        pthread_t t;
        void *r = 0;
        if (pthread_create(&t, 0, nothing, (void *)(long)(i + 1))) { fail("pthread_create", i); continue; }
        if (pthread_join(t, &r) || r != (void *)(long)(i + 1)) fail("pthread_join", i);
        else atomic_fetch_add(&joined, 1);
    }
    return 0;
}

int main(void) {
    pthread_t j;
    if (pthread_create(&j, 0, joiner, 0)) fail("joiner", 0);
    int made = 0, cleared = 0, kept = 0;
    for (int r = 0; r < ROUNDS; r++) {
        pthread_t t[N];
        for (int i = 0; i < N; i++) {
            struct kid *k = &kids[r][i];
            atomic_store(&k->word, -1);
            k->names = (i + r) & 1;
            if (pthread_create(&t[i], 0, kid_main, k)) { fail("pthread_create", r * N + i); t[i] = 0; continue; }
            made++;
        }
        for (int i = 0; i < N; i++)
            if (t[i] && !kids[r][i].names && pthread_join(t[i], 0)) fail("pthread_join", r * N + i);
        for (int i = 0; i < N; i++) {
            struct kid *k = &kids[r][i];
            if (!t[i]) continue;
            if (!k->names) {
                if (atomic_load(&k->word) == -1) kept++;
                else fail("a word nobody named changed", r * N + i);
                continue;
            }
            /* Its answer once it has gone (it is never joined). */
            if (wait_clear(&k->word)) cleared++;
            else fail("not cleared", r * N + i);
            if (k->answer != k->tid) fail("set_tid_address's answer", k->answer);
        }
    }
    pthread_join(j, 0);
    printf("made %d cleared %d kept %d joined %d bad %d\n", made, cleared, kept, atomic_load(&joined), atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
