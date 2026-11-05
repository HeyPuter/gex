/* UDP over the loopback between guests: a server bound to 127.0.0.1 (an
 * ephemeral port), a client that sends unbound (and so gets a port), the
 * reply to the address recvfrom reported, a connected socket's send, a
 * datagram from a forked child, and EADDRINUSE on the bound port. Prints
 * nothing that depends on which ports. */
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    int srv = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = 0, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    printf("bind %d\n", bind(srv, (struct sockaddr *)&a, sizeof a));
    struct sockaddr_in where;
    socklen_t wl = sizeof where;
    getsockname(srv, (struct sockaddr *)&where, &wl);
    printf("bound port nonzero %d\n", where.sin_port != 0);
    int dup = socket(AF_INET, SOCK_DGRAM, 0);
    int r = bind(dup, (struct sockaddr *)&where, sizeof where);
    printf("second bind: %d %s\n", r, r && errno == EADDRINUSE ? "EADDRINUSE" : "");
    close(dup);

    int cli = socket(AF_INET, SOCK_DGRAM, 0);
    printf("sendto %zd\n", sendto(cli, "ping", 4, 0, (struct sockaddr *)&where, sizeof where));
    struct sockaddr_in me;
    socklen_t ml = sizeof me;
    getsockname(cli, (struct sockaddr *)&me, &ml);
    char buf[64] = {0};
    struct sockaddr_in from;
    socklen_t fl = sizeof from;
    ssize_t n = recvfrom(srv, buf, sizeof buf - 1, 0, (struct sockaddr *)&from, &fl);
    printf("server got %zd [%s] from 127.0.0.1 %d, the client's port %d\n", n, buf, from.sin_addr.s_addr == htonl(INADDR_LOOPBACK), from.sin_port == me.sin_port);
    sendto(srv, "pong", 4, 0, (struct sockaddr *)&from, fl);
    memset(buf, 0, sizeof buf);
    n = recv(cli, buf, sizeof buf - 1, 0);
    printf("client got %zd [%s]\n", n, buf);

    int con = socket(AF_INET, SOCK_DGRAM, 0);
    printf("connect %d\n", connect(con, (struct sockaddr *)&where, sizeof where));
    printf("send %zd\n", send(con, "connected", 9, 0));
    memset(buf, 0, sizeof buf);
    n = recv(srv, buf, sizeof buf - 1, 0);
    printf("server got %zd [%s]\n", n, buf);

    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        int k = socket(AF_INET, SOCK_DGRAM, 0);
        sendto(k, "from the child", 14, 0, (struct sockaddr *)&where, sizeof where);
        _exit(0);
    }
    memset(buf, 0, sizeof buf);
    n = recv(srv, buf, sizeof buf - 1, 0);
    printf("server got %zd [%s]\n", n, buf);
    int st;
    waitpid(c, &st, 0);
    return 0;
}
