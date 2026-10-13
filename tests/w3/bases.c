/* Load each named DLL and print "base NAME 0xADDR" (tests/w3/bases.sh). */
#include <windows.h>
#include <stdio.h>
int main(int argc, char **argv)
{
    int i;
    for (i = 1; i < argc; i++) {
        HMODULE m = LoadLibraryA(argv[i]);
        printf("base %s %p\n", argv[i], m);
    }
    return 0;
}
