// msvcrt's i386 register-convention math: _CI* (x87 ST0/ST1 in, ST0 out),
// _ftol (ST0 in, EDX:EAX out), __libm_sse2_* (XMM0/XMM1 in, XMM0 out),
// Built by build.sh like cxxeh.cpp. (_chkesp is not here: it reads the
// caller's ZF, which the function tier does not carry into a call.)
extern "C" {
__declspec(dllimport) int __cdecl printf(const char *, ...);
__declspec(dllimport) void __cdecl exit(int);
__declspec(dllimport) void __cdecl _CIsqrt(void);
__declspec(dllimport) void __cdecl _CIpow(void);
__declspec(dllimport) void __cdecl _CIatan2(void);
__declspec(dllimport) void __cdecl _ftol(void);
__declspec(dllimport) void __cdecl __libm_sse2_pow(void);
int _fltused = 0;
}

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

typedef void (__cdecl *fn)(void);

__declspec(noinline) static double ci1(fn f, double x)
{
    double r;
    __asm { fld x
            call f
            fstp r }
    return r;
}

__declspec(noinline) static double ci2(fn f, double x, double y)
{
    double r;
    __asm { fld x
            fld y
            call f
            fstp r }
    return r;
}

__declspec(noinline) static long long ftol(fn f, double x)
{
    unsigned int lo, hi;
    __asm { fld x
            call f
            mov lo, eax
            mov hi, edx }
    return (long long)(((unsigned long long)hi << 32) | lo);
}

__declspec(noinline) static double sse2(fn f, double x, double y)
{
    double r;
    __asm { movsd xmm0, x
            movsd xmm1, y
            call f
            movsd r, xmm0 }
    return r;
}

int entry(void)
{
    CHECK("_CIsqrt", ci1(_CIsqrt, 2.25) == 1.5);
    CHECK("_CIpow (ST1 to the power ST0)", ci2(_CIpow, 2.0, 10.0) == 1024.0);
    CHECK("_CIatan2 (ST1, ST0)", ci2(_CIatan2, 0.0, -1.0) > 3.14 && ci2(_CIatan2, 0.0, -1.0) < 3.15);
    CHECK("_ftol truncates", ftol(_ftol, -7.9) == -7 && ftol(_ftol, 5000000000.5) == 5000000000ll);
    CHECK("__libm_sse2_pow", sse2(__libm_sse2_pow, 3.0, 4.0) == 81.0);
    printf("%d failures\n", failures);
    exit(failures);
    return failures;
}
