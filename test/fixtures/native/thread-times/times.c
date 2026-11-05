/* Each thread's own name and processor time in /proc, as btop, top -H and a
 * per-thread profile read them: two threads name themselves and spin, one
 * for twice the other's time, twice over; after each round the process
 * reads their task/<tid>/stat and comm, and so does a child of it (another
 * process's /proc/<pid>/task, which comes from the process table). Prints
 * only what holds on Linux, so a run there and one on gex print the same. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* Thread A spins SPIN_MS a round, thread B twice that. */
#define SPIN_MS 300
#define ROUNDS 2
static int tids[2];
static pthread_barrier_t ready, done[ROUNDS], go[ROUNDS];

static long thread_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &ts);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void *spin(void *arg) {
    int i = (int)(long)arg;
    tids[i] = syscall(SYS_gettid);
    pthread_setname_np(pthread_self(), i ? "spin-b" : "spin-a");
    char got[16] = "";
    prctl(PR_GET_NAME, got);
    if (strcmp(got, i ? "spin-b" : "spin-a") != 0) printf("FAIL thread %d: PR_GET_NAME gives '%s'\n", i, got);
    pthread_barrier_wait(&ready);
    for (int r = 0; r < ROUNDS; r++) {
        long until = thread_ms() + SPIN_MS * (i + 1);
        while (thread_ms() < until)
            ;
        pthread_barrier_wait(&done[r]);
        pthread_barrier_wait(&go[r]);
    }
    return 0;
}

static void slurp(const char *path, char *b, size_t cap) {
    FILE *f = fopen(path, "r");
    b[0] = 0;
    if (!f) return;
    size_t n = fread(b, 1, cap - 1, f);
    b[n] = 0;
    fclose(f);
}

/* A task's name (stat's, between the parentheses) and utime+stime, ticks. */
static int task(int pid, int tid, char *name, long *ticks) {
    char path[64], b[1024], comm[32];
    snprintf(path, sizeof path, "/proc/%d/task/%d/stat", pid, tid);
    slurp(path, b, sizeof b);
    char *l = strchr(b, '('), *r = strrchr(b, ')');
    if (!l || !r) return -1;
    snprintf(name, 16, "%.*s", (int)(r - l - 1), l + 1);
    /* after ") ": state(3) ... utime(14) stime(15) */
    char *p = r + 2;
    for (int f = 3; f < 14 && p; f++) p = strchr(p, ' ') ? strchr(p, ' ') + 1 : 0;
    if (!p) return -1;
    long ut = 0, st = 0;
    sscanf(p, "%ld %ld", &ut, &st);
    *ticks = ut + st;
    snprintf(path, sizeof path, "/proc/%d/task/%d/comm", pid, tid);
    slurp(path, comm, sizeof comm);
    comm[strcspn(comm, "\n")] = 0;
    if (strcmp(comm, name) != 0) printf("FAIL comm '%s' is not stat's '%s'\n", comm, name);
    return 0;
}

/* The checks of round `r` (0-based), seen by `who`; `prev` the last round's
 * ticks, updated. A thread spun SPIN_MS (A) or twice that (B) more since.
 * Bounds with room: Linux counts at its tick, gex says a thread's time once
 * a tick of it has gathered. */
static void check(const char *who, int pid, int r, long prev[2]) {
    long t[2];
    for (int i = 0; i < 2; i++) {
        char name[16];
        if (task(pid, tids[i], name, &t[i]) != 0) {
            printf("FAIL %s round %d: no stat for thread %c\n", who, r + 1, 'a' + i);
            return;
        }
        const char *want = i ? "spin-b" : "spin-a";
        if (strcmp(name, want) != 0) printf("FAIL %s round %d: thread %c is called '%s'\n", who, r + 1, 'a' + i, name);
        long grew = t[i] - prev[i], least = SPIN_MS * (i + 1) / 10 * 2 / 3;
        if (grew < least)
            printf("FAIL %s round %d: thread %c's time grew %ld ticks, not %ld or more\n", who, r + 1, 'a' + i, grew, least);
    }
    /* B spun twice what A did: more, and not by A's whole share less */
    if (t[1] - prev[1] <= t[0] - prev[0])
        printf("FAIL %s round %d: B grew %ld ticks, A %ld: B spun twice as long\n", who, r + 1, t[1] - prev[1], t[0] - prev[0]);
    printf("%s round %d: names and times hold\n", who, r + 1);
    prev[0] = t[0];
    prev[1] = t[1];
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    pthread_barrier_init(&ready, 0, 3);
    for (int r = 0; r < ROUNDS; r++) {
        pthread_barrier_init(&done[r], 0, 3);
        pthread_barrier_init(&go[r], 0, 3);
    }
    pthread_t th[2];
    for (int i = 0; i < 2; i++) pthread_create(&th[i], 0, spin, (void *)(long)i);
    pthread_barrier_wait(&ready);
    long mine[2] = {0, 0}, theirs[2] = {0, 0};
    for (int r = 0; r < ROUNDS; r++) {
        pthread_barrier_wait(&done[r]);
        /* a moment for what the threads said to land */
        usleep(100000);
        check("self", getpid(), r, mine);
        int pid = getpid();
        pid_t c = fork();
        if (c == 0) {
            check("child", pid, r, theirs);
            fflush(stdout);
            _exit(0);
        }
        int st;
        waitpid(c, &st, 0);
        /* the child's view of the next round starts from this one's */
        long t;
        char name[16];
        for (int i = 0; i < 2; i++)
            if (task(pid, tids[i], name, &t) == 0) theirs[i] = t;
        pthread_barrier_wait(&go[r]);
    }
    for (int i = 0; i < 2; i++) pthread_join(th[i], 0);
    /* the first thread never named itself: the program's name */
    char name[16];
    long t;
    if (task(getpid(), getpid(), name, &t) == 0) printf("first thread is called %s\n", name);
    return 0;
}
