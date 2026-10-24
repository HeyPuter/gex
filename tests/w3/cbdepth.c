/* Guest callbacks from native Wine nested deep: qsort (msvcrt, native wasm
 * in W3) calls a comparator that sorts again, N levels down (argv[1]). On
 * tier-1 each level is a call-out, which cannot be left: a guest call from
 * native Wine past the host's depth limit ends the process with one line,
 * not a hang. Prints "cbdepth ok N" and exits 0 when every level returned
 * in order.
 */
#include <stdio.h>
#include <stdlib.h>

static int left, want, seen;

static int cmp(const void *a, const void *b);
static void sort(void) { int v[] = {2, 1}; qsort(v, 2, sizeof v[0], cmp); if (v[0] != 1) seen = -1000000; }
static int cmp(const void *a, const void *b)
{
    if (left > 0) { left--; sort(); if (seen >= 0) seen++; }
    return *(const int *)a - *(const int *)b;
}

int main(int argc, char **argv)
{
    want = left = argc > 1 ? atoi(argv[1]) : 100;
    sort();
    if (seen != want) { printf("cbdepth FAIL: %d of %d levels returned\n", seen, want); return 1; }
    printf("cbdepth ok %d\n", want);
    return 0;
}
