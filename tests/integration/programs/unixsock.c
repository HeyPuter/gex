/* Unix domain sockets and descriptor passing, the floor Wayland stands on:
 * buffers travel as descriptors in `SCM_RIGHTS` control messages. The awkward
 * part this test turns on: descriptors on a stream socket attach to a byte, not
 * a message, so a reader taking one byte gets the descriptors sent with it and
 * leaves the rest queued. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int send_fd(int sock, const char *bytes, size_t n, int fd) {
    struct msghdr msg;
    struct iovec iov;
    char control[CMSG_SPACE(sizeof(int))];
    memset(&msg, 0, sizeof msg);
    memset(control, 0, sizeof control);
    iov.iov_base = (void *)bytes;
    iov.iov_len = n;
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    if (fd >= 0) {
        msg.msg_control = control;
        msg.msg_controllen = sizeof control;
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &fd, sizeof(int));
    }
    return (int)sendmsg(sock, &msg, 0);
}

static int recv_fd(int sock, char *bytes, size_t n, int *fd) {
    struct msghdr msg;
    struct iovec iov;
    char control[CMSG_SPACE(sizeof(int) * 4)];
    memset(&msg, 0, sizeof msg);
    iov.iov_base = bytes;
    iov.iov_len = n;
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = sizeof control;
    int got = (int)recvmsg(sock, &msg, 0);
    *fd = -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
            memcpy(fd, CMSG_DATA(c), sizeof(int));
        }
    }
    return got;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    /* A socketpair carries bytes both ways. */
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        perror("socketpair");
        return 1;
    }
    write(sv[0], "ping", 4);
    char buf[64] = {0};
    printf("pair read %zd %s\n", read(sv[1], buf, 4), buf);
    write(sv[1], "pong", 4);
    memset(buf, 0, sizeof buf);
    printf("pair back %zd %s\n", read(sv[0], buf, 4), buf);

    /* An anonymous shared file, sized, mapped, written through the mapping,
       and then sent across as a descriptor. This is exactly a Wayland shm
       buffer. */
    int mem = memfd_create("gex-buffer", MFD_CLOEXEC);
    if (mem < 0) {
        perror("memfd_create");
        return 1;
    }
    const size_t PAGE = 4096;
    if (ftruncate(mem, PAGE) != 0) {
        perror("ftruncate");
        return 1;
    }
    unsigned char *pixels = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, mem, 0);
    if (pixels == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    for (size_t i = 0; i < PAGE; i++) {
        pixels[i] = (unsigned char)(i * 7 + 1);
    }
    struct stat st;
    fstat(mem, &st);
    printf("memfd size %lld\n", (long long)st.st_size);

    /* Two bytes, with the descriptor attached to the first. Reading one byte
       must collect it and reading the second must not. */
    send_fd(sv[0], "AB", 2, mem);
    int got = -1;
    char one[1];
    printf("first byte %d fd=%s\n", recv_fd(sv[1], one, 1, &got),
           got >= 0 ? "yes" : "no");
    int again = -1;
    printf("second byte %d fd=%s\n", recv_fd(sv[1], one, 1, &again),
           again >= 0 ? "yes" : "no");

    /* The received descriptor is the same file: mapping it shows the bytes
       the sender wrote. */
    if (got >= 0) {
        unsigned char *theirs = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, got, 0);
        if (theirs == MAP_FAILED) {
            perror("mmap received");
        } else {
            unsigned sum = 0;
            for (size_t i = 0; i < PAGE; i++) {
                sum = sum * 31 + theirs[i];
            }
            printf("received contents %08x\n", sum);
            munmap(theirs, PAGE);
        }
        close(got);
    }
    close(sv[0]);
    close(sv[1]);

    /* A named socket: bind, listen, and a forked client that connects. */
    const char *path = "/tmp/gex-unixsock";
    unlink(path);
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    strncpy(sa.sun_path, path, sizeof sa.sun_path - 1);
    if (bind(srv, (struct sockaddr *)&sa, sizeof sa) != 0) {
        perror("bind");
        return 1;
    }
    if (listen(srv, 4) != 0) {
        perror("listen");
        return 1;
    }
    pid_t kid = fork();
    if (kid == 0) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(c, (struct sockaddr *)&sa, sizeof sa) != 0) {
            perror("connect");
            _exit(1);
        }
        write(c, "from-the-client", 15);
        char reply[32] = {0};
        ssize_t n = read(c, reply, sizeof reply - 1);
        printf("client heard %zd %s\n", n, reply);
        close(c);
        _exit(0);
    }
    int session = accept(srv, NULL, NULL);
    if (session < 0) {
        perror("accept");
        return 1;
    }
    memset(buf, 0, sizeof buf);
    printf("server heard %zd %s\n", read(session, buf, sizeof buf - 1), buf);
    write(session, "from-the-server", 15);
    int status = 0;
    waitpid(kid, &status, 0);
    printf("client exited %d\n", WEXITSTATUS(status));
    close(session);
    close(srv);
    unlink(path);

    /* Connecting to a name nothing listens on is refused, not a crash. */
    int lone = socket(AF_UNIX, SOCK_STREAM, 0);
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    strncpy(sa.sun_path, "/tmp/gex-nothing-here", sizeof sa.sun_path - 1);
    errno = 0;
    int rc = connect(lone, (struct sockaddr *)&sa, sizeof sa);
    printf("connect to nothing rc=%d refused=%s\n", rc,
           rc < 0 && (errno == ECONNREFUSED || errno == ENOENT) ? "yes" : "no");
    close(lone);
    munmap(pixels, PAGE);
    close(mem);
    return 0;
}
