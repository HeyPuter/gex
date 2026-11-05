/* A function pointer handed to another thread with no system call between:
 * the main thread loads a DLL (xthr_dll.c) and stores a pointer to one of
 * its functions in a variable a second thread spins on, never entering the
 * kernel; that thread calls it as soon as it sees it. In a native program
 * the pointer is a function-table index of a module installed after the
 * spinning thread last caught up with the table: the call has to reach the
 * function all the same (the ICALL pass's bound sends it through gex's
 * call-out, which catches the thread up). Then the same through a pointer
 * the DLL hands out from its own thread (a callback registered with it). */
#include <windows.h>
#include <stdio.h>

static int failures;
static void check(int ok, const char *what, int got)
{
    printf("%s %s (%d)\n", ok ? "ok" : "FAIL", what, got);
    if (!ok) failures++;
}

static int (*volatile g_fn)(int);
static volatile int spinning, result;

static DWORD WINAPI spin(void *arg)
{
    int (*p)(int);
    spinning = 1;
    while (!(p = g_fn)) ;
    result = p(41);
    return 0;
}

int main(int argc, char **argv)
{
    HANDLE t = CreateThread(NULL, 0, spin, NULL, 0, NULL);
    int (*(*get)(void))(int);
    int (*twice)(int (*)(int), int);
    HMODULE m;

    while (!spinning) Sleep(1);
    Sleep(100);
    if (!(m = LoadLibraryA("xthr_dll.dll")))
    {
        printf("FAIL LoadLibrary xthr_dll.dll: %lu\n0 failures\n", GetLastError());
        return 1;
    }
    get = (void *)GetProcAddress(m, "xthr_get");
    twice = (void *)GetProcAddress(m, "xthr_twice");
    if (!get || !twice)
    {
        printf("FAIL GetProcAddress\n");
        return 1;
    }
    g_fn = get();
    if (WaitForSingleObject(t, 20000)) check(0, "the spinning thread's call returned", -1);
    else check(result == 42, "a just-loaded DLL's function called from a thread with no system call since", result);
    check(twice(get(), 5) == 7, "the DLL calls a pointer to its own function", twice(get(), 5));
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
