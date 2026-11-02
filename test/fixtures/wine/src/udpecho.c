/* Winsock UDP to a far address and back, against the page's echoing network
 * (test/wine/udp.test.js: each datagram comes back prefixed with where it
 * went): sendto and recvfrom on an unbound socket, then send and recv on a
 * connected one. Prints one line per check.
 * gex-nt-arch: x64
 * gex-nt-libs: -lws2_32 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

/* A datagram within 10 s: its length, or -1. */
static int wait_recv(SOCKET s, char *buf, int len, struct sockaddr_in *from) {
    fd_set r;
    struct timeval tv = {10, 0};
    FD_ZERO(&r);
    FD_SET(s, &r);
    if (select(0, &r, NULL, NULL, &tv) != 1) return -1;
    int fl = sizeof *from;
    return from ? recvfrom(s, buf, len, 0, (struct sockaddr *)from, &fl) : recv(s, buf, len, 0);
}

int main(void) {
    WSADATA wsa;
    CHECK("WSAStartup", WSAStartup(MAKEWORD(2, 2), &wsa) == 0);
    struct sockaddr_in to = {0};
    to.sin_family = AF_INET;
    to.sin_port = htons(7);
    to.sin_addr.s_addr = inet_addr("10.1.2.3");
    char buf[256];

    SOCKET s = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    CHECK("socket", s != INVALID_SOCKET);
    CHECK("sendto", sendto(s, "wine-udp", 8, 0, (struct sockaddr *)&to, sizeof to) == 8);
    struct sockaddr_in from = {0};
    int n = wait_recv(s, buf, sizeof buf - 1, &from);
    buf[n > 0 ? n : 0] = 0;
    CHECK("recvfrom: the echo", n > 0 && !strcmp(buf, "10.1.2.3:7 wine-udp"));
    CHECK("recvfrom: from where it went", from.sin_addr.s_addr == to.sin_addr.s_addr && from.sin_port == to.sin_port);
    closesocket(s);

    SOCKET c = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    to.sin_port = htons(9);
    CHECK("connect", connect(c, (struct sockaddr *)&to, sizeof to) == 0);
    CHECK("send", send(c, "connected", 9, 0) == 9);
    n = wait_recv(c, buf, sizeof buf - 1, NULL);
    buf[n > 0 ? n : 0] = 0;
    CHECK("recv: the echo", n > 0 && !strcmp(buf, "10.1.2.3:9 connected"));
    closesocket(c);
    WSACleanup();
    printf("%d failures\n", failures);
    return failures;
}
