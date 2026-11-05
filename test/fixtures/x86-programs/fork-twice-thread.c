/* A threaded parent forking twice in quick succession from a thread that
 * is not its first, starting a short-lived thread after each fork, as the
 * Win7 shell's explorer launches one program and then another: both
 * children and the parent must keep running.
 *
 * The parent (the shell) is itself forked from a second thread, as
 * explorer is, so its main thread runs on a context slot other than the
 * first, and its threads must be numbered past it. In the page they were
 * not: a thread the shell started took its main thread's context, and the
 * shell took a wild branch.
 *
 * Each round, the launching thread forks a child that starts threads of
 * its own and works, starts a helper thread that exits at once, then forks
 * again straight away. The shell's other threads work all the while and
 * check their results; so does the launching thread after each fork. Prints
 * "fork-twice-thread ok ROUNDS". */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

#define WORKERS 3

static __attribute__((noinline)) unsigned long long work(unsigned long long seed, int n)
{
    unsigned long long a = seed, b = seed * 3 + 1, c = seed ^ 0x5555, d = 7;
    for (int i = 0; i < n; i++) {
        a = a * 6364136223846793005ull + b;
        b ^= a >> 17;
        c += b * d;
        d = (d << 1) | (c >> 63);
    }
    return a ^ b ^ c ^ d;
}

static atomic_int stop, bad;

static void *worker(void *arg)
{
    unsigned long long seed = (unsigned long long)(long)arg;
    unsigned long long want = work(seed, 20000);
    while (!atomic_load(&stop))
        if (work(seed, 20000) != want) atomic_fetch_add(&bad, 1);
    return NULL;
}

static void *brief(void *arg)
{
    (void)arg;
    return (void *)(long)(work(9, 1000) != work(9, 1000));
}

static void *child_thread(void *arg)
{
    long i = (long)arg;
    return (void *)(long)(work((unsigned long long)i, 200000) != work((unsigned long long)i, 200000));
}

/* A child: threads of its own, working. */
static int child(void)
{
    pthread_t t[3];
    for (long i = 0; i < 3; i++) pthread_create(&t[i], NULL, child_thread, (void *)i);
    int n = work(5, 200000) != work(5, 200000);
    for (int i = 0; i < 3; i++) {
        void *r;
        pthread_join(t[i], &r);
        n += r != NULL;
    }
    return n;
}

static int rounds;

static void *launcher(void *arg)
{
    (void)arg;
    unsigned long long want = work(3, 50000);
    long failed = 0;
    for (int i = 0; i < rounds; i++) {
        pid_t pid[2];
        for (int k = 0; k < 2; k++) {
            pid[k] = fork();
            if (pid[k] == 0) _exit(child());
            if (pid[k] < 0) { failed++; continue; }
            pthread_t b;
            void *r;
            pthread_create(&b, NULL, brief, NULL);
            pthread_join(b, &r);
            failed += r != NULL;
            if (work(3, 50000) != want) failed++;
        }
        for (int k = 0; k < 2; k++) {
            int st = 0;
            if (pid[k] > 0 && (waitpid(pid[k], &st, 0) != pid[k] || !WIFEXITED(st) || WEXITSTATUS(st))) {
                printf("round %d child %d: status %#x\n", i, k, st);
                failed++;
            }
        }
    }
    return (void *)failed;
}

/* The shell: workers, and a launcher forking from its own thread. */
static int shell(void)
{
    pthread_t w[WORKERS], l;
    for (long i = 0; i < WORKERS; i++) pthread_create(&w[i], NULL, worker, (void *)(i + 1));
    pthread_create(&l, NULL, launcher, NULL);
    void *r;
    pthread_join(l, &r);
    atomic_store(&stop, 1);
    for (int i = 0; i < WORKERS; i++) pthread_join(w[i], NULL);
    if (r || atomic_load(&bad)) {
        printf("launcher %ld, workers %d\n", (long)r, atomic_load(&bad));
        return 1;
    }
    return 0;
}

static void *idle(void *arg)
{
    (void)arg;
    while (!atomic_load(&stop)) usleep(1000);
    return NULL;
}

/* Fork the shell from a second thread, as explorer is started. */
static void *start_shell(void *arg)
{
    (void)arg;
    pid_t pid = fork();
    if (pid == 0) _exit(shell());
    int st = 0;
    if (pid < 0 || waitpid(pid, &st, 0) != pid) return (void *)1L;
    return (void *)(long)(WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
}

int main(int argc, char **argv)
{
    rounds = argc > 1 ? atoi(argv[1]) : 6;
    setvbuf(stdout, NULL, _IONBF, 0);
    pthread_t a, b;
    pthread_create(&a, NULL, idle, NULL);
    pthread_create(&b, NULL, start_shell, NULL);
    void *r;
    pthread_join(b, &r);
    atomic_store(&stop, 1);
    pthread_join(a, NULL);
    if (r) {
        printf("shell: status %#lx\n", (long)r);
        return 1;
    }
    printf("fork-twice-thread ok %d\n", rounds);
    return 0;
}
