/* A guest that serves a unix socket, so the host can connect into it. Checks
 * both that the host can reach a socket the *guest* serves, and that a
 * blocking `accept` actually blocks rather than returning EAGAIN. */
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
static void put(const char *s) { write(1, s, strlen(s)); }
int main(void) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strcpy(a.sun_path, "/tmp/echo");
    unlink("/tmp/echo");
    if (bind(fd, (struct sockaddr *)&a, sizeof a) != 0) { put("bind failed\n"); return 1; }
    if (listen(fd, 4) != 0) { put("listen failed\n"); return 1; }
    put("server: listening\n");
    int c = accept(fd, NULL, NULL);
    if (c < 0) { put("server: accept failed\n"); return 1; }
    put("server: a client arrived\n");
    char buf[64];
    ssize_t n = read(c, buf, sizeof buf - 1);
    if (n <= 0) { put("server: nothing to read\n"); return 1; }
    buf[n] = 0;
    put("server: got ");
    put(buf);
    put("\n");
    /* Shout it back, uppercased, so the far end can tell it round-tripped. */
    for (ssize_t i = 0; i < n; i++) {
        if (buf[i] >= 'a' && buf[i] <= 'z') buf[i] -= 32;
    }
    write(c, buf, n);
    put("server: replied\n");
    return 0;
}
