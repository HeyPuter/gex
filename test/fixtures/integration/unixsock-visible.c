/* A UNIX socket bound to a path has to exist in the filesystem too.
 *
 * Binding registers the name where `connect` finds it. Everything else looks
 * in the filesystem: `stat` for S_ISSOCK, `test -e` in a shell, and the wait
 * loop that every X client runs before it tries the display. gex had only the
 * first, so Xvfb sat listening on /tmp/.X11-unix/X0 while nothing could tell
 * it was there.
 */
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

int main(void) {
    const char *path = "/tmp/visible.sock";
    unlink(path);
    struct stat st;
    printf("before bind: %s\n", stat(path, &st) == 0 ? "present" : "absent");

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a;
    memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    strcpy(a.sun_path, path);
    if (bind(fd, (struct sockaddr *)&a, sizeof a) != 0) {
        printf("bind failed\n");
        return 1;
    }
    if (stat(path, &st) != 0) {
        printf("after bind: absent\n");
        return 1;
    }
    printf("after bind: present, socket=%d\n", S_ISSOCK(st.st_mode) ? 1 : 0);

    /* And a client still finds it, which is the part that already worked. */
    if (listen(fd, 4) != 0) { printf("listen failed\n"); return 1; }
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    printf("connect %s\n", connect(c, (struct sockaddr *)&a, sizeof a) == 0 ? "ok" : "failed");

    /* Unlinking removes the name, as it does on Linux. */
    unlink(path);
    printf("after unlink: %s\n", stat(path, &st) == 0 ? "present" : "absent");
    close(c);
    close(fd);
    return 0;
}
