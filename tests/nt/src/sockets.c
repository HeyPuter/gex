/* Winsock over loopback: TCP (listen, connect, accept, send, recv, select,
 * a non-blocking connect, shutdown), UDP (sendto, recvfrom), name lookup
 * of localhost, and WSAEventSelect. What a game's networking (and a
 * launcher's single-instance port) starts with.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <stdio.h>
#include <string.h>
static int fails;
static void ok(int c, const char *what) { printf("%s %s\n", c ? "ok" : "FAIL", what); if (!c) { fails++; printf("  (WSAGetLastError %d)\n", WSAGetLastError()); } }
int main(void) {
    WSADATA wsa;
    struct sockaddr_in a = {0}, b = {0};
    int len = sizeof a, n;
    SOCKET l, c, s, u1, u2;
    char buf[64];
    u_long nb = 1;
    fd_set w, r;
    struct timeval tv = {5, 0};
    struct addrinfo *ai = 0, hints = {0};
    WSAEVENT ev;
    WSANETWORKEVENTS ne;

    setvbuf(stdout, 0, _IONBF, 0);
    ok(!WSAStartup(MAKEWORD(2, 2), &wsa), "WSAStartup 2.2");
    hints.ai_family = AF_INET;
    ok(!getaddrinfo("localhost", "80", &hints, &ai) && ai, "getaddrinfo localhost");
    if (ai) { ok(((struct sockaddr_in *)ai->ai_addr)->sin_addr.s_addr == htonl(INADDR_LOOPBACK), "localhost is 127.0.0.1"); freeaddrinfo(ai); }

    l = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    ok(l != INVALID_SOCKET, "a TCP socket");
    a.sin_family = AF_INET; a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ok(!bind(l, (struct sockaddr *)&a, sizeof a), "bind 127.0.0.1:0");
    ok(!getsockname(l, (struct sockaddr *)&a, &len) && a.sin_port, "getsockname gives a port");
    ok(!listen(l, 4), "listen");

    c = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    ok(!ioctlsocket(c, FIONBIO, &nb), "FIONBIO");
    n = connect(c, (struct sockaddr *)&a, sizeof a);
    ok(n == 0 || WSAGetLastError() == WSAEWOULDBLOCK, "a non-blocking connect starts");
    FD_ZERO(&w); FD_SET(c, &w);
    ok(select(0, 0, &w, 0, &tv) == 1, "select: the connect completes");
    len = sizeof b;
    s = accept(l, (struct sockaddr *)&b, &len);
    ok(s != INVALID_SOCKET && b.sin_addr.s_addr == htonl(INADDR_LOOPBACK), "accept, from 127.0.0.1");
    nb = 0; ioctlsocket(c, FIONBIO, &nb);
    ok(send(c, "hello", 5, 0) == 5, "send");
    FD_ZERO(&r); FD_SET(s, &r);
    ok(select(0, &r, 0, 0, &tv) == 1, "select: readable");
    memset(buf, 0, sizeof buf);
    n = recv(s, buf, sizeof buf, 0);
    if (n != 5) printf("  (recv returned %d: %.8s)\n", n, buf);
    ok(n == 5 && !memcmp(buf, "hello", 5), "recv what was sent");

    ev = WSACreateEvent();
    ok(!WSAEventSelect(s, ev, FD_READ | FD_CLOSE), "WSAEventSelect");
    ok(send(c, "x", 1, 0) == 1, "send again");
    ok(WSAWaitForMultipleEvents(1, &ev, FALSE, 5000, FALSE) == WSA_WAIT_EVENT_0, "the event fires");
    ok(!WSAEnumNetworkEvents(s, ev, &ne) && (ne.lNetworkEvents & FD_READ), "FD_READ");
    nb = 0; WSAEventSelect(s, 0, 0); ioctlsocket(s, FIONBIO, &nb);
    ok(recv(s, buf, sizeof buf, 0) == 1, "recv the byte");
    ok(!shutdown(c, SD_SEND), "shutdown");
    ok(recv(s, buf, sizeof buf, 0) == 0, "recv sees the end");
    closesocket(c); closesocket(s); closesocket(l); WSACloseEvent(ev);

    u1 = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    u2 = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    a.sin_port = 0; len = sizeof a;
    ok(!bind(u1, (struct sockaddr *)&a, sizeof a) && !getsockname(u1, (struct sockaddr *)&a, &len), "a UDP socket bound");
    ok(sendto(u2, "dgram", 5, 0, (struct sockaddr *)&a, sizeof a) == 5, "sendto");
    len = sizeof b; memset(buf, 0, sizeof buf);
    FD_ZERO(&r); FD_SET(u1, &r);
    ok(select(0, &r, 0, 0, &tv) == 1, "select: a datagram");
    ok(recvfrom(u1, buf, sizeof buf, 0, (struct sockaddr *)&b, &len) == 5 && !memcmp(buf, "dgram", 5), "recvfrom");
    closesocket(u1); closesocket(u2);
    WSACleanup();
    printf("%d failures\n", fails);
    return fails != 0;
}
