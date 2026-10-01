/* Who sent a signal, as its siginfo says: kill() is SI_USER with the
 * sender's pid and uid, tgkill() (and raise(), which is tkill) is SI_TKILL,
 * a signal from another process names that process, and a write to a pipe
 * with no reader raises SIGPIPE as SI_USER from the writer itself. The same
 * fields through sigtimedwait. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile int code, pid_ok, uid_ok, got;
static volatile pid_t expect;

static void on_sig(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    got = sig;
    code = si->si_code;
    pid_ok = si->si_pid == expect;
    uid_ok = si->si_uid == getuid();
}

static void report(const char *what) {
    printf("%s: signal %d, code %d, pid ok %d, uid ok %d\n", what, got, code, pid_ok, uid_ok);
    got = code = pid_ok = uid_ok = 0;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_sig;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, 0);
    sigaction(SIGUSR2, &sa, 0);
    sigaction(SIGPIPE, &sa, 0);

    expect = getpid();
    kill(getpid(), SIGUSR1);
    report("kill self");
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR2);
    report("tgkill self");
    raise(SIGUSR1);
    report("raise");

    int p[2];
    pipe(p);
    close(p[0]);
    write(p[1], "x", 1);
    report("broken pipe");

    /* From a child: its pid, not ours. */
    sigset_t one;
    sigemptyset(&one);
    sigaddset(&one, SIGUSR2);
    sigprocmask(SIG_BLOCK, &one, 0);
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) {
        kill(parent, SIGUSR2);
        _exit(0);
    }
    waitpid(child, 0, 0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    struct timespec zero = {0, 0};
    int s = sigtimedwait(&one, &si, &zero);
    printf("sigtimedwait from child: signal %d, code %d, pid ok %d, uid ok %d\n", s, si.si_code,
           si.si_pid == child, si.si_uid == getuid());
    return 0;
}
