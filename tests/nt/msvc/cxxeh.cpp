// MSVC's i386 C++ exceptions and SEH through msvcrt.dll: __CxxFrameHandler3,
// _CxxThrowException, catch by type (with a copy constructor and a virtual
// destructor run by msvcrt), destructors while unwinding, rethrow,
// catch(...), __try/__except (_except_handler3), _setjmp3/longjmp, and many
// throws in a row (nothing may pile up per exception). Built with clang-cl
// and lld-link against an import library from msvcrt.def (build.sh): no
// MSVC headers or libraries. The x64 build (build-x64.sh, x64/msvc_cxxeh)
// is the same through x64's table-based handlers: __CxxFrameHandler3's
// funclets, __C_specific_handler, _setjmp with a frame and an unwinding
// longjmp.
extern "C" {
__declspec(dllimport) int __cdecl printf(const char *, ...);
__declspec(dllimport) void __cdecl exit(int);
#ifdef _M_X64
__declspec(dllimport) int __cdecl _setjmp(void *, void *) __attribute__((returns_twice));
#else
__declspec(dllimport) int __cdecl _setjmp3(void *, int, ...);
#endif
__declspec(dllimport) void __cdecl longjmp(void *, int);
__declspec(dllimport) void __stdcall RaiseException(unsigned long, unsigned long, unsigned long, const unsigned long *);
}

/* What the MSVC runtime libraries would supply. type_info's vtable is only
 * referenced by the type descriptors (catch matching compares names). */
extern "C" int _fltused = 0;
extern "C" const void *const type_info_vftable[2] __asm__("??_7type_info@@6B@") = {0, 0};
void operator delete(void *, decltype(sizeof 0)) noexcept {}

static int failures, dtors, copies;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

struct Guard { int *n; ~Guard() { ++*n; } };
struct Base { int v; Base(int x) : v(x) {} Base(const Base &o) : v(o.v) { copies++; } virtual ~Base() { dtors++; } virtual int kind() const { return 1; } };
struct Derived : Base { Derived(int x) : Base(x) {} Derived(const Derived &o) : Base(o) {} int kind() const override { return 2; } };

__declspec(noinline) static void thrower(int what) {
    int local = 0;
    Guard g{&local};
    if (what == 1) throw 42;
    if (what == 2) throw Derived(7);
    if (what == 3) throw 1.5;
}

__declspec(noinline) static int depth(int n) {
    int unwound = 0;
    Guard g{&unwound};
    if (n == 0) throw n + 100;
    return depth(n - 1) + 1;
}

static int filter_div(unsigned long code) { return code == 0xc0000094 ? 1 : 0; }

/* SEH in a function of its own: clang-cl allows no C++ try beside __try. */
__declspec(noinline) static void seh_tests(void) {
    volatile int zero = 0;
    int seh = 0;
    __try { seh = 10 / zero; } __except (filter_div(_exception_code())) { seh = 1; }
    CHECK("__try/__except a fault (divide by zero), with a filter", seh == 1);
    seh = 0;
    __try { RaiseException(0xe0001234, 0, 0, 0); } __except (1) { seh = 2; }
    CHECK("__except a RaiseException", seh == 2);

}

int entry(void) {
    try { thrower(1); CHECK("throw int", 0); } catch (int v) { CHECK("catch int", v == 42); }

    int n = dtors;
    try { thrower(2); } catch (const Base &b) { CHECK("catch a derived class by base reference", b.v == 7 && b.kind() == 2); }
    CHECK("the thrown object's destructor ran", dtors > n);

    try { thrower(3); } catch (int) { CHECK("catch(int) skipped for a double", 0); } catch (double d) { CHECK("catch double", d == 1.5); }

    try { try { thrower(1); } catch (int) { throw; } } catch (int v) { CHECK("rethrow", v == 42); }
    try { thrower(2); } catch (...) { CHECK("catch(...)", 1); }

    int got = 0;
    Guard outer{&got};
    try { depth(20); } catch (int v) { CHECK("through 21 frames", v == 100); }

    int caught = 0;
    for (int i = 0; i < 20000; i++) {
        try { thrower(1 + (i & 1)); } catch (int) { caught++; } catch (const Base &) { caught++; }
    }
    CHECK("20000 throws", caught == 20000);

    seh_tests();

    volatile int jumps = 0;
#ifdef _M_X64
    alignas(16) unsigned long long jb[32];
    if (_setjmp(jb, __builtin_frame_address(0)) == 0) { jumps++; longjmp(jb, 5); }
#else
    int jb[16];
    if (_setjmp3(jb, 0) == 0) { jumps++; longjmp(jb, 5); }
#endif
    CHECK("setjmp/longjmp", jumps == 1);

    printf("%d failures\n", failures);
    exit(failures);
    return failures;
}
