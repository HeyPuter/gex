/* OutputDebugStringA/W with no debugger: returns, and what the caller can
 * see afterwards (the last error) is the
 * same with the debug_string provider as with Wine's own code
 * (tests/providers/run.sh). Built with i686-w64-mingw32-gcc -O2 -s. */
#include <windows.h>
#include <stdio.h>

int main(void) {
    SetLastError(0x1234);
    OutputDebugStringA("odstr: A\n");
    printf("after A: last error %lu\n", GetLastError());
    SetLastError(0x1234);
    OutputDebugStringW(L"odstr: W\n");
    printf("after W: last error %lu\n", GetLastError());
    for (int i = 0; i < 100; i++) OutputDebugStringA("");
    printf("loop done\n");
    return 0;
}
