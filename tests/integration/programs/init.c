/* An init running as pid 1: starts services (fork/exec/wait), gives each its
 * own session, adopts and reaps orphans, and signals a whole process group.
 *
 * The transcript is fixed, so every pid is a claim: pid 1 first, the rest in
 * order. Children report through exit status, not stdout, so output order does
 * not depend on scheduling. Not run against qemu, which cannot agree on pid 1.
 */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static char self[4096];

/* Re-exec this program in a given role. Separate execs, not plain forks: exec
   is what an init does, and gives each role a fresh translation. */
static pid_t spawn(const char *role) {
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        exit(1);
    }
    if (pid == 0) {
        execl(self, self, role, (char *)NULL);
        perror("execl");
        _exit(127);
    }
    return pid;
}

static void nap(long ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
}

/* Wait for one child and report it as "<pid> exited <status>", or as the
   signal that killed it. */
static void reap(const char *what) {
    int status = 0;
    pid_t pid = wait(&status);
    if (pid < 0) {
        printf("%s: wait failed: %s\n", what, strerror(errno));
        return;
    }
    if (WIFEXITED(status)) {
        printf("%s: pid %d exited %d\n", what, (int)pid, WEXITSTATUS(status));
    } else if (WIFSIGNALED(status)) {
        printf("%s: pid %d killed by %d\n", what, (int)pid, WTERMSIG(status));
    } else {
        printf("%s: pid %d ended oddly (%#x)\n", what, (int)pid, status);
    }
}

/* -- the roles ----------------------------------------------------------- */

/* A service: its own session, and it leaves a worker running behind it.
   Exits with 0 if setsid worked and it really did become the leader. */
static int role_service(void) {
    pid_t sid = setsid();
    int ok = sid == getpid() && getsid(0) == getpid() && getpgid(0) == getpid();
    spawn("worker");
    /* Exiting here is the point: the worker becomes an orphan, and only a
       kernel that reparents it to init will let init reap it below. */
    return ok ? 0 : 1;
}

/* An orphan. Waits for its parent to be gone, then reports the parent it has
   been given. Exiting with the ppid is what makes this checkable without the
   orphan printing anything of its own. */
static int role_worker(void) {
    for (int i = 0; i < 500 && getppid() != 1; i++) {
        nap(10);
    }
    return (int)getppid();
}

/* A member of a process group that init will signal as a group. Handling the
   signal rather than dying from it keeps the exit status the same on any
   kernel: a death by signal is reported differently in places, an exit code
   is not. */
static volatile sig_atomic_t got_term;
static void on_term(int signo) {
    (void)signo;
    got_term = 1;
}

static int role_member(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_term;
    sigaction(SIGTERM, &sa, NULL);
    /* Join the group led by the first member, so init can name the whole set
       with one kill. The leader's pgid is its own pid, which is this
       process's pid when it is the first one. */
    const char *lead = getenv("GEX_LEAD");
    if (lead && *lead) {
        setpgid(0, atoi(lead));
    } else {
        setpgid(0, 0);
    }
    /* A signal sent by another process becomes pending, and is picked up the
       next time this one enters the kernel. So it has to keep entering. */
    for (int i = 0; i < 500 && !got_term; i++) {
        nap(10);
    }
    return got_term ? 42 : 1;
}

/* -- pid 1 --------------------------------------------------------------- */

static int role_init(void) {
    printf("init: pid=%d ppid=%d pgid=%d sid=%d\n", (int)getpid(), (int)getppid(),
           (int)getpgid(0), (int)getsid(0));
    /* init leads its own session already, and a second setsid must fail --
       succeeding would leave its group split across two sessions. */
    printf("init: setsid again -> %s\n",
           setsid() < 0 && errno == EPERM ? "EPERM" : "wrongly allowed");

    /* 1 and 2: a service in its own session, which orphans a worker. */
    pid_t svc = spawn("service");
    printf("service: pid %d\n", (int)svc);
    reap("service");
    /* 3: the worker's parent is gone, so this can only succeed if the worker
       was handed to init. Its status is the parent it found. */
    reap("adopted");

    /* 4: two processes in one group, stopped with one kill. */
    pid_t lead = spawn("member");
    char buf[32];
    snprintf(buf, sizeof buf, "%d", (int)lead);
    setenv("GEX_LEAD", buf, 1);
    pid_t second = spawn("member");
    printf("group: led by %d with %d\n", (int)lead, (int)second);
    /* Give both time to install a handler and join the group. Racing this
       would send the signal before the group exists. */
    for (int i = 0; i < 200; i++) {
        if (getpgid(second) == lead) {
            break;
        }
        nap(10);
    }
    printf("group: both joined -> %s\n",
           getpgid(lead) == lead && getpgid(second) == lead ? "yes" : "no");
    printf("group: kill(-%d, SIGTERM) -> %d\n", (int)lead, kill(-lead, SIGTERM));
    /* Two children signalled at once, reaped in whichever order they die.
       Collected and reported sorted, since arrival order is not fixed. */
    {
        int lines = 0;
        char seen[2][64];
        for (int i = 0; i < 2; i++) {
            int status = 0;
            pid_t pid = wait(&status);
            if (pid < 0) {
                snprintf(seen[lines++], sizeof seen[0], "group: wait failed: %s",
                         strerror(errno));
            } else if (WIFEXITED(status)) {
                snprintf(seen[lines++], sizeof seen[0], "group: pid %d exited %d",
                         (int)pid, WEXITSTATUS(status));
            } else {
                snprintf(seen[lines++], sizeof seen[0], "group: pid %d killed by %d",
                         (int)pid, WTERMSIG(status));
            }
        }
        if (lines == 2 && strcmp(seen[0], seen[1]) > 0) {
            char tmp[64];
            memcpy(tmp, seen[0], sizeof tmp);
            memcpy(seen[0], seen[1], sizeof tmp);
            memcpy(seen[1], tmp, sizeof tmp);
        }
        for (int i = 0; i < lines; i++) {
            printf("%s\n", seen[i]);
        }
    }

    /* Nobody left. */
    printf("init: wait with no children -> %s\n",
           wait(NULL) < 0 && errno == ECHILD ? "ECHILD" : "unexpected");
    printf("init: done\n");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    if (n <= 0) {
        snprintf(self, sizeof self, "%s", argv[0]);
    } else {
        self[n] = 0;
    }
    const char *role = argc > 1 ? argv[1] : "init";
    if (!strcmp(role, "service")) {
        return role_service();
    }
    if (!strcmp(role, "worker")) {
        return role_worker();
    }
    if (!strcmp(role, "member")) {
        return role_member();
    }
    return role_init();
}
