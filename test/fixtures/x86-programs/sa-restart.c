/* SA_RESTART (signal(7), "Interruption of system calls and library
 * functions by signal handlers"). A SIGALRM handler runs while the call
 * waits:
 *   - installed with SA_RESTART, wait4, a pipe read and an untimed futex
 *     wait carry on after the handler and answer what they waited for;
 *   - without it, wait4 says EINTR;
 *   - poll and nanosleep say EINTR even with SA_RESTART.
 * gex answered EINTR for every one of them (a shell's waitpid saw it).
 * The output is the same natively.
 *
 * The alarm is a fixed 100 ms ahead, so on a loaded machine it can fire
 * before the call waits; the handler notes whether the call had begun
 * (`in_call`), and a case whose alarm came early is made again (five times
 * at most). */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t got, in_call, early;
static void on_alarm(int s) {
    (void)s;
    got++;
    if (!in_call) early = 1;
}

/* Before a case's attempt: no handler yet, the call not begun. */
static void attempt(void) {
    got = 0;
    in_call = 0;
    early = 0;
}

/* Whether to make the case again: its alarm came before its call. */
static int again(int *tries) {
    in_call = 0;
    return early && ++*tries < 5;
}

static void handler(int restart) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sa.sa_flags = restart ? SA_RESTART : 0;
    sigaction(SIGALRM, &sa, 0);
}

/* SIGALRM in 100 ms, once. */
static void alarm_soon(void) {
    struct itimerval it = {{0, 0}, {0, 100000}};
    setitimer(ITIMER_REAL, &it, 0);
}

static void ms(int n) {
    struct timespec d = {n / 1000, (n % 1000) * 1000000L};
    while (nanosleep(&d, &d) && errno == EINTR) {}
}

static int fail;
static void report(const char *what, int ok, long r, int e) {
    printf("%s: %s (r=%ld%s%s, handler %d)\n", what, ok ? "ok" : "FAIL", r, r < 0 ? " " : "", r < 0 ? strerror(e) : "",
           (int)got);
    fail |= !ok;
}

static void case_wait4(int restart) {
    handler(restart);
    pid_t c, r;
    int st, e, tries = 0;
    do {
        attempt();
        c = fork();
        if (c == 0) {
            ms(400);
            _exit(7);
        }
        alarm_soon();
        st = 0;
        in_call = 1;
        r = waitpid(c, &st, 0);
        e = errno;
        if (r != c) waitpid(c, 0, 0);
    } while (again(&tries));
    /* The child's pid differs between runs: 1 for it. */
    if (restart) {
        report("wait4, SA_RESTART", r == c && WIFEXITED(st) && WEXITSTATUS(st) == 7 && got == 1, r == c ? 1 : r, e);
    } else {
        report("wait4, no SA_RESTART", r == -1 && e == EINTR && got == 1, r, e);
    }
}

static void case_read(void) {
    handler(1);
    long r;
    int e, tries = 0;
    char ch;
    do {
        attempt();
        int p[2];
        if (pipe(p)) return;
        pid_t c = fork();
        if (c == 0) {
            ms(400);
            if (write(p[1], "x", 1) != 1) _exit(1);
            _exit(0);
        }
        close(p[1]);
        alarm_soon();
        ch = 0;
        in_call = 1;
        r = read(p[0], &ch, 1);
        e = errno;
        close(p[0]);
        waitpid(c, 0, 0);
    } while (again(&tries));
    report("read, SA_RESTART", r == 1 && ch == 'x' && got == 1, r, e);
}

static int word;
static void *waker(void *arg) {
    (void)arg;
    /* The alarm is for the waiting thread. */
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGALRM);
    pthread_sigmask(SIG_BLOCK, &s, 0);
    ms(400);
    __atomic_store_n(&word, 1, __ATOMIC_SEQ_CST);
    syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0);
    return 0;
}

static void case_futex(void) {
    handler(1);
    long r;
    int e, tries = 0;
    do {
        attempt();
        word = 0;
        pthread_t t;
        pthread_create(&t, 0, waker, 0);
        alarm_soon();
        /* One call: woken by the waker (0) once the handler has run, or by
         * a restart that finds the word already changed (EAGAIN). EINTR is
         * the bug. */
        in_call = 1;
        r = syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0);
        e = errno;
        pthread_join(t, 0);
    } while (again(&tries));
    report("futex wait, SA_RESTART", (r == 0 || (r == -1 && e == EAGAIN)) && got == 1, r, e);
}

static void case_poll(void) {
    handler(1);
    long r;
    int e, tries = 0;
    do {
        attempt();
        int p[2];
        if (pipe(p)) return;
        alarm_soon();
        struct pollfd f = {.fd = p[0], .events = POLLIN};
        in_call = 1;
        r = poll(&f, 1, 2000);
        e = errno;
        close(p[0]);
        close(p[1]);
    } while (again(&tries));
    report("poll, SA_RESTART", r == -1 && e == EINTR && got == 1, r, e);
}

static void case_nanosleep(void) {
    handler(1);
    long r;
    int e, tries = 0;
    do {
        attempt();
        alarm_soon();
        struct timespec d = {2, 0};
        in_call = 1;
        r = nanosleep(&d, &d);
        e = errno;
    } while (again(&tries));
    report("nanosleep, SA_RESTART", r == -1 && e == EINTR && got == 1, r, e);
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    case_wait4(1);
    case_wait4(0);
    case_read();
    case_futex();
    case_poll();
    case_nanosleep();
    return fail;
}
