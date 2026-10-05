// A C++ exception thrown by msvcp140 (std::_Xbad_alloc), caught here by
// type, and its what() called through msvcp140's vtable (a thiscall entry
// into the DLL); msvcp140's own exception object copied and destroyed by
// the runtime. Built by build.sh like cxxeh.cpp.
extern "C" {
__declspec(dllimport) int __cdecl printf(const char *, ...);
__declspec(dllimport) void __cdecl exit(int);
__declspec(dllimport) void *__stdcall LoadLibraryA(const char *);
__declspec(dllimport) void *__stdcall GetProcAddress(void *, const char *);
__declspec(dllimport) int __cdecl strcmp(const char *, const char *);
int _fltused = 0;
}
extern "C" const void *const type_info_vftable[2] __asm__("??_7type_info@@6B@") = {0, 0};

namespace std {
class exception {
public:
    virtual ~exception();
    virtual const char *what() const;
private:
    const char *name;
    int do_free;
};
class bad_alloc : public exception {
public:
    virtual ~bad_alloc();
};
}

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

int entry(void)
{
    void *msvcp = LoadLibraryA("msvcp140.dll");
    void (__cdecl *xbad_alloc)(void) = (void (__cdecl *)(void))GetProcAddress(msvcp, "?_Xbad_alloc@std@@YAXXZ");
    CHECK("msvcp140 and std::_Xbad_alloc", msvcp && xbad_alloc);
    int caught = 0;
    if (xbad_alloc)
    {
        try { xbad_alloc(); }
        catch (const std::bad_alloc &e)
        {
            const char *w = e.what();
            caught = 1;
            CHECK("what() through msvcp140's vtable", w && !strcmp(w, "bad allocation"));
        }
        catch (...) { caught = 2; }
    }
    CHECK("caught as std::bad_alloc", caught == 1);
    printf("%d failures\n", failures);
    exit(failures);
    return failures;
}
