/* A process forked, or exec'd, from a thread other than its first, which
 * then starts threads of its own.
 *
 * The page keeps each thread's registers in a context slot. The new
 * process's main thread runs on the slot of the thread that forked or
 * exec'd, slot N rather than 0, and the child numbered its clones' slots
 * from 1: the Nth clone got the main thread's slot and the two ran on one
 * set of registers (the Win7 shell's COM factory child of explorer.exe
 * crashed with its main thread at another thread's pc).
 *
 * Each round, a parent with several threads forks from its last one, and
 * execs itself from its last one. Each new process starts more threads than
 * that and every thread, the main one too, runs a register-heavy loop and
 * checks its answer. Prints "fork-thread-ctx ok ROUNDS". */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define PARENT_THREADS 4
#define CHILD_THREADS 6

static char self[4096];

static __attribute__((noinline)) unsigned long long work(unsigned long long seed)
{
    unsigned long long a = seed, b = seed * 3 + 1, c = seed ^ 0x5555, d = 7;
    for (int i = 0; i < 2000000; i++) {
        a = a * 6364136223846793005ull + b;
        b ^= a >> 17;
        c += b * d;
        d = (d << 1) | (c >> 63);
    }
    return a ^ b ^ c ^ d;
}

static unsigned long long expect[CHILD_THREADS + 1];

static void *run(void *arg)
{
    long i = (long)arg;
    return (void *)(long)(work((unsigned long long)i) != expect[i]);
}

/* The new process: threads beyond the slot it runs on, all working at once. */
static int child(void)
{
    for (long i = 0; i <= CHILD_THREADS; i++) expect[i] = work((unsigned long long)i);
    pthread_t t[CHILD_THREADS];
    for (long i = 1; i <= CHILD_THREADS; i++)
        if (pthread_create(&t[i - 1], NULL, run, (void *)i)) return 2;
    int bad = work(0) != expect[0];
    for (int i = 0; i < CHILD_THREADS; i++) {
        void *r;
        pthread_join(t[i], &r);
        bad += r != NULL;
    }
    return bad ? 3 : 0;
}

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int quit;

static void *idle(void *arg)
{
    (void)arg;
    pthread_mutex_lock(&mu);
    while (!quit) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    return NULL;
}

static const char *how;

static void *last(void *arg)
{
    (void)arg;
    if (!strcmp(how, "exec")) {
        execl(self, self, "child", (char *)NULL);
        _exit(126);
    }
    pid_t pid = fork();
    if (pid == 0) _exit(child());
    int st = 0;
    if (waitpid(pid, &st, 0) != pid) return (void *)1L;
    return (void *)(long)(WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
}

/* A process with threads, forking or exec'ing from its last one. */
static int parent(void)
{
    pthread_t t[PARENT_THREADS];
    for (int i = 0; i < PARENT_THREADS - 1; i++) pthread_create(&t[i], NULL, idle, NULL);
    pthread_create(&t[PARENT_THREADS - 1], NULL, last, NULL);
    void *r;
    pthread_join(t[PARENT_THREADS - 1], &r);
    pthread_mutex_lock(&mu);
    quit = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
    for (int i = 0; i < PARENT_THREADS - 1; i++) pthread_join(t[i], NULL);
    return (int)(long)r;
}

int main(int argc, char **argv)
{
    snprintf(self, sizeof self, "%s", argv[0]);
    if (argc > 1 && !strcmp(argv[1], "child")) return child();
    if (argc > 2 && !strcmp(argv[1], "parent")) { how = argv[2]; return parent(); }
    int rounds = argc > 1 ? atoi(argv[1]) : 3;
    const char *hows[] = { "fork", "exec" };
    for (int i = 1; i <= rounds; i++) {
        for (int k = 0; k < 2; k++) {
            pid_t pid = fork();
            if (pid == 0) {
                execl(self, self, "parent", hows[k], (char *)NULL);
                _exit(127);
            }
            int st = 0;
            if (waitpid(pid, &st, 0) != pid || !WIFEXITED(st) || WEXITSTATUS(st)) {
                printf("%s round %d: status %#x\n", hows[k], i, st);
                return 1;
            }
        }
    }
    printf("fork-thread-ctx ok %d\n", rounds);
    return 0;
}
