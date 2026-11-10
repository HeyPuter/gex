// A C++ rethrow from a catch block (`throw;`) unwinds the frames below the
// catch once: they were unwound when the catch was entered, and the
// rethrow's unwind goes on from the frame that caught, as x86-64's does.
// The innermost frame's __finally counts how often it is unwound; a second
// unwind of it is also a second destructor run under __CxxFrameHandler4,
// which keeps no state in the frame. Built with clang-cl and lld-link
// against import libraries from fixtures/wine/src's .def files (MSVC's x64
// ABI, no MSVC headers or libraries). Prints "ok"/"FAIL" lines, then
// "N failures"; the exit code is N.
extern "C" {
__declspec(dllimport) int __cdecl printf(const char *, ...);
__declspec(dllimport) void __cdecl exit(int);
}

extern "C" int _fltused = 0;
extern "C" const void *const type_info_vftable[2] __asm__("??_7type_info@@6B@") = {0, 0};
void operator delete(void *, decltype(sizeof 0)) noexcept {}

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static volatile int finallies, dtors;
struct Guard { ~Guard() { dtors++; } };

__declspec(noinline) static void thrower(int v) { throw v; }

/* SEH in a function of its own: clang-cl allows no C++ try beside __try. */
__declspec(noinline) static void inner(int v) {
    __try { thrower(v); } __finally { finallies++; }
}

__declspec(noinline) static void with_guard(int v) {
    Guard g;
    inner(v);
}

__declspec(noinline) static void rethrower(int v) {
    try { with_guard(v); } catch (int) { throw; }
}

__declspec(noinline) static void rethrower2(int v) {
    try { rethrower(v); } catch (int) { throw; }
}

int entry(void) {
    int got = 0;

    try { rethrower(42); } catch (int v) { got = v; }
    CHECK("rethrow caught", got == 42);
    CHECK("rethrow: the frames below the catch unwound once (__finally)", finallies == 1);
    CHECK("rethrow: and their destructors run once", dtors == 1);

    finallies = dtors = got = 0;
    try { rethrower2(7); } catch (int v) { got = v; }
    CHECK("rethrow of a rethrow caught", got == 7);
    CHECK("rethrow of a rethrow: the frames below unwound once", finallies == 1 && dtors == 1);

    printf("%d failures\n", failures);
    exit(failures);
    return failures;
}
