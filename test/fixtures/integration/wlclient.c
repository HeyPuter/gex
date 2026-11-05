/* A Wayland client that speaks the wire protocol directly (no libwayland), to
 * test gex's compositor. Each message is an object id, a word holding the
 * opcode (low half) and byte length (high half), then arguments, all
 * little-endian and 32-bit aligned. Does what any client does: get the
 * registry, bind the compositor/shm/xdg_wm_base, make a surface with a toplevel
 * role, draw into a memfd buffer, attach, and commit. The compositor is in the
 * host and reads the pixels in place. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define WIDTH 64
#define HEIGHT 48

static int sock;

/* --- writing messages --- */

static uint8_t obuf[4096];
static size_t olen;

static void put32(uint32_t v) {
    memcpy(obuf + olen, &v, 4);
    olen += 4;
}

static size_t start(uint32_t object, uint16_t opcode) {
    size_t at = olen;
    put32(object);
    put32(opcode);
    return at;
}

static void end(size_t at) {
    uint32_t word;
    memcpy(&word, obuf + at + 4, 4);
    word |= (uint32_t)(olen - at) << 16;
    memcpy(obuf + at + 4, &word, 4);
}

static void put_string(const char *s) {
    uint32_t len = (uint32_t)strlen(s) + 1;
    put32(len);
    memcpy(obuf + olen, s, len);
    olen += len;
    while (olen % 4) {
        obuf[olen++] = 0;
    }
}

/* Send everything queued, optionally with one descriptor attached. */
static int flush_with_fd(int fd) {
    struct msghdr msg;
    struct iovec iov;
    char control[CMSG_SPACE(sizeof(int))];
    memset(&msg, 0, sizeof msg);
    memset(control, 0, sizeof control);
    iov.iov_base = obuf;
    iov.iov_len = olen;
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
    ssize_t n = sendmsg(sock, &msg, 0);
    olen = 0;
    return n < 0 ? -1 : 0;
}

static int flush(void) {
    return flush_with_fd(-1);
}

/* --- reading events --- */

static uint8_t ibuf[65536];
static size_t ilen;

/* Pull whatever has arrived. Returns the number of new bytes, or -1. */
static int drain(int timeout_ms) {
    struct pollfd p = {.fd = sock, .events = POLLIN};
    int r = poll(&p, 1, timeout_ms);
    if (r <= 0) {
        return 0;
    }
    ssize_t n = recv(sock, ibuf + ilen, sizeof ibuf - ilen, 0);
    if (n <= 0) {
        return -1;
    }
    ilen += (size_t)n;
    return (int)n;
}

/* Walk the events we have, reporting the globals the registry named. */
struct global {
    uint32_t name;
    char interface[64];
    uint32_t version;
};
static struct global globals[16];
static int nglobals;

static void parse_events(void) {
    size_t at = 0;
    while (at + 8 <= ilen) {
        uint32_t object, word;
        memcpy(&object, ibuf + at, 4);
        memcpy(&word, ibuf + at + 4, 4);
        uint16_t opcode = word & 0xffff;
        uint16_t size = word >> 16;
        if (size < 8 || at + size > ilen) {
            break;
        }
        const uint8_t *body = ibuf + at + 8;
        if (object == 1 && opcode == 0) {
            /* wl_display.error(object, code, message) */
            uint32_t who, code, len;
            memcpy(&who, body, 4);
            memcpy(&code, body + 4, 4);
            memcpy(&len, body + 8, 4);
            printf("protocol error on %u code %u: %.*s\n", who, code, (int)len - 1,
                   (const char *)body + 12);
            exit(1);
        }
        if (object == 2 && opcode == 0 && nglobals < 16) {
            /* wl_registry.global(name, interface, version) */
            uint32_t name, len;
            memcpy(&name, body, 4);
            memcpy(&len, body + 4, 4);
            struct global *g = &globals[nglobals++];
            g->name = name;
            snprintf(g->interface, sizeof g->interface, "%.*s", (int)len - 1,
                     (const char *)body + 8);
            size_t pad = (len + 3) & ~3u;
            memcpy(&g->version, body + 8 + pad, 4);
        }
        at += size;
    }
    memmove(ibuf, ibuf + at, ilen - at);
    ilen -= at;
}

static uint32_t find_global(const char *want) {
    for (int i = 0; i < nglobals; i++) {
        if (strcmp(globals[i].interface, want) == 0) {
            return globals[i].name;
        }
    }
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    const char *dir = getenv("XDG_RUNTIME_DIR");
    const char *disp = getenv("WAYLAND_DISPLAY");
    if (!dir || !disp) {
        printf("no display in the environment\n");
        return 1;
    }
    char path[256];
    if (disp[0] == '/') {
        snprintf(path, sizeof path, "%s", disp);
    } else {
        snprintf(path, sizeof path, "%s/%s", dir, disp);
    }

    sock = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    snprintf(sa.sun_path, sizeof sa.sun_path, "%s", path);
    if (connect(sock, (struct sockaddr *)&sa, sizeof sa) != 0) {
        printf("connect to %s: %s\n", path, strerror(errno));
        return 1;
    }
    printf("connected\n");

    /* wl_display.get_registry(new_id 2) */
    size_t at = start(1, 1);
    put32(2);
    end(at);
    /* wl_display.sync(new_id 3), so there is something to wait for */
    at = start(1, 0);
    put32(3);
    end(at);
    flush();
    drain(2000);
    parse_events();
    printf("globals %d\n", nglobals);
    for (int i = 0; i < nglobals; i++) {
        printf("  %s v%u\n", globals[i].interface, globals[i].version);
    }

    uint32_t comp = find_global("wl_compositor");
    uint32_t shm = find_global("wl_shm");
    uint32_t wm = find_global("xdg_wm_base");
    if (!comp || !shm || !wm) {
        printf("missing a global\n");
        return 1;
    }

    /* Bind: compositor -> 4, shm -> 5, xdg_wm_base -> 6 */
    at = start(2, 0); put32(comp); put_string("wl_compositor"); put32(4); put32(4); end(at);
    at = start(2, 0); put32(shm);  put_string("wl_shm");        put32(1); put32(5); end(at);
    at = start(2, 0); put32(wm);   put_string("xdg_wm_base");   put32(3); put32(6); end(at);
    /* wl_compositor.create_surface(7) */
    at = start(4, 0); put32(7); end(at);
    /* xdg_wm_base.get_xdg_surface(8, surface 7) */
    at = start(6, 2); put32(8); put32(7); end(at);
    /* xdg_surface.get_toplevel(9) */
    at = start(8, 1); put32(9); end(at);
    /* xdg_toplevel.set_title */
    at = start(9, 2); put_string("gex wayland"); end(at);
    /* xdg_surface.ack_configure(serial 1) -- the real serial arrives in the
       configure event; the compositor here does not check which. */
    at = start(8, 4); put32(1); end(at);
    /* wl_surface.commit, which is how a client says "I am ready to be sized" */
    at = start(7, 6); end(at);
    flush();
    drain(2000);
    parse_events();

    /* A buffer: a memfd, sized, mapped, drawn into. */
    int mem = memfd_create("wl-shm", MFD_CLOEXEC);
    size_t stride = WIDTH * 4;
    size_t bytes = stride * HEIGHT;
    if (mem < 0 || ftruncate(mem, (off_t)bytes) != 0) {
        perror("memfd");
        return 1;
    }
    uint8_t *px = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, mem, 0);
    if (px == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    /* Something a human can check and a test can hash: a blue field with a
       red diagonal and a green border. The format is XRGB8888, which is
       little-endian, so the bytes go blue, green, red, unused. */
    for (int y = 0; y < HEIGHT; y++) {
        for (int x = 0; x < WIDTH; x++) {
            uint8_t *p = px + y * stride + x * 4;
            int border = (x == 0 || y == 0 || x == WIDTH - 1 || y == HEIGHT - 1);
            int diagonal = (x * HEIGHT == y * WIDTH);
            p[0] = border || diagonal ? 0 : 200;
            p[1] = border ? 200 : 0;
            p[2] = diagonal ? 200 : 0;
            p[3] = 255;
        }
    }

    /* wl_shm.create_pool(10, fd, size) -- the descriptor rides along */
    at = start(5, 0); put32(10); put32((uint32_t)bytes); end(at);
    if (flush_with_fd(mem) != 0) {
        perror("sendmsg");
        return 1;
    }
    /* wl_shm_pool.create_buffer(11, offset, w, h, stride, XRGB8888) */
    at = start(10, 0);
    put32(11); put32(0); put32(WIDTH); put32(HEIGHT); put32((uint32_t)stride); put32(1);
    end(at);
    /* wl_surface.attach(buffer 11, 0, 0); wl_surface.frame(12); commit */
    at = start(7, 1); put32(11); put32(0); put32(0); end(at);
    at = start(7, 3); put32(12); end(at);
    at = start(7, 6); end(at);
    flush();

    /* Wait for the frame callback, which is the compositor saying it took the
       buffer and the client may draw again. */
    int saw_callback = 0;
    for (int i = 0; i < 20 && !saw_callback; i++) {
        if (drain(200) < 0) {
            break;
        }
        size_t p = 0;
        while (p + 8 <= ilen) {
            uint32_t object, word;
            memcpy(&object, ibuf + p, 4);
            memcpy(&word, ibuf + p + 4, 4);
            uint16_t size = word >> 16;
            if (size < 8 || p + size > ilen) {
                break;
            }
            if (object == 12 && (word & 0xffff) == 0) {
                saw_callback = 1;
            }
            p += size;
        }
        parse_events();
    }
    printf("frame callback %s\n", saw_callback ? "yes" : "no");
    munmap(px, bytes);
    close(mem);
    close(sock);
    return 0;
}
