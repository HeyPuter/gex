/* Loopback TCP and UDP the way Wine's server uses them: bind to port 0,
 * getsockname for the port, listen, a non-blocking connect completed by
 * poll, accept, send and recv, shutdown; a UDP datagram. */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int fails;
static void ok(int c, const char *what) { printf("%s %s\n", c ? "ok" : "FAIL", what); if (!c) fails++; }

int main(void)
{
    struct sockaddr_in a = {0}, b = {0};
    socklen_t len = sizeof a;
    char buf[16] = {0};
    int l, c, s, u1, u2, r, err = -1;
    struct pollfd p;

    setvbuf(stdout, 0, _IONBF, 0);
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    l = socket(AF_INET, SOCK_STREAM, 0);
    ok(bind(l, (struct sockaddr *)&a, sizeof a) == 0, "bind 127.0.0.1:0");
    ok(getsockname(l, (struct sockaddr *)&a, &len) == 0 && a.sin_port != 0, "getsockname gives the port");
    ok(listen(l, 4) == 0, "listen");
    c = socket(AF_INET, SOCK_STREAM, 0);
    fcntl(c, F_SETFL, O_NONBLOCK);
    r = connect(c, (struct sockaddr *)&a, sizeof a);
    ok(r == 0 || errno == EINPROGRESS, "a non-blocking connect starts");
    p.fd = c; p.events = POLLOUT;
    ok(poll(&p, 1, 5000) == 1, "poll: the connect completes");
    len = sizeof err;
    ok(getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &len) == 0 && err == 0, "SO_ERROR 0");
    len = sizeof b;
    s = accept(l, (struct sockaddr *)&b, &len);
    ok(s >= 0 && b.sin_addr.s_addr == htonl(INADDR_LOOPBACK), "accept, from 127.0.0.1");
    len = sizeof b;
    ok(getpeername(c, (struct sockaddr *)&b, &len) == 0 && b.sin_port == a.sin_port, "getpeername is the listener");
    ok(send(c, "hello", 5, 0) == 5, "send");
    p.fd = s; p.events = POLLIN;
    ok(poll(&p, 1, 5000) == 1, "poll: readable");
    ok(recv(s, buf, sizeof buf, 0) == 5 && !memcmp(buf, "hello", 5), "recv");
    ok(shutdown(c, SHUT_WR) == 0, "shutdown");
    ok(recv(s, buf, sizeof buf, 0) == 0, "recv sees the end");
    close(c); close(s); close(l);

    u1 = socket(AF_INET, SOCK_DGRAM, 0);
    u2 = socket(AF_INET, SOCK_DGRAM, 0);
    a.sin_port = 0; len = sizeof a;
    ok(bind(u1, (struct sockaddr *)&a, sizeof a) == 0 && getsockname(u1, (struct sockaddr *)&a, &len) == 0 && a.sin_port, "UDP bound, with its port");
    ok(sendto(u2, "dgram", 5, 0, (struct sockaddr *)&a, sizeof a) == 5, "sendto");
    p.fd = u1; p.events = POLLIN;
    ok(poll(&p, 1, 5000) == 1, "poll: a datagram");
    len = sizeof b;
    ok(recvfrom(u1, buf, sizeof buf, 0, (struct sockaddr *)&b, &len) == 5 && !memcmp(buf, "dgram", 5), "recvfrom");
    close(u1); close(u2);
    printf("%d failures\n", fails);
    return fails != 0;
}
