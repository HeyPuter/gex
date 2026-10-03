/* A dynamically linked glibc program: ld.so loads libc, which first checks
 * the CPU against its x86 ISA level ("CPU ISA level is lower than
 * required"), then IFUNC resolvers pick routines by cpuid. */
#include <stdio.h>
#include <string.h>
#include <math.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    char buf[64];
    /* IFUNC-dispatched string routines. */
    memset(buf, 'x', sizeof buf);
    buf[40] = 0;
    double v = sqrt((double)strlen(buf));
    printf("hello from glibc %d %s %.3f\n", (int)sizeof(void *) * 8, argc > 1 ? argv[1] : "-", v);
    return 0;
}
