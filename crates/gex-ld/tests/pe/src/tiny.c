/* tiny.exe: no CRT, a relocated pointer and two kernel32 imports; built at a
 * high PE32+ base (0x140000000) with and without a .reloc section. */
#include <windows.h>
static const char message[] = "tiny\n";
static const char *volatile anchor = message;
int entry(void) {
    DWORD written;
    WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), anchor, sizeof message - 1, &written, 0);
    return 0;
}
