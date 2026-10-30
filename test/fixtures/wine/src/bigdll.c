/* A DLL of FUNCS (a multiple of 100, at most 1000) distinct small functions its
 * export `work` runs in a row, code that must be translated (a page or more
 * per few functions). bigdll.mjs `expect` says what it returns.
 * f_i(x): x = x * (2i + 3) + 7i + 1; then x ^= x >> (i % 13 + 1) when bit
 * i % 16 is set, else x += 31i + 5; then x & 3 rounds of
 * x = (x << 3) ^ (x >> (i % 7 + 2)) ^ i (all mod 2^32).
 * Built as each of these:
 * - xcbig: wine/xcache's DLL;
 * - xca, xcb: two that trade places (wine/xcache2);
 * - swapN: three that take turns at one address (wine/dllswap);
 * - plugNN: 24 distinct images at distinct bases (wine/dllcycle).
 * gex-nt-variant: xcbig -DFUNCS=1000
 * gex-nt-variant: xca -DFUNCS=1000
 * gex-nt-variant: xcb -DFUNCS=700
 * gex-nt-variant: swap1000 -DFUNCS=1000 -Wl,--image-base=0x340000000
 * gex-nt-variant: swap700 -DFUNCS=700 -Wl,--image-base=0x340000000
 * gex-nt-variant: swap400 -DFUNCS=400 -Wl,--image-base=0x340000000
 * gex-nt-variant: plug00 -DFUNCS=1000 -Wl,--image-base=0x300000000
 * gex-nt-variant: plug01 -DFUNCS=1000 -Wl,--image-base=0x301000000
 * gex-nt-variant: plug02 -DFUNCS=1000 -Wl,--image-base=0x302000000
 * gex-nt-variant: plug03 -DFUNCS=1000 -Wl,--image-base=0x303000000
 * gex-nt-variant: plug04 -DFUNCS=1000 -Wl,--image-base=0x304000000
 * gex-nt-variant: plug05 -DFUNCS=1000 -Wl,--image-base=0x305000000
 * gex-nt-variant: plug06 -DFUNCS=1000 -Wl,--image-base=0x306000000
 * gex-nt-variant: plug07 -DFUNCS=1000 -Wl,--image-base=0x307000000
 * gex-nt-variant: plug08 -DFUNCS=1000 -Wl,--image-base=0x308000000
 * gex-nt-variant: plug09 -DFUNCS=1000 -Wl,--image-base=0x309000000
 * gex-nt-variant: plug10 -DFUNCS=1000 -Wl,--image-base=0x30a000000
 * gex-nt-variant: plug11 -DFUNCS=1000 -Wl,--image-base=0x30b000000
 * gex-nt-variant: plug12 -DFUNCS=1000 -Wl,--image-base=0x30c000000
 * gex-nt-variant: plug13 -DFUNCS=1000 -Wl,--image-base=0x30d000000
 * gex-nt-variant: plug14 -DFUNCS=1000 -Wl,--image-base=0x30e000000
 * gex-nt-variant: plug15 -DFUNCS=1000 -Wl,--image-base=0x30f000000
 * gex-nt-variant: plug16 -DFUNCS=1000 -Wl,--image-base=0x310000000
 * gex-nt-variant: plug17 -DFUNCS=1000 -Wl,--image-base=0x311000000
 * gex-nt-variant: plug18 -DFUNCS=1000 -Wl,--image-base=0x312000000
 * gex-nt-variant: plug19 -DFUNCS=1000 -Wl,--image-base=0x313000000
 * gex-nt-variant: plug20 -DFUNCS=1000 -Wl,--image-base=0x314000000
 * gex-nt-variant: plug21 -DFUNCS=1000 -Wl,--image-base=0x315000000
 * gex-nt-variant: plug22 -DFUNCS=1000 -Wl,--image-base=0x316000000
 * gex-nt-variant: plug23 -DFUNCS=1000 -Wl,--image-base=0x317000000
 * gex-nt-arch: x64
 * gex-nt-shared */
#include <windows.h>

#ifndef FUNCS
#define FUNCS 1000
#endif

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
#if FUNCS > 000
F1(0)
#endif
#if FUNCS > 100
F1(1)
#endif
#if FUNCS > 200
F1(2)
#endif
#if FUNCS > 300
F1(3)
#endif
#if FUNCS > 400
F1(4)
#endif
#if FUNCS > 500
F1(5)
#endif
#if FUNCS > 600
F1(6)
#endif
#if FUNCS > 700
F1(7)
#endif
#if FUNCS > 800
F1(8)
#endif
#if FUNCS > 900
F1(9)
#endif

#define C3(a, b, c) x = f##a##b##c(x);
#define C2(a, b) C3(a, b, 0) C3(a, b, 1) C3(a, b, 2) C3(a, b, 3) C3(a, b, 4) C3(a, b, 5) C3(a, b, 6) C3(a, b, 7) C3(a, b, 8) C3(a, b, 9)
#define C1(a) C2(a, 0) C2(a, 1) C2(a, 2) C2(a, 3) C2(a, 4) C2(a, 5) C2(a, 6) C2(a, 7) C2(a, 8) C2(a, 9)

__declspec(dllexport) unsigned work(unsigned x)
{
#if FUNCS > 000
	C1(0)
#endif
#if FUNCS > 100
	C1(1)
#endif
#if FUNCS > 200
	C1(2)
#endif
#if FUNCS > 300
	C1(3)
#endif
#if FUNCS > 400
	C1(4)
#endif
#if FUNCS > 500
	C1(5)
#endif
#if FUNCS > 600
	C1(6)
#endif
#if FUNCS > 700
	C1(7)
#endif
#if FUNCS > 800
	C1(8)
#endif
#if FUNCS > 900
	C1(9)
#endif
	sink = x;
	return x;
}

BOOL WINAPI DllMain(HINSTANCE h, DWORD r, void *p) { return TRUE; }
