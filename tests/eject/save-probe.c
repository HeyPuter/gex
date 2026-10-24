/* A save, one visit of three to a page that keeps /save. argv[1] is
 * the visit, argv[2] how it saves. Each visit checks what the last one saved,
 * then saves: "rename" writes a temporary file, fsyncs it and renames it over
 * the save; "stdio" writes the save itself with fopen, fwrite and fclose. The
 * directory is listed before and after. Then it keeps running until the page
 * is reloaded: a snapshot after it exits is not what keeps the save.
 *   1  no save yet; save "one".
 *   2  the save reads "one"; save "two" over it.
 *   3  the save reads "two".
 * Prints "probe: saved N" (or "probe: checked 3"), the listing, or
 * "probe: FAIL ...". */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* Reached through a symlink, a save directory beside the program pointing
 * into the kept one: /game/data/SAVE is a link to /save/data/SAVE. */
#define DIR_ "/game/data/SAVE"
#define SAVE DIR_ "/quick.sav"
#define TMP DIR_ "/quick.tmp"

static int fail(int visit, const char *what) {
    printf("probe: FAIL visit %d: %s (%s)\n", visit, what, strerror(errno));
    fflush(stdout);
    for (;;) pause();
}

static void list(void) {
    DIR *d = opendir(DIR_);
    printf("probe: listing");
    if (!d) printf(" (no directory)");
    for (struct dirent *e; d && (e = readdir(d));)
        if (e->d_name[0] != '.') printf(" %s", e->d_name);
    if (d) closedir(d);
    printf("\n");
}

static const char *how = "rename";

static void save(int visit, const char *what) {
    if (strcmp(how, "stdio") == 0) { /* no fsync, no rename */
        FILE *f = fopen(SAVE, "wb");
        if (!f || fwrite(what, 1, strlen(what), f) != strlen(what) || fclose(f) != 0) fail(visit, "stdio save");
        return;
    }
    int fd = open(TMP, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) fail(visit, "open the temporary file");
    if (write(fd, what, strlen(what)) != (ssize_t)strlen(what)) fail(visit, "write");
    if (fsync(fd) < 0) fail(visit, "fsync");
    if (close(fd) < 0) fail(visit, "close");
    if (rename(TMP, SAVE) < 0) fail(visit, "rename over the save");
}

static int reads(const char *want) {
    char buf[64] = {0};
    int fd = open(SAVE, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    return n >= 0 && strcmp(buf, want) == 0;
}

int main(int argc, char **argv) {
    int visit = argc > 1 ? atoi(argv[1]) : 0;
    if (argc > 2) how = argv[2];
    list();
    switch (visit) {
    case 1:
        if (access(SAVE, F_OK) == 0) fail(visit, "a save before the first one");
        save(visit, "one");
        break;
    case 2:
        if (!reads("one")) fail(visit, "the first save is not back");
        save(visit, "two");
        break;
    case 3:
        if (!reads("two")) fail(visit, "the second save is not back");
        break;
    default:
        fail(visit, "no visit number");
    }
    list();
    printf(visit == 3 ? "probe: checked 3\n" : "probe: saved %d\n", visit);
    fflush(stdout);
    for (;;) pause();
}
