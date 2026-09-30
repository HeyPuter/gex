/* delayed.dll: only ever reached through uses.exe's delay-import table. */
#include <windows.h>
__declspec(dllexport) int d_mul(int a, int b) { return a * b; }
static int negate(int a) { return -a; }
static int (*volatile negator)(int) = negate; /* an absolute pointer, so a .reloc */
__declspec(dllexport) int d_neg(int a) { return negator(a); }
BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, void *reserved) { return TRUE; }
