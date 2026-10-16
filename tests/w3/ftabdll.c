/* tests/w3/ftabstale.sh's DLL: enough functions that its unit's install
 * outruns a small function-table room (GEX_TEST_FTAB_SPARE). */
#include <windows.h>

#define F(n) __attribute__((noinline)) static unsigned f##n(unsigned x) { return x * (n + 3) + (x >> (n % 7)); }
#define F10(n) F(n##0) F(n##1) F(n##2) F(n##3) F(n##4) F(n##5) F(n##6) F(n##7) F(n##8) F(n##9)
F10(1) F10(2) F10(3) F10(4) F10(5) F10(6) F10(7) F10(8) F10(9)
#define C(n) x = f##n(x);
#define C10(n) C(n##0) C(n##1) C(n##2) C(n##3) C(n##4) C(n##5) C(n##6) C(n##7) C(n##8) C(n##9)

__declspec(dllexport) unsigned ftab_sum(unsigned x)
{
    C10(1) C10(2) C10(3) C10(4) C10(5) C10(6) C10(7) C10(8) C10(9)
    return x;
}

BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, void *reserved) { return TRUE; }
