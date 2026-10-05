/* A stand-in winhttp.dll beside loadorder.exe, as a game ships a proxy DLL
 * (Rain World's BepInEx doorstop). loadorder.c checks which one it got.
 * gex-nt-dll: 0x19000000 */
#include <windows.h>
__declspec(dllexport) int gex_loadorder_marker(void) { return 42; }
