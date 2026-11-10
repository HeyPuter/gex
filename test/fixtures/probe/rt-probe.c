/* What the runtime's tests ask a guest about itself, where the test tree's
 * busybox has no applet for it:
 *   rt-probe size    the terminal's "rows cols"
 *   rt-probe tty     the terminal's name
 *   rt-probe ids     "uid gid"
 *   rt-probe creat PATH MODE   open(PATH, O_CREAT|O_WRONLY, MODE, octal)
 *                    under umask 022: the file's mode by fstat, then by stat
 *   rt-probe chmod PATH MODE   PATH opened, chmod to MODE (octal): the mode
 *                    by fstat of the open descriptor, then by stat
 *   rt-probe xattr PATH NAME   the attribute's value
 *   rt-probe setxattr PATH NAME VALUE   the attribute set: "set"
 *   rt-probe connect HOST PORT TEXT   TEXT to HOST:PORT (a name: getaddrinfo),
 *                    then what comes back until the far end closes
 *   rt-probe sendto HOST PORT TEXT...   each TEXT a datagram to HOST:PORT
 *                    from one unconnected UDP socket, then each reply by
 *                    recvfrom: "ADDR:PORT REPLY" (5 s each, else "timeout")
 *   rt-probe udp HOST PORT TEXT   TEXT on a connected UDP socket (send,
 *                    recv): the reply
 *   rt-probe flood HOST PORT N SIZE   N datagrams of SIZE bytes (each
 *                    numbered) on a connected UDP socket, none read until a
 *                    second after the last, then every reply read: "N ok",
 *                    or the first one wrong
 *   rt-probe hold HOST PORT N [fork|trap] [udp]   N connections to
 *                    HOST:PORT (`udp`: connected UDP sockets), a byte sent on
 *                    each, then "held N" and a read of the first for ever
 *                    (with `fork`: a child holds them too, and says "child
 *                    PID"; with `trap`: a trap instead of the read)
 *   rt-probe serve PORT   on 127.0.0.1:PORT, one connection: what it sends,
 *                    upper-cased, back ("listening" once it listens)
 *   rt-probe tone HZ MS   a sine of HZ for MS ms on /dev/dsp (s16, stereo,
 *                    48 kHz), then the ms it took
 *   rt-probe ppid    its parent's pid
 *   rt-probe kill PID SIG   kill(PID, SIG): "0", or the error
 *   rt-probe trap    a trap (wasm's unreachable): the program's crash
 *   rt-probe sigpipe   a write to a pipe with no reader, SIGPIPE's action the
 *                    default: the program dies of SIGPIPE
 *   rt-probe init [CMD]   an init: starts `sh -c CMD`, then collects every
 *                    child, "reaped PID CODE" for each, for ever (when it
 *                    has none, it waits for one)
 */
#include <signal.h>
#include <fcntl.h>
#include <math.h>
#include <sys/soundcard.h>
#include <time.h>
#include <ctype.h>
#include <netdb.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

int main(int argc, char **argv) {
  const char *what = argc > 1 ? argv[1] : "";
  if (!strcmp(what, "size")) {
    struct winsize ws;
    if (ioctl(0, TIOCGWINSZ, &ws) < 0) {
      perror("TIOCGWINSZ");
      return 1;
    }
    printf("%d %d\n", ws.ws_row, ws.ws_col);
  } else if (!strcmp(what, "tty")) {
    const char *name = ttyname(0);
    if (!name) {
      perror("ttyname");
      return 1;
    }
    printf("%s\n", name);
  } else if (!strcmp(what, "ppid")) {
    printf("%d\n", (int)getppid());
  } else if (!strcmp(what, "kill") && argc == 4) {
    if (kill(atoi(argv[2]), atoi(argv[3])) < 0) {
      perror("kill");
      return 1;
    }
    printf("0\n");
  } else if (!strcmp(what, "init")) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("init %d\n", (int)getpid());
    if (argc > 2) {
      pid_t c = fork();
      if (c == 0) {
        execl("/bin/sh", "sh", "-c", argv[2], (char *)NULL);
        _exit(127);
      }
      printf("started %d\n", (int)c);
    }
    for (;;) {
      int st;
      pid_t pid = waitpid(-1, &st, 0);
      if (pid < 0) {
        usleep(50000);
        continue;
      }
      printf("reaped %d %d\n", (int)pid, WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
    }
  } else if (!strcmp(what, "trap")) {
    __builtin_trap();
  } else if (!strcmp(what, "sigpipe")) {
    int p[2];
    signal(SIGPIPE, SIG_DFL);
    if (pipe(p)) return 2;
    close(p[0]);
    (void)!write(p[1], "x", 1);
    return 3;
  } else if ((!strcmp(what, "creat") || !strcmp(what, "chmod")) && argc == 4) {
    mode_t mode = (mode_t)strtoul(argv[3], 0, 8);
    int fd;
    if (!strcmp(what, "creat")) {
      umask(022);
      fd = open(argv[2], O_CREAT | O_WRONLY, mode);
    } else {
      fd = open(argv[2], O_RDONLY);
      if (fd >= 0 && chmod(argv[2], mode) < 0) {
        perror("chmod");
        return 1;
      }
    }
    struct stat f, s;
    if (fd < 0 || fstat(fd, &f) < 0 || stat(argv[2], &s) < 0) {
      perror(argv[2]);
      return 1;
    }
    printf("%04o %04o\n", (unsigned)(f.st_mode & 07777), (unsigned)(s.st_mode & 07777));
  } else if (!strcmp(what, "ids")) {
    printf("%u %u\n", (unsigned)getuid(), (unsigned)getgid());
  } else if (!strcmp(what, "xattr") && argc == 4) {
    char v[4096];
    ssize_t n = getxattr(argv[2], argv[3], v, sizeof v);
    if (n < 0) {
      perror("getxattr");
      return 1;
    }
    printf("%.*s\n", (int)n, v);
  } else if (!strcmp(what, "setxattr") && argc == 5) {
    if (setxattr(argv[2], argv[3], argv[4], strlen(argv[4]), 0) < 0) {
      perror("setxattr");
      return 1;
    }
    printf("set\n");
  } else if (!strcmp(what, "connect") && argc == 5) {
    struct addrinfo hints = {0}, *ai;
    hints.ai_socktype = SOCK_STREAM;
    int e = getaddrinfo(argv[2], argv[3], &hints, &ai);
    if (e) {
      fprintf(stderr, "getaddrinfo: %s\n", gai_strerror(e));
      return 1;
    }
    int s = socket(ai->ai_family, SOCK_STREAM, 0);
    if (s < 0 || connect(s, ai->ai_addr, ai->ai_addrlen) < 0) {
      perror("connect");
      return 1;
    }
    if (write(s, argv[4], strlen(argv[4])) < 0) perror("write");
    shutdown(s, SHUT_WR);
    char buf[4096];
    ssize_t n;
    while ((n = read(s, buf, sizeof buf)) > 0) fwrite(buf, 1, (size_t)n, stdout);
    printf("\n");
  } else if ((!strcmp(what, "sendto") && argc >= 5) || (!strcmp(what, "udp") && argc == 5)) {
    int connected = !strcmp(what, "udp");
    struct addrinfo hints = {0}, *ai;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;
    int e = getaddrinfo(argv[2], argv[3], &hints, &ai);
    if (e) {
      fprintf(stderr, "getaddrinfo: %s\n", gai_strerror(e));
      return 1;
    }
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0 || (connected && connect(s, ai->ai_addr, ai->ai_addrlen) < 0)) {
      perror("connect");
      return 1;
    }
    for (int i = 4; i < argc; i++) {
      ssize_t w = connected ? send(s, argv[i], strlen(argv[i]), 0)
                            : sendto(s, argv[i], strlen(argv[i]), 0, ai->ai_addr, ai->ai_addrlen);
      if (w < 0) {
        perror(connected ? "send" : "sendto");
        return 1;
      }
    }
    for (int i = 4; i < argc; i++) {
      struct pollfd p = {s, POLLIN, 0};
      if (poll(&p, 1, 5000) != 1) {
        printf("timeout\n");
        return 1;
      }
      char buf[2048];
      struct sockaddr_in from = {0};
      socklen_t fl = sizeof from;
      ssize_t n = connected ? recv(s, buf, sizeof buf, 0)
                            : recvfrom(s, buf, sizeof buf, 0, (struct sockaddr *)&from, &fl);
      if (n < 0) {
        perror("recv");
        return 1;
      }
      if (connected) printf("%.*s\n", (int)n, buf);
      else printf("%s:%d %.*s\n", inet_ntoa(from.sin_addr), ntohs(from.sin_port), (int)n, buf);
    }
  } else if (!strcmp(what, "flood") && argc == 6) {
    struct addrinfo hints = {0}, *ai;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;
    int e = getaddrinfo(argv[2], argv[3], &hints, &ai);
    if (e) {
      fprintf(stderr, "getaddrinfo: %s\n", gai_strerror(e));
      return 1;
    }
    int n = atoi(argv[4]), size = atoi(argv[5]);
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0 || connect(s, ai->ai_addr, ai->ai_addrlen) < 0 || size < 4 || size > 65000) {
      perror("connect");
      return 1;
    }
    static unsigned char d[65536];
    for (int i = 0; i < n; i++) {
      for (int k = 0; k < size; k++) d[k] = (unsigned char)(i * 7 + k);
      d[0] = (unsigned char)(i >> 24), d[1] = (unsigned char)(i >> 16), d[2] = (unsigned char)(i >> 8), d[3] = (unsigned char)i;
      if (send(s, d, (size_t)size, 0) != size) {
        perror("send");
        return 1;
      }
    }
    sleep(1);
    for (int i = 0; i < n; i++) {
      struct pollfd p = {s, POLLIN, 0};
      if (poll(&p, 1, 5000) != 1) {
        printf("%d: timeout\n", i);
        return 1;
      }
      ssize_t r = recv(s, d, sizeof d, 0);
      int at = r >= 4 ? (d[0] << 24) | (d[1] << 16) | (d[2] << 8) | d[3] : -1;
      int same = r == size && at == i;
      for (int k = 4; same && k < size; k++) same = d[k] == (unsigned char)(i * 7 + k);
      if (!same) {
        printf("%d: got %zd bytes, number %d\n", i, r, at);
        return 1;
      }
    }
    printf("%d ok\n", n);
  } else if (!strcmp(what, "hold") && argc >= 5 && argc <= 7) {
    int fork_ = 0, trap_ = 0, udp = 0;
    for (int i = 5; i < argc; i++) {
      if (!strcmp(argv[i], "fork")) fork_ = 1;
      else if (!strcmp(argv[i], "trap")) trap_ = 1;
      else if (!strcmp(argv[i], "udp")) udp = 1;
      else return 2;
    }
    int type = udp ? SOCK_DGRAM : SOCK_STREAM;
    struct addrinfo hints = {0}, *ai;
    hints.ai_family = AF_INET;
    hints.ai_socktype = type;
    if (getaddrinfo(argv[2], argv[3], &hints, &ai)) return 1;
    int n = atoi(argv[4]), first = -1;
    for (int i = 0; i < n; i++) {
      int s = socket(ai->ai_family, type, 0);
      if (s < 0 || connect(s, ai->ai_addr, ai->ai_addrlen) < 0) {
        perror("connect");
        return 1;
      }
      if (write(s, "x", 1) < 0) perror("write");
      if (first < 0) first = s;
    }
    setvbuf(stdout, 0, _IONBF, 0);
    if (fork_) {
      pid_t c = fork();
      if (c == 0) {
        char b;
        for (;;) if (read(first, &b, 1) <= 0) pause();
      }
      printf("child %d\n", (int)c);
    }
    printf("held %d\n", n);
    if (trap_) __builtin_trap();
    char b;
    for (;;) if (read(first, &b, 1) <= 0) pause();
  } else if (!strcmp(what, "serve") && argc == 3) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons((unsigned short)atoi(argv[2]));
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0 || listen(s, 1) < 0) {
      perror("listen");
      return 1;
    }
    printf("listening\n");
    fflush(stdout);
    int c = accept(s, NULL, NULL);
    if (c < 0) {
      perror("accept");
      return 1;
    }
    char buf[4096];
    ssize_t n;
    while ((n = read(c, buf, sizeof buf)) > 0) {
      for (ssize_t i = 0; i < n; i++) buf[i] = (char)toupper((unsigned char)buf[i]);
      if (write(c, buf, (size_t)n) < 0) break;
    }
    close(c);
    printf("served\n");
  } else if (!strcmp(what, "tone") && argc == 4) {
    int fd = open("/dev/dsp", O_WRONLY);
    int fmt = AFMT_S16_LE, ch = 2, rate = 48000;
    if (fd < 0 || ioctl(fd, SNDCTL_DSP_SETFMT, &fmt) < 0 || ioctl(fd, SNDCTL_DSP_CHANNELS, &ch) < 0 || ioctl(fd, SNDCTL_DSP_SPEED, &rate) < 0) {
      perror("/dev/dsp");
      return 1;
    }
    double hz = atof(argv[2]);
    long frames = (long)rate * atol(argv[3]) / 1000;
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    short buf[2 * 1024];
    for (long f = 0; f < frames;) {
      int n = frames - f < 1024 ? (int)(frames - f) : 1024;
      for (int i = 0; i < n; i++) buf[2 * i] = buf[2 * i + 1] = (short)(12000 * sin(2 * M_PI * hz * (double)(f + i) / rate));
      if (write(fd, buf, (size_t)n * 4) < 0) {
        perror("write");
        return 1;
      }
      f += n;
    }
    clock_gettime(CLOCK_MONOTONIC, &t1);
    close(fd);
    printf("%ld\n", (long)((t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000));
  } else {
    fprintf(stderr, "usage: rt-probe size|tty|ids|xattr PATH NAME|setxattr PATH NAME VALUE|connect HOST PORT TEXT|sendto HOST PORT TEXT...|udp HOST PORT TEXT|flood HOST PORT N SIZE|hold HOST PORT N [fork|trap] [udp]|serve PORT|tone HZ MS\n");
    return 2;
  }
  return 0;
}
