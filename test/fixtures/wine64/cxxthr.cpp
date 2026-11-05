/* C++ exceptions and thread_local data on several threads of a gexcc-win
 * program: each thread's instance of the program's module needs a
 * thread-local block of its own (the C++ runtime's exception globals and
 * landing-pad context live there), not only the first instance's. */
#include <windows.h>
#include <cstdio>
#include <stdexcept>

static thread_local int mine;
static volatile LONG bad;

static DWORD WINAPI work(void *arg)
{
    int id = (int)(INT_PTR)arg;
    mine = id * 1000;
    for (int i = 0; i < 300; i++) {
        try {
            if (i & 1) throw std::runtime_error("odd");
            throw id * 1000 + i;
        } catch (int v) {
            if (v != id * 1000 + i) InterlockedIncrement(&bad);
        } catch (const std::exception &e) {
            if (!(i & 1)) InterlockedIncrement(&bad);
        }
        if (mine != id * 1000) InterlockedIncrement(&bad);
    }
    return 0;
}

int main()
{
    HANDLE t[4];
    int failures = 0;
    for (int k = 0; k < 4; k++) t[k] = CreateThread(NULL, 0, work, (void *)(INT_PTR)(k + 1), 0, NULL);
    work((void *)(INT_PTR)9);
    WaitForMultipleObjects(4, t, TRUE, INFINITE);
    if (bad) failures++;
    std::printf("%s throws caught and thread_local data kept on 5 threads (%ld bad)\n", bad ? "FAIL" : "ok", (long)bad);
    std::printf("%d failures\n", failures);
    return failures != 0;
}
