/* A SIGCHLD handler that reaps with waitpid(WNOHANG) finds the child that
 * sent it collectable: an exited child's status is there before its
 * parent's SIGCHLD, as POSIX has it. dbus-daemon's activation helper works
 * so: it forks the program, and on each SIGCHLD (through a self-pipe) asks
 * waitpid(WNOHANG) once; a SIGCHLD that comes first and a 0 leave it
 * waiting for a signal that has already come.
 * Several workers babysit at once (one process tree, no process storm),
 * each child execs /bin/false where there is one (an exec, as dbus's
 * children do, makes the race far likelier under gex), and a wait that goes quiet for
 * 10 s is reported, not waited out. One line per worker. */
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

#define WORKERS 6
#define ROUNDS 40

static int sp[2];
static void on_chld(int s) { (void)s; char c = 1; (void)!write(sp[1], &c, 1); }

/* one child reaped as dbus does; 0 when its status was there, 1 when its
 * SIGCHLD came with nothing to reap and no other followed */
static int babysit(void)
{
    pid_t g = fork();
    if (g == 0) {
        execl("/bin/false", "false", (char *)0);
        /* no /bin/false (the browser runtime's guest has only the
         * program): the same status without the exec */
        _exit(1);
    }
    int st;
    pid_t r = waitpid(g, &st, WNOHANG);
    while (r == 0) {
        struct pollfd p = { .fd = sp[0], .events = POLLIN };
        int n = poll(&p, 1, 10000);
        if (n < 0 && errno == EINTR) continue;
        if (n == 0) {
            waitpid(g, &st, 0);
            return 1;
        }
        char c;
        (void)!read(sp[0], &c, 1);
        do r = waitpid(g, &st, WNOHANG); while (r < 0 && errno == EINTR);
    }
    return r == g && WIFEXITED(st) && WEXITSTATUS(st) == 1 ? 0 : 2;
}

static int worker(void)
{
    if (pipe(sp)) return 2;
    struct sigaction sa = { .sa_handler = on_chld, .sa_flags = SA_RESTART };
    sigaction(SIGCHLD, &sa, NULL);
    int lost = 0, bad = 0;
    for (int k = 0; k < ROUNDS; k++) {
        int e = babysit();
        lost += e == 1;
        bad += e == 2;
    }
    if (lost || bad) {
        printf("worker: %d of %d SIGCHLDs came before their child's status, %d bad\n", lost, ROUNDS, bad);
        return 1;
    }
    printf("worker: %d children reaped on their SIGCHLD\n", ROUNDS);
    return 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    pid_t w[WORKERS];
    for (int i = 0; i < WORKERS; i++) {
        w[i] = fork();
        if (w[i] == 0) _exit(worker());
    }
    int fail = 0;
    for (int i = 0; i < WORKERS; i++) {
        int st;
        waitpid(w[i], &st, 0);
        fail |= !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    return fail;
}
