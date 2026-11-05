/* Processes that leave state behind, and the processes after them.
 *
 * The page gives a process's Worker back to its pool when the process exits
 * cleanly, and the next process starts in that realm (host.mjs `#giveBack`,
 * worker.mjs `reset`). Nothing of the one before may reach it. Each round:
 *
 *   leave  sets an interval timer, a SIGALRM and a SIGUSR1 handler, a umask
 *          and a cwd, and exits with the timer still running;
 *   check  sleeps past several of those timer periods without being
 *          interrupted, and finds every one of them at its default;
 *   reader leaves a thread blocked reading its standard input, a pipe, and
 *          exits around it;
 *   readcheck reads that same pipe: the line written after the reader is
 *          gone must reach it, not a read left over from the reader.
 *
 * Started every way a process starts here: posix_spawn (run in its parent's
 * Worker until its exec), vfork + exec, fork + exec, and an exec from a
 * second thread (Linux's de_thread). Prints "reuse-state ok N". */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;
static char self[4096];
static char home[4096];

static void on_signal(int sig) { (void)sig; }

static int leave(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigaction(SIGALRM, &sa, NULL);
    sigaction(SIGUSR1, &sa, NULL);
    struct itimerval it = { { 0, 20000 }, { 0, 20000 } };
    setitimer(ITIMER_REAL, &it, NULL);
    umask(077);
    if (chdir("/") != 0) return 2;
    return 0;
}

static int check(const char *cwd)
{
    /* Past a dozen of `leave`'s timer periods: a timer of its, delivered
     * here, ends this process (SIGALRM's default) or interrupts the sleep. */
    struct timespec t = { 0, 300 * 1000 * 1000 };
    if (nanosleep(&t, NULL) != 0) { printf("check: sleep interrupted (%d)\n", errno); return 1; }
    struct sigaction sa;
    sigaction(SIGALRM, NULL, &sa);
    if (sa.sa_handler != SIG_DFL) { printf("check: SIGALRM handled\n"); return 1; }
    sigaction(SIGUSR1, NULL, &sa);
    if (sa.sa_handler != SIG_DFL) { printf("check: SIGUSR1 handled\n"); return 1; }
    struct itimerval it;
    getitimer(ITIMER_REAL, &it);
    if (it.it_value.tv_sec || it.it_value.tv_usec) { printf("check: a timer runs\n"); return 1; }
    mode_t m = umask(022);
    if (m != 022) { printf("check: umask %o\n", m); return 1; }
    char here[4096];
    if (!getcwd(here, sizeof here) || strcmp(here, cwd)) { printf("check: cwd %s\n", here); return 1; }
    return 0;
}

static void *block_read(void *arg)
{
    char c;
    (void)arg;
    /* Never returns: the process exits around it. */
    for (;;) if (read(0, &c, 1) <= 0) pause();
    return NULL;
}

static int reader(void)
{
    pthread_t t;
    pthread_create(&t, NULL, block_read, NULL);
    /* Long enough for the thread to be parked in its read. */
    struct timespec ts = { 0, 50 * 1000 * 1000 };
    nanosleep(&ts, NULL);
    exit(0);
}

static int readcheck(void)
{
    char buf[16];
    ssize_t n = read(0, buf, sizeof buf - 1);
    if (n != 5 || memcmp(buf, "data\n", 5)) { printf("readcheck: read %zd\n", n); return 1; }
    return 0;
}

static void *exec_check(void *arg)
{
    (void)arg;
    execl(self, self, "check", home, (char *)NULL);
    _exit(126);
}

static int dethread(void)
{
    pthread_t t;
    pthread_create(&t, NULL, exec_check, NULL);
    pthread_join(t, NULL);
    return 3;
}

/* Wait for `pid`, which must exit 0. */
static int reap(pid_t pid, const char *what, int round)
{
    int st;
    if (pid < 0) { printf("%s %d: could not start (%d)\n", what, round, errno); return 1; }
    if (waitpid(pid, &st, 0) != pid) { printf("%s %d: waitpid %d\n", what, round, errno); return 1; }
    if (!WIFEXITED(st) || WEXITSTATUS(st)) { printf("%s %d: status %#x\n", what, round, st); return 1; }
    return 0;
}

static pid_t spawn(const char *mode, int in)
{
    posix_spawn_file_actions_t fa;
    posix_spawn_file_actions_init(&fa);
    if (in >= 0) posix_spawn_file_actions_adddup2(&fa, in, 0);
    char *argv[] = { self, (char *)mode, home, NULL };
    pid_t pid;
    int rc = posix_spawn(&pid, self, &fa, NULL, argv, environ);
    posix_spawn_file_actions_destroy(&fa);
    if (rc) { errno = rc; return -1; }
    return pid;
}

static pid_t start_vfork(const char *mode)
{
    pid_t pid = vfork();
    if (pid == 0) {
        execl(self, self, mode, home, (char *)NULL);
        _exit(127);
    }
    return pid;
}

static pid_t start_fork(const char *mode)
{
    pid_t pid = fork();
    if (pid == 0) {
        execl(self, self, mode, home, (char *)NULL);
        _exit(127);
    }
    return pid;
}

int main(int argc, char **argv)
{
    if (readlink("/proc/self/exe", self, sizeof self - 1) <= 0) strcpy(self, argv[0]);
    if (argc > 2) snprintf(home, sizeof home, "%s", argv[2]);
    if (argc > 2 && !strcmp(argv[1], "leave")) return leave();
    if (argc > 2 && !strcmp(argv[1], "check")) return check(argv[2]);
    if (argc > 2 && !strcmp(argv[1], "reader")) return reader();
    if (argc > 2 && !strcmp(argv[1], "readcheck")) return readcheck();
    if (argc > 2 && !strcmp(argv[1], "dethread")) return dethread();

    int rounds = argc > 1 ? atoi(argv[1]) : 8;
    if (!getcwd(home, sizeof home)) return 1;
    umask(022);
    setvbuf(stdout, NULL, _IONBF, 0);
    int bad = 0;
    for (int i = 1; i <= rounds && !bad; i++) {
        bad |= reap(spawn("leave", -1), "spawn leave", i);
        bad |= reap(spawn("check", -1), "spawn check", i);
        bad |= reap(start_vfork("leave"), "vfork leave", i);
        bad |= reap(start_vfork("check"), "vfork check", i);
        bad |= reap(start_fork("leave"), "fork leave", i);
        bad |= reap(start_fork("check"), "fork check", i);
        bad |= reap(spawn("dethread", -1), "dethread", i);
        int p[2];
        if (pipe(p)) return 1;
        bad |= reap(spawn("reader", p[0]), "reader", i);
        pid_t rc = spawn("readcheck", p[0]);
        if (write(p[1], "data\n", 5) != 5) bad = 1;
        bad |= reap(rc, "readcheck", i);
        close(p[0]);
        close(p[1]);
    }
    if (bad) return 1;
    printf("reuse-state ok %d\n", rounds);
    return 0;
}
