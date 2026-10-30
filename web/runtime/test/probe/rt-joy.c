/* Game controllers as a program without udev finds them, for the runtime's
 * gamepad test:
 *
 *   rt-joy list     each /dev/input/eventN: "eventN NAME VVVV:PPPP keys K
 *                   abs A x MIN..MAX down D" (K keys and A axes it has, the
 *                   first axis's range, D keys down now), then "mtime S"
 *                   for the directory, or "none"
 *   rt-joy read N   "ready", then each event of /dev/input/eventN as
 *                   "key CODE VALUE", "abs CODE VALUE" or "syn", waiting in
 *                   poll(); "gone" when the controller goes */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

static int bits(const unsigned char *b, int n) {
  int c = 0;
  for (int i = 0; i < n; i++) c += __builtin_popcount(b[i]);
  return c;
}

static int list(void) {
  DIR *d = opendir("/dev/input");
  if (!d) {
    printf("none\n");
    return 0;
  }
  struct dirent *e;
  int found = 0;
  while ((e = readdir(d))) {
    if (strncmp(e->d_name, "event", 5)) continue;
    char path[64];
    snprintf(path, sizeof path, "/dev/input/%s", e->d_name);
    int fd = open(path, O_RDONLY | O_NONBLOCK);
    if (fd < 0) {
      printf("%s open: %s\n", e->d_name, strerror(errno));
      continue;
    }
    char name[128] = "";
    struct input_id id = {0};
    unsigned char key[KEY_MAX / 8 + 1] = {0}, abs[ABS_MAX / 8 + 1] = {0}, down[KEY_MAX / 8 + 1] = {0};
    unsigned char ev[EV_MAX / 8 + 1] = {0};
    struct input_absinfo x = {0};
    ioctl(fd, EVIOCGNAME(sizeof name), name);
    ioctl(fd, EVIOCGID, &id);
    ioctl(fd, EVIOCGBIT(0, sizeof ev), ev);
    ioctl(fd, EVIOCGBIT(EV_KEY, sizeof key), key);
    ioctl(fd, EVIOCGBIT(EV_ABS, sizeof abs), abs);
    ioctl(fd, EVIOCGKEY(sizeof down), down);
    ioctl(fd, EVIOCGABS(ABS_X), &x);
    printf("%s %s %04x:%04x keys %d abs %d x %d..%d down %d ev %02x\n", e->d_name, name, id.vendor, id.product,
           bits(key, sizeof key), bits(abs, sizeof abs), x.minimum, x.maximum, bits(down, sizeof down), ev[0]);
    found++;
    close(fd);
  }
  closedir(d);
  if (!found) printf("none\n");
  struct stat st;
  if (stat("/dev/input", &st) == 0) printf("mtime %lld\n", (long long)st.st_mtime);
  return 0;
}

static int readev(const char *n) {
  char path[64];
  snprintf(path, sizeof path, "/dev/input/event%s", n);
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    printf("open: %s\n", strerror(errno));
    return 1;
  }
  printf("ready\n");
  fflush(stdout);
  for (;;) {
    struct pollfd p = {fd, POLLIN, 0};
    if (poll(&p, 1, -1) < 0) continue;
    struct input_event ev[16];
    ssize_t r = read(fd, ev, sizeof ev);
    if (r < 0) {
      if (errno == EAGAIN || errno == EINTR) continue;
      printf(errno == ENODEV ? "gone\n" : "read: %s\n", strerror(errno));
      return 0;
    }
    for (int i = 0; i < r / (ssize_t)sizeof *ev; i++) {
      if (ev[i].type == EV_KEY) printf("key %d %d\n", ev[i].code, ev[i].value);
      else if (ev[i].type == EV_ABS) printf("abs %d %d\n", ev[i].code, ev[i].value);
      else if (ev[i].type == EV_SYN) printf("syn\n");
    }
    fflush(stdout);
  }
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "list")) return list();
  if (argc > 2 && !strcmp(argv[1], "read")) return readev(argv[2]);
  fprintf(stderr, "usage: rt-joy list | read N\n");
  return 2;
}
