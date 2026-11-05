/* A gexcc-win program's calls of DLL functions that no module of Wine's own
 * native PE side imports: the core links them in and exports them only when
 * it is linked with every DLL export (wine/dist.sh's default). Built apart
 * from the core and run against the dist's stripped release modules, as a
 * site's programs are. Without them a call stops the program with "DLL!name:
 * called before the module defining it was loaded". Prints "ok ..." lines and
 * "0 failures". */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <process.h>
#include <io.h>
#include <direct.h>
#include <sys/timeb.h>

static int failures;
#define CHECK(c, ...) do { if (c) printf("ok "); else { printf("FAIL "); failures++; } printf(__VA_ARGS__); printf("\n"); } while (0)

int main(void)
{
    FILETIME c, e, k, u;
    WCHAR low[8];
    char buf[32], cwd[MAX_PATH];
    struct __timeb64 tb;

    setvbuf(stdout, NULL, _IONBF, 0);
    SetLastError(0x1234);
    CHECK(GetLastError() == 0x1234, "SetLastError, then GetLastError %#lx", GetLastError());
    CHECK(GetProcessTimes(GetCurrentProcess(), &c, &e, &k, &u), "GetProcessTimes");
    CHECK(LCMapStringEx(LOCALE_NAME_INVARIANT, LCMAP_LOWERCASE, L"ABC", -1, low, 8, NULL, NULL, 0) == 4 && !wcscmp(low, L"abc"),
          "LCMapStringEx lowercases ABC");
    /* bound is the point: a gex process may refuse the affinity itself */
    CHECK(1, "SetThreadAffinityMask returns (%#llx)", (unsigned long long)SetThreadAffinityMask(GetCurrentThread(), 1));
    CHECK(_getpid() == (int)GetCurrentProcessId(), "_getpid %d", _getpid());
    CHECK(!_access("C:\\windows", 0), "_access C:\\windows");
    CHECK(_getcwd(cwd, sizeof(cwd)) && !_chdir("C:\\windows") && !_chdir(cwd), "_chdir there and back");
    _ftime64(&tb);
    CHECK(tb.time > 1700000000, "_ftime64 %lld", (long long)tb.time);
    CHECK(sprintf_s(buf, sizeof(buf), "%d-%s", 42, "x") == 4 && !strcmp(buf, "42-x"), "sprintf_s %s", buf);
    printf("%d failures\n", failures);
    return failures;
}
