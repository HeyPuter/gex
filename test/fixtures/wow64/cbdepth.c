/* Guest callbacks from native Wine nested deep: qsort (msvcrt, native wasm
 * in 32-bit Wine) calls a comparator that sorts again, N levels down (argv[1]). On
 * tier-1 each level is a call-out, which cannot be left: a guest call from
 * native Wine past the host's depth limit ends the process with one line,
 * not a hang. Prints "cbdepth ok N" and exits 0 when every level returned
 * in order. With a second argument "thread", the levels run on a thread
 * whose stack is 1 MiB (the least Wine gives one).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>

static int left, want, seen;

static int cmp(const void *a, const void *b);
static void sort(void) { int v[] = {2, 1}; qsort(v, 2, sizeof v[0], cmp); if (v[0] != 1) seen = -1000000; }
static int cmp(const void *a, const void *b)
{
    if (left > 0) { left--; sort(); if (seen >= 0) seen++; }
    return *(const int *)a - *(const int *)b;
}

static DWORD WINAPI run(void *arg) { sort(); return 0; }

int main(int argc, char **argv)
{
    want = left = argc > 1 ? atoi(argv[1]) : 100;
    if (argc > 2 && !strcmp(argv[2], "thread"))
    {
        HANDLE t = CreateThread(NULL, 1 << 20, run, NULL, STACK_SIZE_PARAM_IS_A_RESERVATION, NULL);
        WaitForSingleObject(t, INFINITE);
    }
    else sort();
    if (seen != want) { printf("cbdepth FAIL: %d of %d levels returned\n", seen, want); return 1; }
    printf("cbdepth ok %d\n", want);
    return 0;
}
