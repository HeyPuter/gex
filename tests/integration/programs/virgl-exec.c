/* The same two clients, but the second arrives through fork+exec.
 *
 * A plain fork already works. What the shell does is fork and then exec, and
 * that is the one difference left between the reproduction that passes and
 * glxinfo, which connects to gex's renderer and is never answered. */
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int handshake(const char *who) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a;
    memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    strcpy(a.sun_path, "/tmp/.gex-test-virgl");
    if (connect(fd, (struct sockaddr *)&a, sizeof a) != 0) {
        printf("%s: connect failed\n", who);
        fflush(stdout);
        return 1;
    }
    const char *name = "exec-client";
    unsigned hdr[2] = {(unsigned)strlen(name), 8};
    write(fd, hdr, 8);
    write(fd, name, strlen(name));
    unsigned ping[2] = {0, 10};
    write(fd, ping, 8);
    unsigned reply[2] = {0xffffffffu, 0xffffffffu};
    ssize_t n = read(fd, reply, 8);
    printf("%s: read %zd, len=%u cmd=%u\n", who, n, reply[0], reply[1]);
    fflush(stdout);
    close(fd);
    return n == 8 ? 0 : 1;
}

int main(int argc, char **argv) {
    if (argc > 1) return handshake("exec'd child");
    if (handshake("parent")) return 1;
    pid_t p = fork();
    if (p == 0) {
        execl(argv[0], argv[0], "child", (char *)0);
        printf("exec failed\n");
        fflush(stdout);
        _exit(127);
    }
    int st = 0;
    waitpid(p, &st, 0);
    printf("child exit %d\n", WEXITSTATUS(st));
    return 0;
}
