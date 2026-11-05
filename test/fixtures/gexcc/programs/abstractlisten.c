/* A listening unix socket stays listening, and keeps its name, until its
 * last descriptor goes: a forked child closing its inherited copy, or an
 * execve closing a close-on-exec one, leaves the parent's listener working.
 * For an abstract name (ksmserver's ICE socket, which forks a command per
 * client) and a path name alike. The last close gives an abstract name up:
 * a connect is refused and the name can be bound again. One line per check,
 * compared with native. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(name, cond)                                          \
    do {                                                           \
        printf("%s %s\n", (cond) ? "ok  " : "FAIL", name);         \
        fflush(stdout);                                            \
    } while (0)

static socklen_t addr(struct sockaddr_un *a, int abstract, const char *name)
{
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    if (!abstract) {
        snprintf(a->sun_path, sizeof a->sun_path, "%s", name);
        return sizeof *a;
    }
    snprintf(a->sun_path + 1, sizeof a->sun_path - 1, "%s", name);
    return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
}

static int listener(int abstract, const char *name, int flags)
{
    struct sockaddr_un a;
    socklen_t l = addr(&a, abstract, name);
    int s = socket(AF_UNIX, SOCK_STREAM | flags, 0);
    if (s < 0 || bind(s, (void *)&a, l) != 0 || listen(s, 8) != 0)
        return -1;
    return s;
}

/* A connect to the name, and the listener handing the connection over. */
static int reaches(int s, int abstract, const char *name)
{
    struct sockaddr_un a;
    socklen_t l = addr(&a, abstract, name);
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    if (connect(c, (void *)&a, l) != 0) {
        close(c);
        return 0;
    }
    int x = accept(s, NULL, NULL);
    char b = 'x';
    int ok = x >= 0 && write(c, &b, 1) == 1 && read(x, &b, 1) == 1 && b == 'x';
    close(x);
    close(c);
    return ok;
}

static void kinds(int abstract, const char *self)
{
    char name[64], what[128];
    const char *k = abstract ? "abstract" : "path";
    snprintf(name, sizeof name, "%s/abstractlisten-%d", abstract ? "@test" : "/tmp", (int)getpid());
    if (!abstract)
        unlink(name);
    int s = listener(abstract, name, 0);
    snprintf(what, sizeof what, "%s: listening", k);
    CHECK(what, s >= 0 && reaches(s, abstract, name));

    /* a child that closes its copy and exits */
    pid_t ch = fork();
    if (ch == 0) {
        close(s);
        _exit(0);
    }
    waitpid(ch, NULL, 0);
    snprintf(what, sizeof what, "%s: after a child closed its copy", k);
    CHECK(what, reaches(s, abstract, name));

    /* a child that exits holding its copy */
    ch = fork();
    if (ch == 0)
        _exit(0);
    waitpid(ch, NULL, 0);
    snprintf(what, sizeof what, "%s: after a child exited with its copy", k);
    CHECK(what, reaches(s, abstract, name));

    /* a dup closed in this process */
    close(dup(s));
    snprintf(what, sizeof what, "%s: after a dup was closed", k);
    CHECK(what, reaches(s, abstract, name));

    /* a close-on-exec copy, closed by a child's execve */
    int e = fcntl(s, F_DUPFD_CLOEXEC, 0);
    ch = fork();
    if (ch == 0) {
        close(s);
        execl(self, self, "exit", (char *)NULL);
        /* Where the program cannot reach its own file (a gexcc program
         * under gexc runs with / as its directory), exit holding the copy. */
        _exit(errno == ENOENT ? 0 : 127);
    }
    int st = 0;
    waitpid(ch, &st, 0);
    close(e);
    snprintf(what, sizeof what, "%s: after an execve closed a close-on-exec copy", k);
    CHECK(what, WIFEXITED(st) && WEXITSTATUS(st) == 0 && reaches(s, abstract, name));

    /* the last descriptor */
    close(s);
    struct sockaddr_un a;
    socklen_t l = addr(&a, abstract, name);
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    int r = connect(c, (void *)&a, l);
    int err = errno;
    close(c);
    snprintf(what, sizeof what, "%s: after the last close, a connect is refused", k);
    CHECK(what, r != 0 && err == ECONNREFUSED);
    if (abstract) {
        s = listener(abstract, name, 0);
        CHECK("abstract: the name binds again", s >= 0 && reaches(s, abstract, name));
        close(s);
    } else {
        unlink(name);
    }
}

int main(int argc, char **argv)
{
    if (argc > 1 && strcmp(argv[1], "exit") == 0)
        return 0;
    setvbuf(stdout, NULL, _IONBF, 0);
    kinds(1, argv[0]);
    kinds(0, argv[0]);
    return 0;
}
