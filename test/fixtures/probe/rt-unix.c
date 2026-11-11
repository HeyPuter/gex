/* A program at the other end of the page's unix sockets (core/unix:
 * sys.listen, sys.connect). PATH starting with '@' is an abstract name.
 *   rt-unix connect PATH TEXT   TEXT to PATH, then shutdown(SHUT_WR), then
 *                    what comes back (each read after a poll) until the end;
 *                    a failed connect prints "connect: ERRNO" and exits 1
 *   rt-unix half PATH   what comes until the end (each read after a
 *                    select), as "got TEXT", then "after eof" written back
 *   rt-unix write PATH MIB   MIB MiB written (byte i is i % 251), "wrote K"
 *                    after each MiB
 *   rt-unix slowread PATH MS   connected, MS ms asleep, then everything read
 *                    to the end: "read N"
 *   rt-unix pipe PATH   reads until the end ("eof"), then writes until a
 *                    write fails: its errno ("EPIPE")
 *   rt-unix serve PATH N   bound and listening ("listening"), N connections
 *                    served at once on one epoll: each read to its end, then
 *                    what it sent upper-cased back, then closed; "served N"
 *   rt-unix seqpacket PATH TEXT   `connect`'s, on a SOCK_SEQPACKET socket
 *   rt-unix dgram PATH TEXT   TEXT sent to PATH from a SOCK_DGRAM socket:
 *                    "sent", or "sendto: ERRNO"
 */
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static const char *ename(int e) {
  switch (e) {
  case ECONNREFUSED: return "ECONNREFUSED";
  case ENOENT: return "ENOENT";
  case EPIPE: return "EPIPE";
  case ECONNRESET: return "ECONNRESET";
  case EPROTOTYPE: return "EPROTOTYPE";
  case ENOTCONN: return "ENOTCONN";
  case EADDRINUSE: return "EADDRINUSE";
  }
  static char b[32];
  snprintf(b, sizeof b, "errno %d", e);
  return b;
}

static socklen_t addr(const char *path, struct sockaddr_un *a) {
  memset(a, 0, sizeof *a);
  a->sun_family = AF_UNIX;
  size_t n = strlen(path);
  if (n >= sizeof a->sun_path) n = sizeof a->sun_path - 1;
  memcpy(a->sun_path, path, n);
  if (path[0] == '@') {
    a->sun_path[0] = 0;
    return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + n);
  }
  return sizeof *a;
}

static int dial(const char *path, int type) {
  struct sockaddr_un a;
  socklen_t len = addr(path, &a);
  int s = socket(AF_UNIX, type, 0);
  if (s < 0) {
    printf("socket: %s\n", ename(errno));
    exit(1);
  }
  if (connect(s, (struct sockaddr *)&a, len) < 0) {
    printf("connect: %s\n", ename(errno));
    exit(1);
  }
  return s;
}

static void put(int s, const char *b, size_t n) {
  while (n) {
    ssize_t w = write(s, b, n);
    if (w < 0) {
      printf("write: %s\n", ename(errno));
      exit(1);
    }
    b += w;
    n -= (size_t)w;
  }
}

static int talk(const char *path, const char *text, int type) {
  int s = dial(path, type);
  put(s, text, strlen(text));
  if (shutdown(s, SHUT_WR) < 0) printf("shutdown: %s\n", ename(errno));
  char b[4096];
  for (;;) {
    struct pollfd p = { .fd = s, .events = POLLIN };
    if (poll(&p, 1, 30000) != 1) {
      printf("poll: timeout\n");
      return 1;
    }
    ssize_t r = read(s, b, sizeof b);
    if (r < 0) {
      printf("read: %s\n", ename(errno));
      return 1;
    }
    if (r == 0) break;
    fwrite(b, 1, (size_t)r, stdout);
  }
  return 0;
}

static int half(const char *path) {
  int s = dial(path, SOCK_STREAM);
  char b[4096];
  size_t n = 0;
  for (;;) {
    fd_set r;
    FD_ZERO(&r);
    FD_SET(s, &r);
    struct timeval tv = { 30, 0 };
    if (select(s + 1, &r, NULL, NULL, &tv) != 1) {
      printf("select: timeout\n");
      return 1;
    }
    ssize_t got = read(s, b + n, sizeof b - 1 - n);
    if (got < 0) {
      printf("read: %s\n", ename(errno));
      return 1;
    }
    if (got == 0) break;
    n += (size_t)got;
  }
  b[n] = 0;
  printf("got %s\n", b);
  fflush(stdout);
  put(s, "after eof\n", 10);
  close(s);
  return 0;
}

static int flood(const char *path, int mib) {
  int s = dial(path, SOCK_STREAM);
  static unsigned char b[65536];
  unsigned long long at = 0;
  for (int k = 1; k <= mib; k++) {
    for (int c = 0; c < 16; c++) {
      for (size_t i = 0; i < sizeof b; i++) b[i] = (unsigned char)((at + i) % 251);
      put(s, (const char *)b, sizeof b);
      at += sizeof b;
    }
    printf("wrote %d\n", k);
    fflush(stdout);
  }
  close(s);
  return 0;
}

static int slowread(const char *path, int ms) {
  int s = dial(path, SOCK_STREAM);
  struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
  nanosleep(&ts, NULL);
  static char b[65536];
  unsigned long long n = 0;
  for (ssize_t r; (r = read(s, b, sizeof b)) != 0;) {
    if (r < 0) {
      printf("read: %s\n", ename(errno));
      return 1;
    }
    n += (unsigned long long)r;
  }
  printf("read %llu\n", n);
  return 0;
}

static int pipe_(const char *path) {
  signal(SIGPIPE, SIG_IGN);
  int s = dial(path, SOCK_STREAM);
  char b[4096];
  for (ssize_t r; (r = read(s, b, sizeof b)) != 0;) {
    if (r < 0) {
      printf("read: %s\n", ename(errno));
      return 1;
    }
  }
  printf("eof\n");
  memset(b, 'x', sizeof b);
  for (int i = 0; i < 1000; i++) {
    if (write(s, b, sizeof b) < 0) {
      printf("%s\n", ename(errno));
      return 0;
    }
  }
  printf("every write taken\n");
  return 1;
}

struct conn {
  int fd;
  char buf[4096];
  size_t n, sent;
  int replying;
};

static int serve(const char *path, int want) {
  struct sockaddr_un a;
  socklen_t len = addr(path, &a);
  if (path[0] != '@') unlink(path);
  int l = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
  if (bind(l, (struct sockaddr *)&a, len) < 0 || listen(l, 16) < 0) {
    printf("bind: %s\n", ename(errno));
    return 1;
  }
  int ep = epoll_create1(0);
  struct epoll_event ev = { .events = EPOLLIN, .data.ptr = NULL };
  epoll_ctl(ep, EPOLL_CTL_ADD, l, &ev);
  printf("listening\n");
  fflush(stdout);
  int accepted = 0, done = 0;
  while (done < want) {
    struct epoll_event got[8];
    int n = epoll_wait(ep, got, 8, 30000);
    if (n <= 0) {
      printf("epoll: timeout, %d served\n", done);
      return 1;
    }
    for (int i = 0; i < n; i++) {
      struct conn *c = got[i].data.ptr;
      if (!c) {
        int fd;
        while (accepted < want && (fd = accept4(l, NULL, NULL, SOCK_NONBLOCK)) >= 0) {
          c = calloc(1, sizeof *c);
          c->fd = fd;
          struct epoll_event e = { .events = EPOLLIN, .data.ptr = c };
          epoll_ctl(ep, EPOLL_CTL_ADD, fd, &e);
          accepted++;
        }
        continue;
      }
      if (!c->replying) {
        for (;;) {
          ssize_t r = read(c->fd, c->buf + c->n, sizeof c->buf - c->n);
          if (r > 0) {
            c->n += (size_t)r;
            continue;
          }
          if (r < 0 && errno == EAGAIN) break;
          // The end (or an error): the reply.
          for (size_t k = 0; k < c->n; k++) c->buf[k] = (char)toupper((unsigned char)c->buf[k]);
          c->replying = 1;
          struct epoll_event e = { .events = EPOLLOUT, .data.ptr = c };
          epoll_ctl(ep, EPOLL_CTL_MOD, c->fd, &e);
          break;
        }
        continue;
      }
      ssize_t w = write(c->fd, c->buf + c->sent, c->n - c->sent);
      if (w > 0) c->sent += (size_t)w;
      if ((w < 0 && errno != EAGAIN) || c->sent == c->n) {
        epoll_ctl(ep, EPOLL_CTL_DEL, c->fd, NULL);
        close(c->fd);
        free(c);
        done++;
      }
    }
  }
  printf("served %d\n", done);
  return 0;
}

static int dgram(const char *path, const char *text) {
  struct sockaddr_un a;
  socklen_t len = addr(path, &a);
  int s = socket(AF_UNIX, SOCK_DGRAM, 0);
  if (sendto(s, text, strlen(text), 0, (struct sockaddr *)&a, len) < 0) {
    printf("sendto: %s\n", ename(errno));
    return 1;
  }
  printf("sent\n");
  return 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  if (argc >= 4 && !strcmp(argv[1], "connect")) return talk(argv[2], argv[3], SOCK_STREAM);
  if (argc >= 4 && !strcmp(argv[1], "seqpacket")) return talk(argv[2], argv[3], SOCK_SEQPACKET);
  if (argc >= 3 && !strcmp(argv[1], "half")) return half(argv[2]);
  if (argc >= 4 && !strcmp(argv[1], "write")) return flood(argv[2], atoi(argv[3]));
  if (argc >= 4 && !strcmp(argv[1], "slowread")) return slowread(argv[2], atoi(argv[3]));
  if (argc >= 3 && !strcmp(argv[1], "pipe")) return pipe_(argv[2]);
  if (argc >= 4 && !strcmp(argv[1], "serve")) return serve(argv[2], atoi(argv[3]));
  if (argc >= 4 && !strcmp(argv[1], "dgram")) return dgram(argv[2], argv[3]);
  fprintf(stderr, "usage: rt-unix connect|seqpacket|half|write|slowread|pipe|serve|dgram PATH ...\n");
  return 2;
}
