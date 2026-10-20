/* tests/w3/provdir.sh: which code answers msvcr120's abs and sdemo.dll's
 * sd_add. */
#include <windows.h>
#include <stdio.h>

int main(void) {
    HMODULE m = LoadLibraryA("msvcr120.dll");
    int(__cdecl * p_abs)(int) = m ? (void *)GetProcAddress(m, "abs") : NULL;
    printf("msvcr120 at %p\n", (void *)m);
    printf("abs %d\n", p_abs ? p_abs(-7) : -1);
    HMODULE s = LoadLibraryA("sdemo.dll");
    int(__cdecl * p_add)(int, int) = s ? (void *)GetProcAddress(s, "sd_add") : NULL;
    printf("sd_add %d\n", p_add ? p_add(2, 3) : -1);
    printf("sd_add %d\n", p_add ? p_add(2, 3) : -1);
    printf("provdir done\n");
    fflush(stdout);
    return 0;
}
