// A unix socket's names as getsockname and getpeername give them: an
// abstract name is exactly the bytes bound (its length is offsetof +
// those bytes, trailing NULs included), a path counts its NUL, an
// accepted end has its listener's name, and an unnamed end is the family
// alone.
#define _GNU_SOURCE
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static const struct sockaddr_un *bound;
static socklen_t bound_len;

// The name's length, and whether its bytes are the ones bound.
static void show(const char *what, int fd, int peer) {
    struct sockaddr_un a;
    memset(&a, 0x55, sizeof a);
    socklen_t l = sizeof a;
    int r = peer ? getpeername(fd, (struct sockaddr *)&a, &l) : getsockname(fd, (struct sockaddr *)&a, &l);
    printf("%s %d len %u family %d", what, r, (unsigned)l, a.sun_family);
    if (r == 0 && l > offsetof(struct sockaddr_un, sun_path))
        printf(" %s", l == bound_len && !memcmp(&a, bound, l) ? "the bound name" : "another name");
    putchar('\n');
}

static void run(const char *label, const struct sockaddr_un *a, socklen_t len) {
    bound = a;
    bound_len = len;
    printf("-- %s\n", label);
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    printf("bind %d\n", bind(l, (const struct sockaddr *)a, len));
    show("bound", l, 0);
    printf("listen %d\n", listen(l, 1));
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    show("fresh", c, 0);
    printf("connect %d\n", connect(c, (const struct sockaddr *)a, len));
    int s = accept(l, NULL, NULL);
    show("accepted", s, 0);
    show("client", c, 0);
    show("client peer", c, 1);
    close(s);
    close(c);
    close(l);
}

int main(void) {
    const char *name = "gex-unix-names";
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    // Abstract, as X11 and D-Bus bind: offsetof + NUL + the name.
    memcpy(a.sun_path + 1, name, strlen(name));
    run("abstract", &a, offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name));
    // Abstract with the whole sun_path: the trailing NULs are the name's.
    struct sockaddr_un b = {.sun_family = AF_UNIX};
    memcpy(b.sun_path + 1, "gex-unix-names-pad", 18);
    run("abstract padded", &b, sizeof b);
    // A path (absolute: gex keeps a relative one resolved, where Linux
    // gives it back as spelled).
    struct sockaddr_un p = {.sun_family = AF_UNIX};
    snprintf(p.sun_path, sizeof p.sun_path, "/tmp/gex-unix-names-%d.sock", (int)getuid());
    unlink(p.sun_path);
    run("path", &p, offsetof(struct sockaddr_un, sun_path) + strlen(p.sun_path) + 1);
    unlink(p.sun_path);
    return 0;
}
