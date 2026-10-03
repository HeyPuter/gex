/* A DLL linked at a fixed base without DYNAMIC_BASE (fixedbase.c loads it).
 * gex-nt-dll: 0x18000000 */
#include <windows.h>
__declspec(dllexport) int fixedbase_value = 1234;
__declspec(dllexport) int *fixedbase_ptr(void) { return &fixedbase_value; }
