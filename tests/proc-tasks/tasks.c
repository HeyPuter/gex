/* What /proc says about a process's threads, printed with the ids replaced
 * by names (P the process, T0 its first thread, T1.. the others it starts),
 * so a run on Linux and a run on gex print the same. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define N 3
static int tids[N + 1];
static pthread_barrier_t started, go[N + 1];

static const char *name(int id) {
    static char buf[8][16];
    static int k;
    char *b = buf[k++ % 8];
    for (int i = 0; i <= N; i++)
        if (tids[i] == id) { snprintf(b, 16, "T%d", i); return b; }
    snprintf(b, 16, id == getpid() ? "P" : "other");
    return b;
}

static char *slurp(const char *path) {
    static char buf[8][4096];
    static int k;
    char *b = buf[k++ % 8];
    FILE *f = fopen(path, "r");
    if (!f) { snprintf(b, 4096, "(%s)", strerror(errno)); return b; }
    size_t n = fread(b, 1, 4095, f);
    b[n] = 0;
    fclose(f);
    return b;
}

static int field(const char *status, const char *key) {
    const char *p = strstr(status, key);
    return p ? atoi(p + strlen(key)) : -1;
}

/* stat's fields after the ")": 3 is state, 4 ppid, 20 num_threads */
static long stat_field(const char *stat, int n) {
    const char *p = strrchr(stat, ')');
    if (!p) return -1;
    p += 2;
    for (int i = 3; i < n; i++) { p = strchr(p, ' '); if (!p) return -1; p++; }
    return atol(p);
}

static int cmp(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }

static void list(const char *dir, const char *label) {
    int ids[64], n = 0;
    DIR *d = opendir(dir);
    if (!d) { printf("%s: (%s)\n", label, strerror(errno)); return; }
    struct dirent *e;
    while ((e = readdir(d)) && n < 64)
        if (e->d_name[0] >= '0' && e->d_name[0] <= '9') ids[n++] = atoi(e->d_name);
    closedir(d);
    qsort(ids, n, sizeof *ids, cmp);
    printf("%s:", label);
    for (int i = 0; i < n; i++) printf(" %s", name(ids[i]));
    printf("\n");
}

static void task(const char *proc, int tid) {
    char p[96];
    snprintf(p, sizeof p, "%s/task/%d/stat", proc, tid);
    char *stat = slurp(p);
    snprintf(p, sizeof p, "%s/task/%d/status", proc, tid);
    char *st = slurp(p);
    printf("  %s: stat id %s ppid %s threads %ld; status Pid %s Tgid %s Threads %d\n", name(tid),
           name(atoi(stat)), stat_field(stat, 4) == getppid() ? "parent" : "?", stat_field(stat, 20),
           name(field(st, "\nPid:")), name(field(st, "\nTgid:")), field(st, "\nThreads:"));
    snprintf(p, sizeof p, "/proc/%d/status", tid);
    st = slurp(p);
    snprintf(p, sizeof p, "/proc/%d/stat", tid);
    stat = slurp(p);
    printf("  /proc/%s: stat id %s; status Pid %s Tgid %s\n", name(tid), name(atoi(stat)),
           name(field(st, "\nPid:")), name(field(st, "\nTgid:")));
}

static void report(const char *when) {
    printf("%s\n", when);
    printf("  status Threads %d, stat threads %ld\n", field(slurp("/proc/self/status"), "\nThreads:"),
           stat_field(slurp("/proc/self/stat"), 20));
    list("/proc/self/task", "  task");
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/task", getpid());
    list(p, "  /proc/P/task");
}

static void *body(void *arg) {
    int i = (int)(long)arg;
    tids[i] = syscall(SYS_gettid);
    pthread_barrier_wait(&started);
    pthread_barrier_wait(&go[i]);
    return 0;
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    tids[0] = syscall(SYS_gettid);
    printf("T0 is P: %s\n", tids[0] == getpid() ? "yes" : "no");
    report("alone");
    pthread_t t[N + 1];
    pthread_barrier_init(&started, 0, N + 1);
    for (int i = 1; i <= N; i++) {
        pthread_barrier_init(&go[i], 0, 2);
        pthread_create(&t[i], 0, body, (void *)(long)i);
    }
    pthread_barrier_wait(&started);
    report("with 3 threads");
    for (int i = 0; i <= N; i++) task("/proc/self", tids[i]);
    {
        char p[96];
        snprintf(p, sizeof p, "/proc/%d", getpid());
        task(p, tids[2]);
        snprintf(p, sizeof p, "/proc/%d/task/%d/comm", getpid(), tids[1]);
        printf("  T1's comm is P's: %s\n", strcmp(slurp(p), slurp("/proc/self/comm")) ? "no" : "yes");
        snprintf(p, sizeof p, "/proc/self/task/%d/stat", getpid() + 100000);
        printf("  task of no thread: %s\n", slurp(p));
    }
    int in_proc = 0, t1_in_proc = 0;
    DIR *d = opendir("/proc");
    struct dirent *e;
    while ((e = readdir(d))) {
        in_proc |= atoi(e->d_name) == getpid();
        t1_in_proc |= atoi(e->d_name) == tids[1];
    }
    closedir(d);
    printf("  /proc lists P: %s, T1: %s\n", in_proc ? "yes" : "no", t1_in_proc ? "yes" : "no");
    /* another process's view */
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        char p[64];
        snprintf(p, sizeof p, "/proc/%d/task", getppid());
        list(p, "  from a child, the parent's task");
        snprintf(p, sizeof p, "/proc/%d/status", getppid());
        printf("  from a child, the parent's Threads %d\n", field(slurp(p), "\nThreads:"));
        snprintf(p, sizeof p, "/proc/%d/task/%d/status", getppid(), tids[3]);
        printf("  from a child, T3's Pid %s\n", name(field(slurp(p), "\nPid:")));
        printf("  the child's own Threads %d\n", field(slurp("/proc/self/status"), "\nThreads:"));
        _exit(0);
    }
    waitpid(c, 0, 0);
    pthread_barrier_wait(&go[3]);
    pthread_join(t[3], 0);
    report("after T3 ended");
    for (int i = 1; i < N; i++) { pthread_barrier_wait(&go[i]); pthread_join(t[i], 0); }
    report("after all ended");
    return 0;
}
