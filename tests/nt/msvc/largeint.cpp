// ntdll's i386 compiler helpers: _alldiv/_aulldiv/_allrem/_aullrem (the
// compiler's 64-bit division, stdcall), _allshl/_allshr/_aullshr (EDX:EAX
// and CL), _alldvrm (remainder in EBX:ECX), RtlUlongByteSwap (fastcall).
// Built by build.sh like cxxeh.cpp.
extern "C" {
__declspec(dllimport) int __cdecl printf(const char *, ...);
__declspec(dllimport) void __cdecl exit(int);
__declspec(dllimport) void *__stdcall GetModuleHandleA(const char *);
__declspec(dllimport) void *__stdcall GetProcAddress(void *, const char *);
int _fltused = 0;
}

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

__declspec(noinline) static long long sdiv(long long a, long long b) { return a / b; }
__declspec(noinline) static unsigned long long udiv(unsigned long long a, unsigned long long b) { return a / b; }
__declspec(noinline) static long long srem(long long a, long long b) { return a % b; }

typedef void (*fn)(void);

__declspec(noinline) static unsigned long long shift(fn f, unsigned long long v, unsigned char n)
{
    unsigned int lo = (unsigned int)v, hi = (unsigned int)(v >> 32), rlo, rhi;
    __asm { mov eax, lo
            mov edx, hi
            mov cl, n
            call f
            mov rlo, eax
            mov rhi, edx }
    return ((unsigned long long)rhi << 32) | rlo;
}

__declspec(noinline) static long long dvrm(fn f, long long a, long long b, long long *rem)
{
    unsigned int alo = (unsigned int)a, ahi = (unsigned int)(a >> 32), blo = (unsigned int)b, bhi = (unsigned int)(b >> 32);
    unsigned int qlo, qhi, rlo, rhi;
    __asm { push ebx
            push bhi
            push blo
            push ahi
            push alo
            call f
            mov qlo, eax
            mov qhi, edx
            mov rlo, ecx
            mov rhi, ebx
            pop ebx }
    *rem = (long long)(((unsigned long long)rhi << 32) | rlo);
    return (long long)(((unsigned long long)qhi << 32) | qlo);
}

__declspec(noinline) static unsigned int bswap(fn f, unsigned int v)
{
    unsigned int r;
    __asm { mov ecx, v
            call f
            mov r, eax }
    return r;
}

int entry(void)
{
    void *ntdll = GetModuleHandleA("ntdll.dll");
    CHECK("_alldiv", sdiv(-100000000000ll, 7) == -14285714285ll);
    CHECK("_aulldiv", udiv(0xfffffffffffffff0ull, 16) == 0x0fffffffffffffffull);
    CHECK("_allrem", srem(-100000000000ll, 7) == -5);
    CHECK("_allshl", shift((fn)GetProcAddress(ntdll, "_allshl"), 1, 40) == (1ull << 40));
    CHECK("_allshr (signed)", shift((fn)GetProcAddress(ntdll, "_allshr"), 0x8000000000000000ull, 4) == 0xf800000000000000ull);
    CHECK("_aullshr", shift((fn)GetProcAddress(ntdll, "_aullshr"), 0x8000000000000000ull, 63) == 1);
    long long rem = 0;
    CHECK("_alldvrm", dvrm((fn)GetProcAddress(ntdll, "_alldvrm"), -100000000001ll, 7, &rem) == -14285714285ll && rem == -6);
    CHECK("RtlUlongByteSwap (fastcall)", bswap((fn)GetProcAddress(ntdll, "RtlUlongByteSwap"), 0x11223344) == 0x44332211);
    printf("%d failures\n", failures);
    exit(failures);
    return failures;
}
