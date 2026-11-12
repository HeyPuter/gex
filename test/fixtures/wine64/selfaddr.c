/* GetModuleHandleExW(FROM_ADDRESS) on a gexcc-win program's own static
 * data, zeroed data and function: each is the program's module, as an
 * image's would be. */
#include <windows.h>
#include <stdio.h>

static const WCHAR lit[] = L"selfaddr";
static int zeroed;

static int of(const void *p)
{
    HMODULE h = 0;
    return GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                              p, &h) && h == GetModuleHandleW(NULL);
}

int main(int argc, char **argv)
{
    int data = of(lit), bss = of(&zeroed), code = of((const void *)of);
    printf("selfaddr: data %d bss %d code %d\n", data, bss, code);
    if (data && bss && code) printf("selfaddr ok\n");
    return 0;
}
