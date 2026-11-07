/* An x86-64 exe at Free Pascal's default base, 0x100000000, with no
 * relocations (as FPC and Lazarus link one): it can start only there.
 * On gex's memory64 flavour nothing of the runtime's may hold that
 * address when the process starts. Prints "ok ..." lines and
 * "N failures".
 *
 * gex-nt-arch: x64
 * gex-nt-variant: fpcbase -Wl,--image-base=0x100000000 -Wl,--disable-dynamicbase -Wl,--disable-reloc-section
 */
#include <windows.h>
#include <stdio.h>

static int failures;

static void check(int ok, const char *what)
{
    printf("%s %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) failures++;
}

int main(void)
{
    char *base = (char *)GetModuleHandleA(NULL);
    static int data = 42;
    check(base == (char *)0x100000000, "the exe is at 0x100000000");
    check((char *)&data > base && (char *)&data < base + 0x1000000 && data == 42, "its data is in it");
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
