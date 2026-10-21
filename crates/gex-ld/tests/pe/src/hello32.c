#include <windows.h>
static const char message[] = "Hello from x86 PE32!\n";
static const char *volatile anchor = message;
static int __stdcall add(int a, int b) {
    return a + b;
}
static int(__stdcall *volatile callback)(int, int) = add;
int entry(void) {
    DWORD written = 0;
    if (callback(19, 23) != 42) {
        return 1;
    }
    HMODULE kernel = GetModuleHandleA("kernel32.dll");
    DWORD(WINAPI * get_error)(void) = (void *)GetProcAddress(kernel, "GetLastError");
    SetLastError(123);
    if (!get_error || get_error() != 123) {
        return 2;
    }
    if (!WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), anchor, sizeof(message) - 1, &written, 0)) {
        return 3;
    }
    return written != sizeof(message) - 1;
}
