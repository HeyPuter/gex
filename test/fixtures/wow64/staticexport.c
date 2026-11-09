/* A DLL's export that a static library it imports defines, not its own
 * code: msftedit's IID_IRichEditOle and IID_IRichEditOleCallback (its
 * .spec's `extern`, libuuid's data). GetProcAddress finds each, and each
 * is the interface id. Prints "staticexport ok", or what is missing. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static const GUID ole = { 0x00020d00, 0, 0, { 0xc0, 0, 0, 0, 0, 0, 0, 0x46 } };
static const GUID callback = { 0x00020d03, 0, 0, { 0xc0, 0, 0, 0, 0, 0, 0, 0x46 } };

int main(void)
{
    HMODULE m = LoadLibraryA("msftedit.dll");
    const GUID *a, *b;
    int bad = 0;

    if (!m) { printf("staticexport: msftedit.dll not loaded (%lu)\n", GetLastError()); return 1; }
    a = (const GUID *)GetProcAddress(m, "IID_IRichEditOle");
    b = (const GUID *)GetProcAddress(m, "IID_IRichEditOleCallback");
    if (!a) { printf("staticexport: no IID_IRichEditOle\n"); bad = 1; }
    else if (memcmp(a, &ole, sizeof(ole))) { printf("staticexport: IID_IRichEditOle is not the id\n"); bad = 1; }
    if (!b) { printf("staticexport: no IID_IRichEditOleCallback\n"); bad = 1; }
    else if (memcmp(b, &callback, sizeof(callback))) { printf("staticexport: IID_IRichEditOleCallback is not the id\n"); bad = 1; }
    if (bad) return 1;
    printf("staticexport ok\n");
    return 0;
}
