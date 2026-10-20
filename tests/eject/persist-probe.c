/* tests/eject/persist-written.sh's program: one visit of three to a page that
 * keeps /save in IndexedDB. argv[1] is the visit.
 *   1  /save/a.txt is not there yet; write it ("one").
 *   2  a.txt is back (restored); write /save/b.txt ("two"), leave a.txt alone.
 *   3  a.txt (stored once, not written since) and b.txt are both back.
 * Never touches /save/seed.txt (a bundle file) or /save/big.bin (a lazy one).
 * Prints "probe: ok N" or "probe: FAIL ..." and exits 0 or 1. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int has(const char *path, const char *want)
{
    char buf[64] = { 0 };
    FILE *f = fopen(path, "r");
    if (!f) return 0;
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = 0;
    return strcmp(buf, want) == 0;
}

static int put(const char *path, const char *what)
{
    FILE *f = fopen(path, "w");
    if (!f) return 0;
    int ok = fputs(what, f) >= 0;
    return fclose(f) == 0 && ok;
}

int main(int argc, char **argv)
{
    int visit = argc > 1 ? atoi(argv[1]) : 0;
    const char *bad = NULL;
    switch (visit) {
    case 1:
        if (fopen("/save/a.txt", "r")) bad = "a.txt there before it was written";
        else if (!put("/save/a.txt", "one")) bad = "could not write a.txt";
        break;
    case 2:
        if (!has("/save/a.txt", "one")) bad = "a.txt not restored";
        else if (!put("/save/b.txt", "two")) bad = "could not write b.txt";
        break;
    case 3:
        if (!has("/save/a.txt", "one")) bad = "a.txt (restored, not rewritten) was dropped";
        else if (!has("/save/b.txt", "two")) bad = "b.txt not restored";
        break;
    default:
        bad = "no visit number";
    }
    if (bad) printf("probe: FAIL visit %d: %s\n", visit, bad);
    else printf("probe: ok %d\n", visit);
    fflush(stdout);
    return bad ? 1 : 0;
}
