/* liba.dll: named, ordinal-only and data exports, plus forwarders (see
 * liba.def) to libb by name, to libb by ordinal and to a system DLL. It also
 * imports from libb itself, so the closure is two deep. */
#include <windows.h>
int b_second(int x);
int a_counter = 41;
static int (*volatile hook)(int) = b_second;
int a_add(int a, int b) { return a + b + hook(0) + 1; }
int a_ord(int x) { return x + a_counter; }
BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, void *reserved) { return TRUE; }
