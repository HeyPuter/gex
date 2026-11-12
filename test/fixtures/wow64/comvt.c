/* Guest code calling Wine's native functions through vtables and exports,
 * built for i386 and for x86-64. Vtables are the native modules' entry path
 * (guest calls to table indices); GetProcAddress gives the export path.
 * - IStream (CreateStreamOnHGlobal): stdcall methods, LARGE_INTEGER and
 *   ULARGE_INTEGER by value (two stack words on i386), out pointers;
 * - IMalloc (CoGetMalloc): SIZE_T in and out;
 * - msvcrt's C++ exception: its constructor (an export) and what() and the
 *   deleting destructor through its vtable, thiscall on i386 (this in ECX);
 * - exports of every i386 convention where the arch has them: cdecl with
 *   a 64-bit and a floating-point result (msvcrt), stdcall (MulDiv),
 *   fastcall (RtlUlongByteSwap, RtlUlonglongByteSwap), and a 64-bit struct
 *   result (RtlConvertLongToLargeInteger).
 * Prints "ok ..." per check and "N failures". */
#define COBJMACROS
#include <windows.h>
#include <objbase.h>
#include <stdio.h>
#include <string.h>

#ifdef __i386__
#define THISCALL __attribute__((thiscall))
#else
#define THISCALL
#endif
/* (FASTCALL: winnt.h's __fastcall, nothing on x86-64) */

static int failures;
static void check(int ok, const char *what, long long got)
{
    printf("%s %s (%lld)\n", ok ? "ok" : "FAIL", what, got);
    if (!ok) failures++;
}

static void stream(void)
{
    IStream *s;
    LARGE_INTEGER off;
    ULARGE_INTEGER pos, size;
    STATSTG st;
    ULONG n = 0;
    char buf[32];
    HRESULT hr = CreateStreamOnHGlobal(NULL, TRUE, &s);

    check(hr == S_OK, "IStream: created", hr);
    if (hr != S_OK) return;
    check(IStream_Write(s, "0123456789abcdef", 16, &n) == S_OK && n == 16, "IStream::Write", n);
    off.QuadPart = -6;
    hr = IStream_Seek(s, off, STREAM_SEEK_END, &pos);
    check(hr == S_OK && pos.QuadPart == 10, "IStream::Seek, LARGE_INTEGER by value from the end", (long long)pos.QuadPart);
    memset(buf, 0, sizeof buf);
    check(IStream_Read(s, buf, 4, &n) == S_OK && n == 4 && !memcmp(buf, "abcd", 4), "IStream::Read after the seek", n);
    size.QuadPart = 12;
    check(IStream_SetSize(s, size) == S_OK, "IStream::SetSize, ULARGE_INTEGER by value", 0);
    hr = IStream_Stat(s, &st, STATFLAG_NONAME);
    check(hr == S_OK && st.cbSize.QuadPart == 12, "IStream::Stat sees the size", (long long)st.cbSize.QuadPart);
    off.QuadPart = 3;
    IStream_Seek(s, off, STREAM_SEEK_SET, &pos);
    off.QuadPart = -1;  /* both words of the offset matter: 0xffffffff above */
    hr = IStream_Seek(s, off, STREAM_SEEK_CUR, &pos);
    check(hr == S_OK && pos.QuadPart == 2, "IStream::Seek back from the current position", (long long)pos.QuadPart);
    check(IStream_Release(s) == 0, "IStream::Release", 0);
}

static void malloc_vt(void)
{
    IMalloc *m;
    void *p;

    check(CoGetMalloc(1, &m) == S_OK, "IMalloc: CoGetMalloc", 0);
    p = IMalloc_Alloc(m, 1000);
    check(p != NULL, "IMalloc::Alloc", (long long)(INT_PTR)p);
    check(IMalloc_GetSize(m, p) >= 1000, "IMalloc::GetSize", (long long)IMalloc_GetSize(m, p));
    IMalloc_Free(m, p);
    IMalloc_Release(m);
}

/* msvcrt's exception: { vtable, name, do_free } */
struct exc { const void *const *vtable; const char *name; int do_free; };
typedef struct exc *(THISCALL *ctor_fn)(struct exc *, const char *const *);
typedef const char *(THISCALL *what_fn)(struct exc *);
typedef void *(THISCALL *dtor_fn)(struct exc *, unsigned int);

static void cxx(HMODULE crt)
{
#ifdef __i386__
    ctor_fn ctor = (ctor_fn)GetProcAddress(crt, "??0exception@@QAE@ABQBD@Z");
#else
    ctor_fn ctor = (ctor_fn)GetProcAddress(crt, "??0exception@@QEAA@AEBQEBD@Z");
#endif
    struct exc e;
    const char *msg = "comvt message";

    check(ctor != NULL, "msvcrt exception's constructor exported", 0);
    if (!ctor) return;
    memset(&e, 0, sizeof e);
    check(ctor(&e, &msg) == &e, "exception::exception(const char *const &), thiscall export", 0);
    check(e.vtable != NULL, "exception's vtable", 0);
    if (!e.vtable) return;
    {
        const char *w = ((what_fn)e.vtable[1])(&e);
        check(w && !strcmp(w, "comvt message"), "exception::what through the vtable", w ? (long long)strlen(w) : -1);
    }
    /* the deleting destructor, without delete (flags 0): returns this */
    check(((dtor_fn)e.vtable[0])(&e, 0) == &e, "the destructor through the vtable", 0);
}

static void exports(HMODULE crt)
{
    HMODULE nt = GetModuleHandleA("ntdll.dll"), k32 = GetModuleHandleA("kernel32.dll");
    long long (__cdecl *atoi64)(const char *) = (void *)GetProcAddress(crt, "_atoi64");
    double (__cdecl *ldexp_)(double, int) = (void *)GetProcAddress(crt, "ldexp");
    int (WINAPI *muldiv)(int, int, int) = (void *)GetProcAddress(k32, "MulDiv");
    ULONG (FASTCALL *ulswap)(ULONG) = (void *)GetProcAddress(nt, "RtlUlongByteSwap");
    ULONGLONG (FASTCALL *ullswap)(ULONGLONG) = (void *)GetProcAddress(nt, "RtlUlonglongByteSwap");
    LARGE_INTEGER (WINAPI *l2li)(LONG) = (void *)GetProcAddress(nt, "RtlConvertLongToLargeInteger");

    check(atoi64 && atoi64("-81985529216486895") == -81985529216486895ll, "cdecl export, 64-bit result", atoi64 ? atoi64("12") : -1);
    check(ldexp_ && ldexp_(1.5, 4) == 24.0, "cdecl export, double argument and result", ldexp_ ? (long long)ldexp_(1.5, 4) : -1);
    check(muldiv && muldiv(300000, 300000, 70000) == 1285714, "stdcall export, a 64-bit intermediate", muldiv ? muldiv(300000, 300000, 70000) : -1);
    check(muldiv && muldiv(-7, 9, 2) == -32, "stdcall export, rounding", muldiv ? muldiv(-7, 9, 2) : -1);
#ifdef __i386__
    /* fastcall exports exist on i386 only */
    check(ulswap && ulswap(0x11223344) == 0x44332211, "fastcall export, the argument in ECX", ulswap ? ulswap(0x11223344) : -1);
    check(ullswap && ullswap(0x1122334455667788ull) == 0x8877665544332211ull, "fastcall export, a 64-bit argument on the stack", ullswap ? (long long)ullswap(1) : -1);
    check(l2li && l2li(-5).QuadPart == -5, "stdcall export, an 8-byte struct result in EDX:EAX", l2li ? l2li(-5).QuadPart : 0);
#else
    (void)ulswap; (void)ullswap; (void)l2li;
#endif
}

int main(void)
{
    HMODULE crt = LoadLibraryA("msvcrt.dll");

    CoInitialize(NULL);
    stream();
    malloc_vt();
    cxx(crt);
    exports(crt);
    CoUninitialize();
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
