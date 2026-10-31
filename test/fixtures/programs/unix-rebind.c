/* A unix socket's name, unlinked, can be bound again: by a new socket after
 * the first is closed, and while the first is still open (the name is the
 * file's, not the socket's). wineserver unlinks a stale socket and binds
 * its own; with the name still taken it says "couldn't bind to the socket
 * even though we hold the lock". Prints one line per case, then "ok". */
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <errno.h>

static int bound(const char *path) {
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strcpy(a.sun_path, path);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s < 0 || bind(s, (struct sockaddr *)&a, sizeof a) || listen(s, 1)) {
        printf("bind %s: %s\n", path, strerror(errno));
        return -1;
    }
    return s;
}

int main(void) {
    int bad = 0;
    unlink("/tmp/rb1");
    int a = bound("/tmp/rb1");
    close(a);
    unlink("/tmp/rb1");
    int b = bound("/tmp/rb1");
    printf("closed, unlinked, bound again: %s\n", b >= 0 ? "yes" : "no");
    bad += b < 0;
    unlink("/tmp/rb1");
    int c = bound("/tmp/rb1");
    printf("open, unlinked, bound again: %s\n", c >= 0 ? "yes" : "no");
    bad += c < 0;
    if (b >= 0) close(b);
    if (c >= 0) close(c);
    puts(bad ? "FAIL" : "ok");
    return bad;
}
