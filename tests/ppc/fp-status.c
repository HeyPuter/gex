/* The FP status gap (docs/ppc.md §8.1): an inexact division raises FE_INEXACT
 * on the hardware (and under qemu-ppc); gex does not compute the flag. */
#include <fenv.h>
#include <stdio.h>
int main(void) {
    volatile double a = 1.0, b = 3.0;
    feclearexcept(FE_ALL_EXCEPT);
    volatile double c = a / b;
    (void)c;
    printf("inexact=%d\n", fetestexcept(FE_INEXACT) != 0);
    return 0;
}
