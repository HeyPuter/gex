/*
 * The gex side of the calls in gexos.h. See that header for why they exist.
 *
 * Everything here is marshalling: POSIX shapes in (iovecs, control messages,
 * sockaddrs, pollfd arrays) and flat arguments out to the Worker's JavaScript,
 * which owns the descriptor table and knows whether a descriptor is a guest
 * socket in the global kernel or a queue to the compositor next door.
 *
 * The one thing that is NOT marshalling is `gex_shm_alloc`: a Wayland client
 * hands its compositor pixels as a descriptor plus an offset, and the whole
 * point of putting the X server in the host is that the compositor and the
 * page can read those pixels where they lie. So a pool is malloc'd out of this
 * module's own memory and the descriptor is a name for that address.
 */
#include "gexos.h"

#include <emscripten.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <unistd.h>

/* -- what the Worker provides ---------------------------------------------- */

EM_JS(int, gexjs_socket, (void), { return Module.gexos.socket(); });
EM_JS(int, gexjs_bind, (int fd, const char *path, int len), { return Module.gexos.bind(fd, path, len); });
EM_JS(int, gexjs_connect, (int fd, const char *path, int len), { return Module.gexos.connect(fd, path, len); });
EM_JS(int, gexjs_accept, (int fd), { return Module.gexos.accept(fd); });
EM_JS(int, gexjs_close, (int fd), { return Module.gexos.close(fd); });
EM_JS(int, gexjs_readable, (int fd), { return Module.gexos.readable(fd); });
EM_JS(int, gexjs_writable, (int fd), { return Module.gexos.writable(fd); });
EM_JS(int, gexjs_send, (int fd, const char *buf, int len, const int *fds, int nfds),
      { return Module.gexos.send(fd, buf, len, fds, nfds); });
EM_JS(int, gexjs_recv, (int fd, char *buf, int len, int *fds, int maxfds, int *nfds),
      { return Module.gexos.recv(fd, buf, len, fds, maxfds, nfds); });
/* Everything the Worker does between two turns of the server's loop: pump the
 * window manager, take the compositor's scene to the page, read input back. */
EM_JS(void, gexjs_tick, (void), { Module.gexos.tick(); });
/* Sleep until something might have happened, or `ms` passes. This is the one
 * place the Worker blocks, and it blocks the way a guest process does: on the
 * global kernel's wake word. */
#ifdef GEX_XWL_JSPI
/* The single-thread build's: nothing may block, so the
 * wait is a promise and the server's stack suspends on it (JSPI) while the
 * guests run. */
EM_ASYNC_JS(int, gexjs_wait, (int ms), { return await Module.gexos.wait(ms); });
#else
EM_JS(int, gexjs_wait, (int ms), { return Module.gexos.wait(ms); });
#endif
EM_JS(int, gexjs_shm, (int addr, int size), { return Module.gexos.shm(addr, size); });
EM_JS(int, gexjs_shm_addr, (int fd), { return Module.gexos.shmAddr(fd); });
EM_JS(int, gexjs_shm_unmap, (int addr), { return Module.gexos.shmUnmap(addr); });


/* -- errno, in two dialects ------------------------------------------------ */

/*
 * The gex kernel speaks Linux errno numbers -- it *is* a Linux kernel, and its
 * guests expect Linux numbering -- while emscripten's libc uses WASI's, where
 * for instance 11 is ECANCELED rather than EAGAIN. So every error crossing from
 * JavaScript is translated here, which is the one place both spellings are in
 * scope.
 *
 * Getting this wrong is quiet and awful: libwayland reads EAGAIN as "nothing to
 * read yet" and anything else as "the connection is broken", so an untranslated
 * 11 turned the first successful read of the registry into a fatal error.
 */
static int
from_linux(int err)
{
    switch (err) {
    case 1:   return EPERM;
    case 2:   return ENOENT;
    case 4:   return EINTR;
    case 5:   return EIO;
    case 9:   return EBADF;
    case 11:  return EAGAIN;
    case 12:  return ENOMEM;
    case 13:  return EACCES;
    case 14:  return EFAULT;
    case 16:  return EBUSY;
    case 17:  return EEXIST;
    case 22:  return EINVAL;
    case 23:  return ENFILE;
    case 24:  return EMFILE;
    case 28:  return ENOSPC;
    case 32:  return EPIPE;
    case 38:  return ENOSYS;
    case 98:  return EADDRINUSE;
    case 104: return ECONNRESET;
    case 107: return ENOTCONN;
    case 111: return ECONNREFUSED;
    default:  return EIO;
    }
}

/* -- a scratch buffer for scatter/gather ----------------------------------- */

static char *scratch;
static size_t scratch_len;

static char *
scratch_for(size_t want)
{
    if (want <= scratch_len)
        return scratch;
    size_t grow = scratch_len ? scratch_len : 65536;
    while (grow < want)
        grow *= 2;
    char *p = realloc(scratch, grow);
    if (!p)
        return NULL;
    scratch = p;
    scratch_len = grow;
    return scratch;
}

static size_t
iov_total(const struct iovec *iov, int n)
{
    size_t total = 0;
    for (int i = 0; i < n; i++)
        total += iov[i].iov_len;
    return total;
}

/* -- sockets --------------------------------------------------------------- */

int
gex_socket(int domain, int type, int protocol)
{
    (void) domain; (void) type; (void) protocol;
    int fd = gexjs_socket();
    if (fd < 0) {
        errno = ENFILE;
        return -1;
    }
    return fd;
}

/* The name in a `sockaddr_un`, with its length: an abstract name starts with a
 * NUL and is not NUL-terminated, so the length is the only way to read it, and
 * the global kernel's socket table takes the same pair. */
static int
unix_name(const struct sockaddr *addr, socklen_t len, const char **path, int *plen)
{
    const struct sockaddr_un *un = (const struct sockaddr_un *) addr;
    if (!addr || len < sizeof(un->sun_family))
        return -1;
    int n = (int) (len - offsetof(struct sockaddr_un, sun_path));
    if (n < 0)
        return -1;
    if (n > 0 && un->sun_path[0] != '\0') {
        /* A path name: trust the NUL rather than the caller's length, which
         * Xtrans rounds up to the whole struct. */
        int c = 0;
        while (c < n && un->sun_path[c] != '\0')
            c++;
        n = c;
    }
    *path = un->sun_path;
    *plen = n;
    return 0;
}

int
gex_bind(int fd, const struct sockaddr *addr, socklen_t len)
{
    const char *path;
    int plen;
    if (unix_name(addr, len, &path, &plen) < 0) {
        errno = EINVAL;
        return -1;
    }
    /* bind and listen are one step in the global kernel, so this descriptor
     * becomes a listener here and `gex_listen` has nothing left to do. */
    int r = gexjs_bind(fd, path, plen);
    if (r < 0) {
        errno = from_linux(-r);
        return -1;
    }
    return 0;
}

int
gex_listen(int fd, int backlog)
{
    (void) fd; (void) backlog;
    return 0;                   /* the bind above already listened */
}

int
gex_accept(int fd, struct sockaddr *addr, socklen_t *len)
{
    int r = gexjs_accept(fd);
    if (r < 0) {
        errno = from_linux(-r);
        return -1;
    }
    if (addr && len) {
        struct sockaddr_un un;
        memset(&un, 0, sizeof un);
        un.sun_family = AF_UNIX;
        socklen_t want = sizeof un < *len ? sizeof un : *len;
        memcpy(addr, &un, want);
        *len = want;
    }
    return r;
}

int
gex_connect(int fd, const struct sockaddr *addr, socklen_t len)
{
    const char *path;
    int plen;
    if (unix_name(addr, len, &path, &plen) < 0) {
        errno = EINVAL;
        return -1;
    }
    int r = gexjs_connect(fd, path, plen);
    if (r < 0) {
        errno = from_linux(-r);
        return -1;
    }
    return 0;
}

int
gex_close(int fd)
{
    int r = gexjs_close(fd);
    if (r < 0) {
        errno = from_linux(-r);
        return -1;
    }
    return 0;
}

int
gex_shutdown(int fd, int how)
{
    (void) how;
    return gex_close(fd);
}

int
gex_fcntl(int fd, int cmd, ...)
{
    va_list ap;
    va_start(ap, cmd);
    long arg = va_arg(ap, long);
    va_end(ap);
    (void) fd;
    switch (cmd) {
    case F_GETFL:
        /* Always non-blocking: every read here returns what is there. The
         * server sets O_NONBLOCK on its client sockets and expects EAGAIN, and
         * that is exactly what the queues below do. */
        return O_RDWR | O_NONBLOCK;
    case F_SETFL:
    case F_SETFD:
        return 0;
    case F_GETFD:
        return FD_CLOEXEC;
    case F_DUPFD:
    case F_DUPFD_CLOEXEC:
        /* The same descriptor, not a copy.
         *
         * libwayland duplicates every descriptor it is about to send
         * (`wl_closure_marshal` -> `wl_os_dupfd_cloexec`) so that the caller
         * may close its own. A descriptor here is a name this Worker made up,
         * and what it names -- a pool of pixels -- outlives any of them, so an
         * alias is the whole of what a dup means. Returning 0 from the default
         * case, as this used to, meant every wl_shm pool arrived at the
         * compositor as descriptor 0: a window whose pixels were nowhere. */
        return fd;
    default:
        (void) arg;
        return 0;
    }
}

int
gex_setsockopt(int fd, int level, int name, const void *val, socklen_t len)
{
    (void) fd; (void) level; (void) name; (void) val; (void) len;
    return 0;
}

int
gex_getsockopt(int fd, int level, int name, void *val, socklen_t *len)
{
    (void) fd; (void) level;
    /* SO_PEERCRED is how the server decides a connection is local and may
     * skip authorisation. Every client here is local by construction. The
     * struct is declared here rather than included: emscripten's headers have
     * no `struct ucred`, and this is the Linux shape the server reads. */
    struct gex_ucred { int pid; unsigned int uid; unsigned int gid; };
    if (val && len && *len >= sizeof(struct gex_ucred) && name == SO_PEERCRED) {
        struct gex_ucred c = { 1, 0, 0 };
        memcpy(val, &c, sizeof c);
        *len = sizeof c;
        return 0;
    }
    if (val && len && *len >= sizeof(int)) {
        int zero = 0;
        memcpy(val, &zero, sizeof zero);
        *len = sizeof zero;
    }
    return 0;
}

/* Both names are the same made-up unix address: the server logs them and
 * Xtrans copies them about, and nothing here has any other name to give. */
int
gex_getpeername(int fd, struct sockaddr *addr, socklen_t *len)
{
    return gex_getsockname(fd, addr, len);
}

int
gex_getsockname(int fd, struct sockaddr *addr, socklen_t *len)
{
    (void) fd;
    if (addr && len) {
        struct sockaddr_un un;
        memset(&un, 0, sizeof un);
        un.sun_family = AF_UNIX;
        socklen_t want = sizeof un < *len ? sizeof un : *len;
        memcpy(addr, &un, want);
        *len = want;
    }
    return 0;
}

/* -- reading and writing --------------------------------------------------- */

ssize_t
gex_read(int fd, void *buf, size_t n)
{
    int got = gexjs_recv(fd, buf, (int) n, NULL, 0, NULL);
    if (got < 0) {
        errno = from_linux(-got);
        return -1;
    }
    return got;
}

ssize_t
gex_write(int fd, const void *buf, size_t n)
{
    int put = gexjs_send(fd, buf, (int) n, NULL, 0);
    if (put < 0) {
        errno = from_linux(-put);
        return -1;
    }
    return put;
}

ssize_t
gex_readv(int fd, const struct iovec *iov, int n)
{
    size_t total = iov_total(iov, n);
    if (total == 0)
        return 0;
    char *p = scratch_for(total);
    if (!p) {
        errno = ENOMEM;
        return -1;
    }
    int got = gexjs_recv(fd, p, (int) total, NULL, 0, NULL);
    if (got < 0) {
        errno = from_linux(-got);
        return -1;
    }
    size_t left = (size_t) got, at = 0;
    for (int i = 0; i < n && left; i++) {
        size_t take = iov[i].iov_len < left ? iov[i].iov_len : left;
        memcpy(iov[i].iov_base, p + at, take);
        at += take;
        left -= take;
    }
    return got;
}

ssize_t
gex_writev(int fd, const struct iovec *iov, int n)
{
    size_t total = iov_total(iov, n);
    if (total == 0)
        return 0;
    char *p = scratch_for(total);
    if (!p) {
        errno = ENOMEM;
        return -1;
    }
    size_t at = 0;
    for (int i = 0; i < n; i++) {
        memcpy(p + at, iov[i].iov_base, iov[i].iov_len);
        at += iov[i].iov_len;
    }
    int put = gexjs_send(fd, p, (int) total, NULL, 0);
    if (put < 0) {
        errno = from_linux(-put);
        return -1;
    }
    return put;
}

/* Control messages carry descriptors, which is how a Wayland client hands over
 * a pool of pixels and how DRI3 would hand over a buffer. The descriptors here
 * are names for addresses in this module's memory (see `gex_shm_alloc`), so
 * passing one is passing a number: no dup, no reference counting in the
 * kernel, and the compositor next door resolves it to an address. */
#define GEX_MAX_FDS 8

ssize_t
gex_recvmsg(int fd, struct msghdr *msg, int flags)
{
    (void) flags;
    size_t total = iov_total(msg->msg_iov, (int) msg->msg_iovlen);
    char *p = total ? scratch_for(total) : NULL;
    if (total && !p) {
        errno = ENOMEM;
        return -1;
    }
    int fds[GEX_MAX_FDS];
    int nfds = 0;
    int got = gexjs_recv(fd, p, (int) total, fds, GEX_MAX_FDS, &nfds);
    if (got < 0) {
        errno = from_linux(-got);
        return -1;
    }
    size_t left = (size_t) got, at = 0;
    for (int i = 0; i < (int) msg->msg_iovlen && left; i++) {
        size_t take = msg->msg_iov[i].iov_len < left ? msg->msg_iov[i].iov_len : left;
        memcpy(msg->msg_iov[i].iov_base, p + at, take);
        at += take;
        left -= take;
    }
    msg->msg_flags = 0;
    if (nfds > 0 && msg->msg_control &&
        msg->msg_controllen >= CMSG_SPACE(nfds * sizeof(int))) {
        struct cmsghdr *hdr = CMSG_FIRSTHDR(msg);
        hdr->cmsg_level = SOL_SOCKET;
        hdr->cmsg_type = SCM_RIGHTS;
        hdr->cmsg_len = CMSG_LEN(nfds * sizeof(int));
        memcpy(CMSG_DATA(hdr), fds, nfds * sizeof(int));
        msg->msg_controllen = CMSG_SPACE(nfds * sizeof(int));
    }
    else {
        msg->msg_controllen = 0;
    }
    return got;
}

ssize_t
gex_sendmsg(int fd, const struct msghdr *msg, int flags)
{
    (void) flags;
    size_t total = iov_total(msg->msg_iov, (int) msg->msg_iovlen);
    char *p = total ? scratch_for(total) : NULL;
    if (total && !p) {
        errno = ENOMEM;
        return -1;
    }
    size_t at = 0;
    for (int i = 0; i < (int) msg->msg_iovlen; i++) {
        memcpy(p + at, msg->msg_iov[i].iov_base, msg->msg_iov[i].iov_len);
        at += msg->msg_iov[i].iov_len;
    }
    int fds[GEX_MAX_FDS];
    int nfds = 0;
    for (struct cmsghdr *hdr = CMSG_FIRSTHDR((struct msghdr *) msg); hdr;
         hdr = CMSG_NXTHDR((struct msghdr *) msg, hdr)) {
        if (hdr->cmsg_level != SOL_SOCKET || hdr->cmsg_type != SCM_RIGHTS)
            continue;
        int n = (int) ((hdr->cmsg_len - CMSG_LEN(0)) / sizeof(int));
        const int *got = (const int *) CMSG_DATA(hdr);
        for (int i = 0; i < n && nfds < GEX_MAX_FDS; i++)
            fds[nfds++] = got[i];
    }
    int put = gexjs_send(fd, p, (int) total, fds, nfds);
    if (put < 0) {
        errno = from_linux(-put);
        return -1;
    }
    return put;
}

/* -- waiting --------------------------------------------------------------- */

/*
 * The server's whole idea of time passing.
 *
 * `WaitForSomething` works out how long it may sleep from its timers and calls
 * this; a Wayland round trip calls it too. Every descriptor it can name here is
 * one of ours, so readiness is a question for the Worker, and sleeping is the
 * same 20-ms-bounded park a guest process does on the global kernel's wake
 * word. The tick before the wait is where the compositor's side of the frame
 * happens -- see `gexjs_tick`.
 */
/* gexgl.c: a GL frame put into its X window, between two turns of the main
 * loop. Not from a Wayland round trip's wait (`gex_ppoll`), which can be in
 * the middle of a request. */
int gex_xgl_service(void);
static int in_ppoll;

int
gex_poll(struct pollfd *fds, nfds_t nfds, int timeout)
{
    long long deadline_slept = 0;
    for (;;) {
        gexjs_tick();
        /* A frame put: back to the loop, whose block handler posts its
         * damage, as a request's drawing would be. */
        if (!in_ppoll && gex_xgl_service())
            return 0;
        int ready = 0;
        for (nfds_t i = 0; i < nfds; i++) {
            fds[i].revents = 0;
            if (fds[i].fd < 0)
                continue;
            if ((fds[i].events & POLLOUT) && gexjs_writable(fds[i].fd) > 0)
                fds[i].revents |= POLLOUT;
            if (fds[i].events & POLLIN) {
                int r = gexjs_readable(fds[i].fd);
                if (r > 0)
                    fds[i].revents |= POLLIN;
                else if (r < 0)
                    fds[i].revents |= POLLNVAL;
            }
            if (fds[i].revents)
                ready++;
        }
        /* POLLOUT counts. The server asks for it when a client's socket in
         * the kernel is full (256 KiB) with a reply still buffered, and it
         * writes the rest only when poll says so: a poll that kept waiting
         * for POLLIN instead hung every reply bigger than the socket (the
         * client waits for the reply and sends nothing). */
        if (ready || timeout == 0)
            return ready;
        int slice = timeout < 0 ? 20 : (timeout - (int) deadline_slept > 20
                                        ? 20 : timeout - (int) deadline_slept);
        if (slice <= 0)
            return 0;
        gexjs_wait(slice);
        deadline_slept += slice;
        if (timeout > 0 && deadline_slept >= timeout) {
            /* One last look before saying the time is up. */
            gexjs_tick();
            int last = 0;
            for (nfds_t i = 0; i < nfds; i++) {
                fds[i].revents = 0;
                if (fds[i].fd >= 0 && (fds[i].events & POLLIN) &&
                    gexjs_readable(fds[i].fd) > 0)
                    fds[i].revents |= POLLIN;
                if (fds[i].fd >= 0 && (fds[i].events & POLLOUT) &&
                    gexjs_writable(fds[i].fd) > 0)
                    fds[i].revents |= POLLOUT;
                if (fds[i].revents)
                    last++;
            }
            return last;
        }
    }
}

/* libwayland waits in `ppoll`; the server waits in `poll`. Same wait. */
int
gex_ppoll(struct pollfd *fds, nfds_t nfds, const struct timespec *timeout,
          const sigset_t *mask)
{
    (void) mask;
    int ms = -1;
    if (timeout) {
        long long total = (long long) timeout->tv_sec * 1000
                        + timeout->tv_nsec / 1000000;
        ms = total > 2147483647LL ? 2147483647 : (int) total;
    }
    in_ppoll++;
    int r = gex_poll(fds, nfds, ms);
    in_ppoll--;
    return r;
}

/* -- pools of pixels ------------------------------------------------------- */

int
gex_shm_alloc(off_t size)
{
    if (size <= 0) {
        errno = EINVAL;
        return -1;
    }
    /* Page-aligned because a client may be handed the address and expect to
     * treat it as a mapping; malloc's alignment is not enough for that. */
    void *p = aligned_alloc(4096, ((size_t) size + 4095) & ~(size_t) 4095);
    if (!p) {
        errno = ENOMEM;
        return -1;
    }
    memset(p, 0, (size_t) size);
    int fd = gexjs_shm((int) (uintptr_t) p, (int) size);
    if (fd < 0) {
        free(p);
        errno = ENFILE;
        return -1;
    }
    return fd;
}

void *
gex_shm_map(int fd, size_t size)
{
    (void) size;
    int addr = gexjs_shm_addr(fd);
    if (!addr) {
        errno = EBADF;
        return (void *) -1;      /* MAP_FAILED */
    }
    return (void *) (uintptr_t) addr;
}

int
gex_shm_unmap(void *addr, size_t size)
{
    (void) size;
    /* A mapping is one of the pool's references (page/src/hostx.js `#pools`):
     * the server closes the descriptor right after making the pool and draws
     * through the mapping until the pixmap goes, and the compositor holds its
     * own. The pixels are freed when the last of them lets go. */
    gexjs_shm_unmap((int) (uintptr_t) addr);
    return 0;
}

int
gex_shm_truncate(int fd, off_t size)
{
    (void) fd; (void) size;
    return 0;                   /* allocated at its final size */
}
