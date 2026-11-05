/* test/wow64/lazyplace: an image mapped and never run is never placed; one
 * that runs is placed at its first miss. lazyplace_a.dll is mapped with
 * DONT_RESOLVE_DLL_REFERENCES (no DllMain, no import resolution: no code of
 * it runs) and lazyplace_b.dll loaded and called. */
#include <windows.h>
#include <stdio.h>

int main(void)
{
    HMODULE a = LoadLibraryExA("lazyplace_a.dll", NULL, DONT_RESOLVE_DLL_REFERENCES);
    HMODULE b = LoadLibraryA("lazyplace_b.dll");
    unsigned (*mix)(unsigned);
    unsigned r;

    if (!a || !b) { printf("FAIL load a=%p b=%p err=%lu\n", a, b, GetLastError()); return 1; }
    printf("lazyplace_a at %p\n", a);
    printf("lazyplace_b at %p\n", b);
    fflush(stdout);
    mix = (void *)GetProcAddress(b, "lazy_mix");
    if (!mix) { printf("FAIL no lazy_mix\n"); return 1; }
    r = mix(7);
    printf("lazyplace ok %08x\n", r);
    fflush(stdout);
    return 0;
}
