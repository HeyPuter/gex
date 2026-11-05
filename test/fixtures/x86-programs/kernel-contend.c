/* Threads that live in the kernel at once: a
 * contended mutex, condition-variable ping-pong between pairs, PI locks,
 * mmap/munmap churn, and calls that read process state, all from several
 * threads together. Correctness: the counters come out exact. Speed: the
 * time each phase took goes to stderr (test/x86-programs compares stdout only), so
 * the same binary is the KLock microbench: KERNEL_CONTEND_SCALE=N
 * multiplies the iteration counts. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define THREADS 4
static long scale = 1;

static double now_s(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* 1: one mutex, every thread. */
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static void *mutex_worker(void *arg) {
    (void)arg;
    for (long i = 0; i < 20000 * scale; i++) {
        pthread_mutex_lock(&mu);
        counter++;
        pthread_mutex_unlock(&mu);
    }
    return NULL;
}

/* 2: pairs passing a token through a condition variable. */
struct pair {
    pthread_mutex_t m;
    pthread_cond_t c;
    long turn, passes;
};
static struct pair pairs[THREADS / 2];
struct side {
    struct pair *p;
    int me;
};
static void *pingpong(void *arg) {
    struct side *s = arg;
    struct pair *p = s->p;
    for (long i = 0; i < 2000 * scale; i++) {
        pthread_mutex_lock(&p->m);
        while (p->turn % 2 != s->me) pthread_cond_wait(&p->c, &p->m);
        p->turn++;
        p->passes++;
        pthread_cond_signal(&p->c);
        pthread_mutex_unlock(&p->m);
    }
    return NULL;
}

/* 3: priority-inheritance locks (FUTEX_LOCK_PI / UNLOCK_PI). */
static pthread_mutex_t pi;
static long pi_counter;
static void *pi_worker(void *arg) {
    (void)arg;
    for (long i = 0; i < 5000 * scale; i++) {
        pthread_mutex_lock(&pi);
        pi_counter++;
        pthread_mutex_unlock(&pi);
    }
    return NULL;
}

/* 4: the memory map, from every thread: map, touch, unmap. */
static long mapped[THREADS];
static void *map_worker(void *arg) {
    long me = (long)arg;
    for (long i = 0; i < 500 * scale; i++) {
        size_t len = 65536 + 4096 * (i % 4);
        unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) return NULL;
        p[0] = 1;
        p[len - 1] = 2;
        if (mprotect(p, 4096, PROT_READ) == 0 && p[0] == 1 && p[len - 1] == 2) mapped[me]++;
        munmap(p, len);
    }
    return NULL;
}

/* 5: calls that only read the process or thread. */
static long idents[THREADS];
static void *ident_worker(void *arg) {
    long me = (long)arg;
    pid_t pid = getpid();
    for (long i = 0; i < 20000 * scale; i++) {
        if (getpid() == pid && syscall(SYS_gettid) > 0 && getppid() >= 0) idents[me]++;
    }
    return NULL;
}

static double run(const char *name, void *(*fn)(void *), int pass_index) {
    pthread_t t[THREADS];
    double t0 = now_s();
    for (long i = 0; i < THREADS; i++) pthread_create(&t[i], NULL, fn, pass_index ? (void *)i : NULL);
    for (int i = 0; i < THREADS; i++) pthread_join(t[i], NULL);
    double dt = now_s() - t0;
    fprintf(stderr, "[kernel-contend] %-8s %.3f s\n", name, dt);
    return dt;
}

int main(void) {
    const char *s = getenv("KERNEL_CONTEND_SCALE");
    if (s && atol(s) > 0) scale = atol(s);
    double total = 0;
    total += run("mutex", mutex_worker, 0);

    pthread_t t[THREADS];
    struct side sides[THREADS];
    double t0 = now_s();
    for (int i = 0; i < THREADS / 2; i++) {
        pthread_mutex_init(&pairs[i].m, NULL);
        pthread_cond_init(&pairs[i].c, NULL);
    }
    for (int i = 0; i < THREADS; i++) {
        sides[i].p = &pairs[i / 2];
        sides[i].me = i % 2;
        pthread_create(&t[i], NULL, pingpong, &sides[i]);
    }
    for (int i = 0; i < THREADS; i++) pthread_join(t[i], NULL);
    double dt = now_s() - t0;
    fprintf(stderr, "[kernel-contend] %-8s %.3f s\n", "condvar", dt);
    total += dt;

    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_setprotocol(&a, PTHREAD_PRIO_INHERIT);
    pthread_mutex_init(&pi, &a);
    total += run("pi", pi_worker, 0);
    total += run("mmap", map_worker, 1);
    total += run("ident", ident_worker, 1);
    fprintf(stderr, "[kernel-contend] total    %.3f s\n", total);

    long passes = 0, maps = 0, ids = 0;
    for (int i = 0; i < THREADS / 2; i++) passes += pairs[i].passes;
    for (int i = 0; i < THREADS; i++) maps += mapped[i], ids += idents[i];
    printf("mutex %ld\ncondvar %ld\npi %ld\nmmap %ld\nident %ld\n", counter / scale, passes / scale,
           pi_counter / scale, maps / scale, ids / scale);
    return 0;
}
