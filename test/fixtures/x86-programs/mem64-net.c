/* Network I/O into and out of buffers above 4 GiB: read/write, recv/send
 * (and MSG_PEEK), readv/writev and sendto/recvfrom on a TCP connection to a
 * server outside the guest (mem64-net-server.mjs, which answers every byte
 * XOR 0x5a), and getrandom. In the page a host socket's bytes cross between
 * JS and the guest's buffer in place: their address was cut to 32 bits, so a
 * read landed on whatever lived at the low alias (the shim's heap) and a send
 * carried the bytes from there. argv[1] is the server's port. With a wasm32
 * memory, or natively, the buffers are low and the output is the same.
 * x86-64 only. */
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#define N 6000

static unsigned char *A, *B;

static void fill(int salt) {
    for (int i = 0; i < N; i++) A[i] = (unsigned char)(i * 7 + salt);
    memset(B, 0, N);
}

/* B holds A's bytes XOR 0x5a: what the server sent back. */
static void check(const char *what, long got) {
    int bad = 0;
    for (int i = 0; i < N; i++) bad += B[i] != (A[i] ^ 0x5a);
    printf("%s: %ld bytes, %d wrong\n", what, got, bad);
}

/* how: 0 read, 1 recv, 2 recvfrom. */
static long take(int s, unsigned char *p, long n, int how) {
    long got = 0;
    while (got < n) {
        long k = how == 0 ? read(s, p + got, n - got)
                 : how == 1 ? recv(s, p + got, n - got, 0)
                 : recvfrom(s, p + got, n - got, 0, NULL, NULL);
        if (k <= 0) break;
        got += k;
    }
    return got;
}

/* how: 0 write, 1 send, 2 sendto. */
static int give(int s, const unsigned char *p, long n, int how) {
    long put = 0;
    while (put < n) {
        long k = how == 0 ? write(s, p + put, n - put)
                 : how == 1 ? send(s, p + put, n - put, 0)
                 : sendto(s, p + put, n - put, 0, NULL, 0);
        if (k <= 0) return -1;
        put += k;
    }
    return 0;
}

int main(int argc, char **argv) {
    unsigned char *m = mmap(0, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED || argc < 2) return 2;
    A = m + 4096;
    B = m + 65536;
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(atoi(argv[1]));
    a.sin_addr.s_addr = htonl(0x7f000001);
    if (connect(s, (void *)&a, sizeof a)) { puts("connect failed"); return 1; }

    const char *names[3] = {"write/read", "send/recv", "sendto/recvfrom"};
    for (int how = 0; how < 3; how++) {
        fill(how);
        if (give(s, A, N, how)) { printf("%s: send failed\n", names[how]); return 1; }
        check(names[how], take(s, B, N, how));
    }

    /* writev and readv, each in two pieces. */
    fill(3);
    struct iovec w[2] = {{A, 1000}, {A + 1000, N - 1000}};
    long k = writev(s, w, 2);
    if (k <= 0 || give(s, A + k, N - k, 0)) { puts("writev failed"); return 1; }
    long got = 0;
    while (got < N) {
        struct iovec r[2];
        if (got < 2500) { r[0] = (struct iovec){B + got, 2500 - got}; r[1] = (struct iovec){B + 2500, N - 2500}; }
        else { r[0] = (struct iovec){B + got, N - got}; r[1] = (struct iovec){B + N, 0}; }
        long n = readv(s, r, 2);
        if (n <= 0) break;
        got += n;
    }
    check("writev/readv", got);

    /* MSG_PEEK: the bytes are seen without being taken, then taken. */
    fill(4);
    if (give(s, A, 16, 1)) return 1;
    long seen = 0;
    while (seen < 16) {
        seen = recv(s, B, 16, MSG_PEEK);
        if (seen <= 0) break;
    }
    int bad = 0;
    for (int i = 0; i < 16; i++) bad += B[i] != (A[i] ^ 0x5a);
    memset(B, 0, 16);
    long taken = take(s, B, 16, 1);
    for (int i = 0; i < 16; i++) bad += B[i] != (A[i] ^ 0x5a);
    printf("peek: %ld then %ld bytes, %d wrong\n", seen, taken, bad);
    close(s);

    /* getrandom straight into the high buffer. */
    memset(B, 0, 4096);
    long r = getrandom(B, 4096, 0);
    int zero = 0;
    for (int i = 0; i < 4096; i++) zero += !B[i];
    printf("getrandom: %ld bytes, %s\n", r, zero < 100 ? "random" : "mostly zero");
    printf("buffers above 4 GiB: %s\n", (unsigned long)m >> 32 ? "yes" : "no");
    return 0;
}
