/* A guest that connects to a unix socket and expects an answer. The other end
 * is a different guest in another Worker; the host bridges the two handles.
 * The first thing to cross the Worker-per-process seam. */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>

#define PATH "/tmp/relay"

int main(void) {
    /* Look before connecting, the way Xlib does: a host-bound socket must be
     * visible to `stat`, not only reachable by `connect`. */
    struct stat st;
    if (stat(PATH, &st)) { perror("stat " PATH); return 1; }
    if (!S_ISSOCK(st.st_mode)) {
        fprintf(stderr, "%s is not a socket (mode %o)\n", PATH, st.st_mode);
        return 1;
    }
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) { perror("socket"); return 1; }
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    strcpy(a.sun_path, PATH);
    if (connect(fd, (struct sockaddr *)&a, sizeof a)) { perror("connect"); return 1; }
    const char *msg = "over here";
    if (write(fd, msg, strlen(msg)) < 0) { perror("write"); return 1; }
    char buf[64];
    ssize_t n = read(fd, buf, sizeof buf - 1);
    if (n <= 0) { perror("read"); return 1; }
    buf[n] = 0;
    printf("client got: %s\n", buf);
    return 0;
}
