/* A guest that connects to a unix socket and expects an answer.
 *
 * The other end is not in this process, and not in this Worker: it is a
 * different guest with its own kernel and its own filesystem. What joins them
 * is the host -- `gex_host_bind` serves the path here, `gex_host_connect`
 * reaches into the server's Worker, and JavaScript moves bytes between the
 * two handles.
 *
 * That is the whole seam a Worker-per-process design rests on, and this is
 * the first thing to cross it.
 */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>

#define PATH "/tmp/relay"

int main(void) {
    /* Look before connecting, the way Xlib does. A host-bound socket lived
     * only in the name table for a while, so `connect` found it and `stat`
     * did not -- which is invisible to anything that connects blind and fatal
     * to anything that checks. Checking here keeps that from coming back. */
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
