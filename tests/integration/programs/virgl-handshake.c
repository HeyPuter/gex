/* The vtest handshake Mesa runs before anything else: connect to the socket,
 * name yourself, ask for the capability set, print its size. A live program
 * rather than a recording, because the caps reply differs by renderer (native
 * host vs WebGL 2) and printing the size says which answered. Proves the path
 * guest socket -> host device -> renderer -> reply. */
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

/* vtest, from virglrenderer's vtest_protocol.h. */
#define VCMD_GET_CAPS2 9
#define VCMD_CREATE_RENDERER 8

static int send_all(int fd, const void *buf, size_t len) {
    const char *p = buf;
    while (len) {
        ssize_t n = write(fd, p, len);
        if (n <= 0) return -1;
        p += n;
        len -= (size_t)n;
    }
    return 0;
}

static int recv_all(int fd, void *buf, size_t len) {
    char *p = buf;
    while (len) {
        ssize_t n = read(fd, p, len);
        if (n <= 0) return -1;
        p += n;
        len -= (size_t)n;
    }
    return 0;
}

int main(void) {
    /* Unbuffered, because the interesting runs are the ones that do not
     * finish: a buffered line is lost when the process is killed, and what is
     * left looks like a program that printed nothing rather than one that got
     * halfway. */
    setvbuf(stdout, NULL, _IONBF, 0);

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        printf("no socket\n");
        return 1;
    }
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof addr);
    addr.sun_family = AF_UNIX;
    strcpy(addr.sun_path, "/tmp/.virgl_test");
    if (connect(fd, (struct sockaddr *)&addr, sizeof addr) != 0) {
        printf("no renderer listening\n");
        return 1;
    }

    /* CREATE_RENDERER carries a name, and its length is in *bytes* -- alone
     * among the commands, whose lengths are in dwords. Getting that wrong
     * desynchronises the stream immediately and looks like a hang. */
    const char *name = "gex-handshake";
    unsigned int hdr[2] = {(unsigned int)strlen(name) + 1, VCMD_CREATE_RENDERER};
    if (send_all(fd, hdr, sizeof hdr) || send_all(fd, name, strlen(name) + 1)) {
        printf("could not introduce myself\n");
        return 1;
    }

    unsigned int caps[2] = {0, VCMD_GET_CAPS2};
    if (send_all(fd, caps, sizeof caps)) {
        printf("could not ask for the caps\n");
        return 1;
    }

    unsigned int reply[2];
    if (recv_all(fd, reply, sizeof reply)) {
        printf("no answer\n");
        return 1;
    }
    /* The caps reply's length field is a *byte* count, one more than the
     * truth; every other command's is in dwords. */
    unsigned int size = reply[0] ? reply[0] - 1 : 0;
    printf("capset reply %u bytes\n", size);

    /* Drained, so the answer is known to have arrived whole rather than
     * merely announced. */
    unsigned int left = size;
    char chunk[4096];
    while (left) {
        unsigned int want = left > sizeof chunk ? (unsigned int)sizeof chunk : left;
        if (recv_all(fd, chunk, want)) {
            printf("caps truncated with %u to go\n", left);
            return 1;
        }
        left -= want;
    }
    printf("capset read whole\n");
    close(fd);
    return 0;
}
