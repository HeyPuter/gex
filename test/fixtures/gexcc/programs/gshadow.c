/* <gshadow.h> (systemd's user records and sysusers): /etc/gshadow's lines
 * parsed and written back, the same as glibc on the host. */
#include <errno.h>
#include <gshadow.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void show(const struct sgrp *g) {
    if (!g) {
        printf("none\n");
        return;
    }
    printf("name=%s passwd=%s adm=", g->sg_namp, g->sg_passwd);
    for (char **a = g->sg_adm; *a; a++)
        printf("[%s]", *a);
    printf(" mem=");
    for (char **m = g->sg_mem; *m; m++)
        printf("[%s]", *m);
    printf("\n");
}

int main(void) {
    char path[] = "/tmp/gexcc-gshadow-XXXXXX";
    int fd = mkstemp(path);
    FILE *f = fdopen(fd, "w+");
    fputs("root:*::\n\nwheel:!:alice:alice,bob,carol\n", f);
    struct sgrp g = { "audio", "!", (char *[]){ NULL }, (char *[]){ "dave", "erin", NULL } };
    putsgent(&g, f);
    rewind(f);
    struct sgrp *s;
    while ((s = fgetsgent(f)))
        show(s);
    printf("end: %s\n", errno == ENOENT ? "ENOENT" : strerror(errno));
    /* a buffer too small: ERANGE, then the same entry with room */
    rewind(f);
    struct sgrp r, *out;
    char small[8], big[256];
    int e = fgetsgent_r(f, &r, small, sizeof small, &out);
    printf("small: %s\n", e == ERANGE ? "ERANGE" : "other");
    e = fgetsgent_r(f, &r, big, sizeof big, &out);
    printf("again: %d ", e);
    show(out);
    show(sgetsgent("video:x:root:"));
    fclose(f);
    unlink(path);
    return 0;
}
