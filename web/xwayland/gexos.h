/*
 * The socket and shared-memory calls a host-side X server makes, redirected
 * into gex.
 *
 * Xwayland has exactly two connections to the world: X clients on one side,
 * a Wayland compositor on the other. Compiled for the host there is no kernel
 * under it to provide either -- emscripten's sockets are WebSockets, and its
 * mmap cannot share a file with anyone. Both are things gex has, in different
 * places:
 *
 *   X clients      are guest processes, and the unix socket they connect to
 *                  (/tmp/.X11-unix/X0) lives in the global kernel module. The
 *                  page's Worker holds an instance of it, so `k_sock_*` is a
 *                  direct call away -- and because that instance imports THIS
 *                  module's memory as the process memory, a read lands in the
 *                  server's own buffer with no copy in between.
 *
 *   the compositor is gex-wayland, running in this same Worker as its own
 *                  module. There is no socket at all: a write here is a call
 *                  into the compositor, and its events come back into a queue.
 *
 * So rather than emulating a socket layer, the few places that name one are
 * pointed at these. Each file that needs it includes this header, which
 * renames the calls in that translation unit only -- no libc symbol is
 * shadowed, so the rest of the module (fonts, the keymap, stdio) keeps the
 * emscripten filesystem it already has.
 *
 * An fd here is a REAL emscripten descriptor, obtained by opening /dev/null,
 * so its number can never collide with one the filesystem hands out. What it
 * means is ours: every read and write of it goes through the renamed calls.
 */
#ifndef GEX_OS_H
#define GEX_OS_H

/* The real declarations first, and on purpose.
 *
 * The renames at the bottom are macros, so anything declared *after* them is
 * declared under the new name -- which for libc's own <poll.h> means a
 * conflicting `gex_poll`. Pulling those headers in here, before the macros,
 * means every later include is a no-op behind its own guard and only call
 * sites are renamed. It also lets these declarations use the same types libc
 * does (`nfds_t`, `socklen_t`) rather than guesses that happen to match. */
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

int gex_socket(int domain, int type, int protocol);
int gex_bind(int fd, const struct sockaddr *addr, socklen_t len);
int gex_listen(int fd, int backlog);
int gex_accept(int fd, struct sockaddr *addr, socklen_t *len);
int gex_connect(int fd, const struct sockaddr *addr, socklen_t len);
int gex_close(int fd);
int gex_shutdown(int fd, int how);
int gex_fcntl(int fd, int cmd, ...);
int gex_setsockopt(int fd, int level, int name, const void *val, socklen_t len);
int gex_getsockopt(int fd, int level, int name, void *val, socklen_t *len);
int gex_getpeername(int fd, struct sockaddr *addr, socklen_t *len);
int gex_getsockname(int fd, struct sockaddr *addr, socklen_t *len);
ssize_t gex_read(int fd, void *buf, size_t n);
ssize_t gex_write(int fd, const void *buf, size_t n);
ssize_t gex_readv(int fd, const struct iovec *iov, int n);
ssize_t gex_writev(int fd, const struct iovec *iov, int n);
ssize_t gex_recvmsg(int fd, struct msghdr *msg, int flags);
ssize_t gex_sendmsg(int fd, const struct msghdr *msg, int flags);
int gex_poll(struct pollfd *fds, nfds_t nfds, int timeout);
int gex_ppoll(struct pollfd *fds, nfds_t nfds, const struct timespec *timeout,
              const sigset_t *mask);

/* A pool of pixels the compositor is going to read directly out of this
 * module's memory: `gex_shm_alloc` returns a descriptor, `gex_shm_map` the
 * address it stands for. Nothing is copied and nothing is a file. */
int gex_shm_alloc(off_t size);
void *gex_shm_map(int fd, size_t size);
int gex_shm_unmap(void *addr, size_t size);
int gex_shm_truncate(int fd, off_t size);

#ifdef GEX_OS_RENAME
#define socket      gex_socket
#define bind        gex_bind
#define listen      gex_listen
#define accept      gex_accept
#define connect     gex_connect
#define close       gex_close
#define shutdown    gex_shutdown
#define fcntl       gex_fcntl
#define setsockopt  gex_setsockopt
#define getsockopt  gex_getsockopt
#define getpeername gex_getpeername
#define getsockname gex_getsockname
#define read        gex_read
#define write       gex_write
#define readv       gex_readv
#define writev      gex_writev
#define recvmsg     gex_recvmsg
#define sendmsg     gex_sendmsg
#define poll        gex_poll
#define ppoll       gex_ppoll
#endif /* GEX_OS_RENAME */

#endif /* GEX_OS_H */
