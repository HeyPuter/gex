/* mkdir's mode, less the umask, is the directory's mode: on the host's
 * filesystem (/tmp here) and after chmod. wineserver refuses a server
 * directory that others can read (it makes it 0700). */
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

static void show(const char *what, const char *p) {
    struct stat st;
    if (stat(p, &st)) { printf("%s: stat failed\n", what); return; }
    printf("%s: %o\n", what, st.st_mode & 07777);
}

int main(void) {
    char d[] = "/tmp/gex-mkdir-mode-XXXXXX";
    if (!mkdtemp(d)) return 1;
    char a[64], b[64], c[64];
    snprintf(a, sizeof a, "%s/a", d);
    snprintf(b, sizeof b, "%s/b", d);
    snprintf(c, sizeof c, "%s/c", d);
    umask(022);
    mkdir(a, 0700); show("mkdir 0700", a);
    mkdir(b, 0777); show("mkdir 0777, umask 022", b);
    umask(077);
    mkdir(c, 0755); show("mkdir 0755, umask 077", c);
    chmod(c, 0750); show("chmod 0750", c);
    rmdir(a); rmdir(b); rmdir(c); rmdir(d);
    return 0;
}
