/*
 * pid 1 of a System (web/runtime): starts the programs the page asks for.
 *
 * The page's runtime Worker (web/runtime/src/io-worker.js) listens on the
 * abstract unix socket "\0gex-runtime" in the global kernel; this connects
 * to it and reads requests. A program's standard input, output and error are
 * a terminal the page made (opened here by its /dev/pts name, which makes it
 * the controlling terminal of the program's session), /dev/null, or three
 * pipe ends the page made in the kernel and sends with the request
 * (SCM_RIGHTS). The program runs in a session of its own (posix_spawn,
 * which gex runs without a fork; another user's program goes through this
 * one as a trampoline, `--as`).
 *
 * As pid 1 it also reaps every orphan, and says when a program it started
 * ends.
 *
 * Requests, page to here: a u32 length (little-endian) and that many bytes,
 * the first a letter, the rest NUL-terminated fields.
 *   'S' id flags uid gid cwd pts argc argv... envc env...
 *       flags: 1 the terminal /dev/pts/<pts>; 2 /dev/null; else 3
 *       descriptors come with the request
 *       uid, gid: -1 keeps this process's
 *   'K' pid signal
 * Answers, here to the page: one line each.
 *   "P id pid"           started
 *   "E id errno"         could not start (no such program, ...)
 *   "X pid code signal"  a program ended (signal 0: exited with code)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stddef.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#define NAME "gex-runtime"
#define MAXFDS 64

static int ctl = -1;
static int wake[2] = {-1, -1};
static int fds[MAXFDS];
static int nfds;

static void on_chld(int sig) {
  (void)sig;
  int e = errno;
  char c = 0;
  (void)!write(wake[1], &c, 1);
  errno = e;
}

static void say(const char *line) {
  size_t n = strlen(line), at = 0;
  while (at < n) {
    ssize_t r = write(ctl, line + at, n - at);
    if (r < 0) {
      if (errno == EINTR) continue;
      _exit(0);
    }
    at += (size_t)r;
  }
}

static int connect_page(void) {
  struct sockaddr_un a;
  memset(&a, 0, sizeof a);
  a.sun_family = AF_UNIX;
  memcpy(a.sun_path + 1, NAME, sizeof NAME - 1);
  socklen_t len = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + sizeof NAME - 1);
  for (int tries = 0; tries < 600; tries++) {
    int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (s < 0) return -1;
    if (connect(s, (struct sockaddr *)&a, len) == 0) return s;
    close(s);
    usleep(50000);
  }
  return -1;
}

/* Read exactly n bytes, keeping any descriptors that come with them. */
static int read_full(unsigned char *buf, size_t n) {
  size_t at = 0;
  while (at < n) {
    struct iovec iov = {buf + at, n - at};
    union {
      char b[CMSG_SPACE(sizeof(int) * 16)];
      struct cmsghdr align;
    } cm;
    struct msghdr m;
    memset(&m, 0, sizeof m);
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = cm.b;
    m.msg_controllen = sizeof cm.b;
    ssize_t r = recvmsg(ctl, &m, MSG_CMSG_CLOEXEC);
    if (r < 0) {
      if (errno == EINTR) continue;
      return -1;
    }
    if (r == 0) return -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
      if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS) continue;
      int k = (int)((c->cmsg_len - CMSG_LEN(0)) / sizeof(int));
      int *p = (int *)CMSG_DATA(c);
      for (int i = 0; i < k; i++) {
        if (nfds < MAXFDS) fds[nfds++] = p[i];
        else close(p[i]);
      }
    }
    at += (size_t)r;
  }
  return 0;
}

static int take_fd(void) {
  if (!nfds) return -1;
  int f = fds[0];
  memmove(fds, fds + 1, sizeof(int) * (size_t)--nfds);
  return f;
}

/* The next NUL-terminated field of [*at, end), or NULL. */
static char *field(char **at, char *end) {
  char *s = *at;
  if (s >= end) return NULL;
  char *z = memchr(s, 0, (size_t)(end - s));
  if (!z) return NULL;
  *at = z + 1;
  return s;
}

/* The program `file` names: itself when it has a slash, else the first
 * executable of that name on the PATH of `envp` (or the usual one). The
 * errno of the search when there is none. */
static int find_program(const char *file, char **envp, char *out, size_t cap) {
  if (strchr(file, '/')) {
    if (strlen(file) >= cap) return ENAMETOOLONG;
    strcpy(out, file);
    return 0;
  }
  const char *path = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
  for (char **e = envp; *e; e++)
    if (!strncmp(*e, "PATH=", 5)) path = *e + 5;
  int best = ENOENT;
  size_t fl = strlen(file);
  for (const char *p = path;; p++) {
    const char *z = strchrnul(p, ':');
    size_t dl = (size_t)(z - p);
    if (dl + 1 + fl + 1 <= cap) {
      memcpy(out, p, dl);
      size_t at = dl;
      if (dl) out[at++] = '/';
      memcpy(out + at, file, fl + 1);
      if (access(out, X_OK) == 0) return 0;
      if (errno == EACCES) best = EACCES;
    }
    if (!*z) break;
    p = z;
  }
  return best;
}

static const char *self_path = "/run/gex/init";

static void spawn(char *at, char *end) {
  char *id = field(&at, end), *flags_s = field(&at, end);
  char *uid_s = field(&at, end), *gid_s = field(&at, end), *cwd = field(&at, end);
  char *pts = field(&at, end), *argc_s = field(&at, end);
  int flags = flags_s ? atoi(flags_s) : 0;
  int io[3] = {-1, -1, -1};
  if (!(flags & 3))
    for (int i = 0; i < 3; i++) io[i] = take_fd();
  char line[96];
  if (!id || !argc_s) {
    for (int i = 0; i < 3; i++) if (io[i] >= 0) close(io[i]);
    return;
  }
  int argc = atoi(argc_s);
  /* Room for the trampoline's four words in front (below). */
  char **args = calloc((size_t)argc + 6, sizeof *args);
  char **argv = args + 5;
  for (int i = 0; i < argc; i++) argv[i] = field(&at, end);
  char *envc_s = field(&at, end);
  int envc = envc_s ? atoi(envc_s) : 0;
  char **envp = calloc((size_t)envc + 1, sizeof *envp);
  for (int i = 0; i < envc; i++) envp[i] = field(&at, end);
  int bad = argc < 1 || !argv[argc - 1] || (envc && !envp[envc - 1]);
  if (!(flags & 3))
    for (int i = 0; i < 3; i++) bad |= io[i] < 0;
  char prog[4096];
  int e = bad ? EINVAL : find_program(argv[0], envp, prog, sizeof prog);
  posix_spawn_file_actions_t fa;
  posix_spawnattr_t sa;
  posix_spawn_file_actions_init(&fa);
  posix_spawnattr_init(&sa);
  if (!e) {
    if (flags & 3) {
      char path[32];
      if (flags & 1) snprintf(path, sizeof path, "/dev/pts/%s", pts);
      else strcpy(path, "/dev/null");
      /* Opened after setsid, without O_NOCTTY: a terminal becomes the
       * session's controlling terminal. */
      posix_spawn_file_actions_addopen(&fa, 0, path, O_RDWR, 0);
      posix_spawn_file_actions_adddup2(&fa, 0, 1);
      posix_spawn_file_actions_adddup2(&fa, 0, 2);
    } else {
      for (int i = 0; i < 3; i++) posix_spawn_file_actions_adddup2(&fa, io[i], i);
    }
    if (*cwd) posix_spawn_file_actions_addchdir_np(&fa, cwd);
    sigset_t none, all;
    sigemptyset(&none);
    sigfillset(&all);
    posix_spawnattr_setsigmask(&sa, &none);
    posix_spawnattr_setsigdefault(&sa, &all);
    posix_spawnattr_setflags(&sa, POSIX_SPAWN_SETSID | POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF);
    int uid = atoi(uid_s), gid = atoi(gid_s);
    char **run = argv;
    const char *path = prog;
    /* Another user: through this program as a trampoline that takes on
     * the ids and then is the program (posix_spawn cannot set them). */
    if ((uid >= 0 && (uid_t)uid != getuid()) || (gid >= 0 && (gid_t)gid != getgid())) {
      run = args;
      run[0] = (char *)self_path;
      run[1] = "--as";
      run[2] = uid_s;
      run[3] = gid_s;
      run[4] = prog;
      path = self_path;
    }
    pid_t pid;
    e = posix_spawn(&pid, path, &fa, &sa, run, envp);
    if (!e) snprintf(line, sizeof line, "P %s %d\n", id, (int)pid);
  }
  if (e) snprintf(line, sizeof line, "E %s %d\n", id, e);
  say(line);
  posix_spawn_file_actions_destroy(&fa);
  posix_spawnattr_destroy(&sa);
  for (int i = 0; i < 3; i++) if (io[i] >= 0) close(io[i]);
  free(args);
  free(envp);
}

/* `init --as UID GID PATH ARGV...`: the trampoline for another user. */
static int as_user(int argc, char **argv) {
  if (argc < 6) return 127;
  int uid = atoi(argv[2]), gid = atoi(argv[3]);
  if (gid >= 0 && setgid((gid_t)gid) < 0) {
    fprintf(stderr, "gex-init: setgid %d: %s\n", gid, strerror(errno));
    return 126;
  }
  if (uid >= 0 && setuid((uid_t)uid) < 0) {
    fprintf(stderr, "gex-init: setuid %d: %s\n", uid, strerror(errno));
    return 126;
  }
  execv(argv[4], argv + 5);
  fprintf(stderr, "gex-init: %s: %s\n", argv[4], strerror(errno));
  return 127;
}

static void reap(void) {
  for (;;) {
    int st;
    pid_t p = waitpid(-1, &st, WNOHANG);
    if (p <= 0) {
      if (p < 0 && errno == EINTR) continue;
      return;
    }
    char line[64];
    if (WIFSIGNALED(st)) snprintf(line, sizeof line, "X %d %d %d\n", (int)p, 128 + WTERMSIG(st), WTERMSIG(st));
    else snprintf(line, sizeof line, "X %d %d 0\n", (int)p, WEXITSTATUS(st));
    say(line);
  }
}

/* The tree's own start, `/etc/gex/boot` when it has one (a system's
 * daemons: dbus, ...): run once, as root, before the first program, its
 * output on the console. It must end; what it starts goes on. */
static void boot(void) {
  static char *const argv[] = {"/etc/gex/boot", NULL};
  static char *const envp[] = {"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "HOME=/root", NULL};
  if (access(argv[0], X_OK) < 0) return;
  pid_t pid;
  int err = posix_spawn(&pid, argv[0], NULL, NULL, argv, envp);
  if (err) {
    fprintf(stderr, "gex-init: %s: %s\n", argv[0], strerror(err));
    return;
  }
  int st;
  while (waitpid(pid, &st, 0) < 0 && errno == EINTR) {}
  if (WIFSIGNALED(st)) fprintf(stderr, "gex-init: %s: killed by signal %d\n", argv[0], WTERMSIG(st));
  else if (WEXITSTATUS(st)) fprintf(stderr, "gex-init: %s: exit %d\n", argv[0], WEXITSTATUS(st));
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "--as")) return as_user(argc, argv);
  if (argc > 0 && argv[0][0] == '/') self_path = argv[0];
  signal(SIGPIPE, SIG_IGN);
  if (pipe2(wake, O_CLOEXEC | O_NONBLOCK) < 0) return 1;
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_chld;
  sa.sa_flags = SA_NOCLDSTOP;
  sigaction(SIGCHLD, &sa, NULL);
  ctl = connect_page();
  if (ctl < 0) {
    fprintf(stderr, "gex-init: the page's runtime is not listening\n");
    return 1;
  }
  /* Once the page listens: its mounts are in place by then. */
  boot();
  say("R\n");
  size_t cap = 1 << 16;
  unsigned char *buf = malloc(cap);
  for (;;) {
    struct pollfd p[2] = {{ctl, POLLIN, 0}, {wake[0], POLLIN, 0}};
    int n = poll(p, 2, -1);
    if (n < 0 && errno != EINTR) return 1;
    if (p[1].revents & POLLIN) {
      char sink[64];
      while (read(wake[0], sink, sizeof sink) > 0) {}
    }
    reap();
    if (n <= 0 || !(p[0].revents & (POLLIN | POLLHUP | POLLERR))) continue;
    unsigned char hdr[4];
    if (read_full(hdr, 4) < 0) return 0;
    uint32_t len = (uint32_t)hdr[0] | (uint32_t)hdr[1] << 8 | (uint32_t)hdr[2] << 16 | (uint32_t)hdr[3] << 24;
    if (len == 0 || len > (64u << 20)) return 1;
    if (len > cap) {
      cap = len;
      buf = realloc(buf, cap);
    }
    if (read_full(buf, len) < 0) return 0;
    char *at = (char *)buf + 1, *end = (char *)buf + len;
    if (buf[0] == 'S') {
      spawn(at, end);
    } else if (buf[0] == 'K') {
      char *pid = field(&at, end), *sig = field(&at, end);
      if (pid && sig) kill(atoi(pid), atoi(sig));
    }
    reap();
  }
}
