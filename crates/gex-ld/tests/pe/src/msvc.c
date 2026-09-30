/* msvc.exe: the same program as tiny.c, for clang-cl and lld-link, which have
 * no Windows headers or MSVC libraries here: the two kernel32 functions are
 * declared by hand, and the import library comes from a .def. */
typedef void *HANDLE;
__declspec(dllimport) HANDLE __stdcall GetStdHandle(unsigned long);
__declspec(dllimport) int __stdcall WriteFile(HANDLE, const void *, unsigned long, unsigned long *, void *);
static const char message[] = "msvc\n";
static const char *volatile anchor = message;
static int (*volatile table[2])(int);
static int twice(int x) { return x * 2; }
static int thrice(int x) { return x * 3; }
int entry(void) {
    unsigned long written;
    table[0] = twice;
    table[1] = thrice;
    WriteFile(GetStdHandle((unsigned long)-11), anchor, sizeof message - 1, &written, 0);
    return table[written & 1](3);
}
