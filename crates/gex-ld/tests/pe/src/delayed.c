/* delayed.dll: only ever reached through uses.exe's delay-import table. */
#include <windows.h>
__declspec(dllexport) int d_mul(int a, int b) { return a * b; }
__declspec(dllexport) int d_neg(int a) { return -a; }
BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, void *reserved) { return TRUE; }
