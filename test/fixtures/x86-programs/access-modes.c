/* access(2) honours its mode. gex answered every access() of an existing
 * path with 0, so Xwayland took root's /var/lib/xkb (755 root) for its
 * keymap output, xkbcomp could not write there, and the server died with
 * "Failed to activate virtual core keyboard". Run as an ordinary user. */
#include <stdio.h>
#include <unistd.h>

static void probe(const char *path, int mode, const char *name) {
    printf("%s %s: %d\n", path, name, access(path, mode) == 0);
}

int main(void) {
    if (getuid() == 0) { puts("skipped (root)"); return 0; }
    probe("/", F_OK, "F_OK");
    probe("/", R_OK | X_OK, "R_OK|X_OK");
    probe("/", W_OK, "W_OK");
    probe("/etc", W_OK, "W_OK");
    probe("/etc/passwd", R_OK, "R_OK");
    probe("/etc/passwd", W_OK, "W_OK");
    probe("/etc/passwd", X_OK, "X_OK");
    probe("/bin/sh", X_OK, "X_OK");
    probe("/nonexistent-gex", F_OK, "F_OK");
    return 0;
}
