/* What the runtime's tests ask a guest about itself, where the test tree's
 * busybox has no applet for it:
 *   rt-probe size    the terminal's "rows cols"
 *   rt-probe tty     the terminal's name
 *   rt-probe ids     "uid gid"
 *   rt-probe xattr PATH NAME   the attribute's value
 */
#include <stdio.h>
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
  } else {
    fprintf(stderr, "usage: rt-probe size|tty|ids|xattr PATH NAME\n");
    return 2;
  }
  return 0;
}
