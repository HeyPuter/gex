/* xcbig.dll: a thousand distinct small functions its export `work` runs in a
 * row, code that must be translated (a page or more per few functions).
 * f_i(x): x = x * (2i + 3) + 7i + 1; then x ^= x >> (i % 13 + 1) when bit
 * i % 16 is set, else x += 31i + 5; then x & 3 rounds of
 * x = (x << 3) ^ (x >> (i % 7 + 2)) ^ i (all mod 2^32).
 * gex-nt-arch: x64
 * gex-nt-shared */
#include <windows.h>

static volatile unsigned sink;

#define F(name, i) \
	__attribute__((noinline)) static unsigned name(unsigned x) \
	{ \
		x = x * (2u * (i) + 3u) + (i) * 7u + 1u; \
		if (x & (1u << ((i) % 16))) x ^= x >> ((i) % 13 + 1); \
		else x += (i) * 31u + 5u; \
		for (int k = 0; k < (int)(x & 3); k++) x = (x << 3) ^ (x >> ((i) % 7 + 2)) ^ (unsigned)(i); \
		return x; \
	}
#define F3(a, b, c) F(f##a##b##c, 100 * a + 10 * b + c)
#define F2(a, b) F3(a, b, 0) F3(a, b, 1) F3(a, b, 2) F3(a, b, 3) F3(a, b, 4) F3(a, b, 5) F3(a, b, 6) F3(a, b, 7) F3(a, b, 8) F3(a, b, 9)
#define F1(a) F2(a, 0) F2(a, 1) F2(a, 2) F2(a, 3) F2(a, 4) F2(a, 5) F2(a, 6) F2(a, 7) F2(a, 8) F2(a, 9)
F1(0) F1(1) F1(2) F1(3) F1(4) F1(5) F1(6) F1(7) F1(8) F1(9)

#define C3(a, b, c) x = f##a##b##c(x);
#define C2(a, b) C3(a, b, 0) C3(a, b, 1) C3(a, b, 2) C3(a, b, 3) C3(a, b, 4) C3(a, b, 5) C3(a, b, 6) C3(a, b, 7) C3(a, b, 8) C3(a, b, 9)
#define C1(a) C2(a, 0) C2(a, 1) C2(a, 2) C2(a, 3) C2(a, 4) C2(a, 5) C2(a, 6) C2(a, 7) C2(a, 8) C2(a, 9)

__declspec(dllexport) unsigned work(unsigned x)
{
	C1(0) C1(1) C1(2) C1(3) C1(4) C1(5) C1(6) C1(7) C1(8) C1(9)
	sink = x;
	return x;
}

BOOL WINAPI DllMain(HINSTANCE h, DWORD r, void *p) { return TRUE; }
