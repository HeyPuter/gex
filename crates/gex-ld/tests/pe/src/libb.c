/* libb.dll: the target of liba's forwarders. `b_hidden` is exported by
 * ordinal only (NONAME in libb.def). */
#include <windows.h>
__declspec(dllexport) int b_value(int x) { return x * 5; }
static int (*volatile self_ref)(int) = b_value; /* an absolute pointer, so a .reloc */
int b_second(int x) { return self_ref(x) - x * 5 + x - 1; }
int b_hidden(int x) { return x ^ 0x55; }
BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, void *reserved) { return TRUE; }
