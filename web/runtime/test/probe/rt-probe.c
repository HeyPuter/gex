/* What the runtime's tests ask a guest about itself, where the test tree's
 * busybox has no applet for it:
 *   rt-probe size    the terminal's "rows cols"
 *   rt-probe tty     the terminal's name
 *   rt-probe ids     "uid gid"
 *   rt-probe xattr PATH NAME   the attribute's value
 *   rt-probe connect HOST PORT TEXT   TEXT to HOST:PORT (a name: getaddrinfo),
 *                    then what comes back until the far end closes
 *   rt-probe serve PORT   on 127.0.0.1:PORT, one connection: what it sends,
 *                    upper-cased, back ("listening" once it listens)
 *   rt-probe tone HZ MS   a sine of HZ for MS ms on /dev/dsp (s16, stereo,
 *                    48 kHz), then the ms it took
 */
#include <fcntl.h>
#include <math.h>
#include <sys/soundcard.h>
#include <time.h>
#include <ctype.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <string.h>
#include <sys/ioctl.h>
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
    fprintf(stderr, "usage: rt-probe size|tty|ids|xattr PATH NAME|connect HOST PORT TEXT|serve PORT|tone HZ MS\n");
    return 2;
  }
  return 0;
}
