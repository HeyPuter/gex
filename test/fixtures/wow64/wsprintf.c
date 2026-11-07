/* test/wow64/wsprintf: user32's wsprintfA and wsprintfW, varargs exports
 * whose .spec calls the buffer a string as well as the format, format
 * integers, a string and a character as Windows does. Prints
 * "wsprintf ok", or each mismatch. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

int main(void)
{
    char a[128], wa[256];
    WCHAR w[128];
    int bad = 0, n;

    n = wsprintfA(a, "%d|%u|%x|%s|%c|%d", 12345, 7u, 0xab, "str", 'q', -2);
    if (strcmp(a, "12345|7|ab|str|q|-2") || n != 19) { printf("wsprintfA: '%s' (%d)\n", a, n); bad = 1; }
    memset(w, 0x55, sizeof(w));
    n = wsprintfW(w, L"%d|%u|%x|%s|%c|%d", 12345, 7u, 0xab, L"str", L'q', -2);
    WideCharToMultiByte(CP_ACP, 0, w, -1, wa, sizeof(wa), NULL, NULL);
    if (strcmp(wa, "12345|7|ab|str|q|-2") || n != 19) { printf("wsprintfW: '%s' (%d)\n", wa, n); bad = 1; }
    if (!bad) printf("wsprintf ok\n");
    fflush(stdout);
    return bad;
}
