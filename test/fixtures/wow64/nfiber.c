/* Fibers from native code (test/wow64/nfiber), built with gexcc-win: a
 * fiber is guest state, so a native start routine is refused, by
 * CreateFiber and CreateFiberEx alike, with ERROR_NOT_SUPPORTED. Prints
 * "nfiber refused E1 E2" (each call's last error, 0 for a fiber made).
 * With "switch", the thread made a fiber switches to itself from native
 * code, which ends the process with a message. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static void WINAPI routine(void *arg)
{
    (void)arg;
    printf("nfiber: the native routine ran\n");
}

int main(int argc, char **argv)
{
    void *self = ConvertThreadToFiber(NULL), *f;
    DWORD e1 = 0, e2 = 0;

    if (!self) { printf("nfiber: no ConvertThreadToFiber %lu\n", GetLastError()); return 1; }
    if (argc > 1 && !strcmp(argv[1], "switch"))
    {
        fflush(stdout);
        SwitchToFiber(self);
        printf("nfiber: switched\n");
        return 2;
    }
    SetLastError(0);
    if (!(f = CreateFiber(0, routine, NULL))) e1 = GetLastError(); else DeleteFiber(f);
    SetLastError(0);
    if (!(f = CreateFiberEx(0, 0, 0, routine, NULL))) e2 = GetLastError(); else DeleteFiber(f);
    printf("nfiber refused %lu %lu\n", e1, e2);
    return 0;
}
